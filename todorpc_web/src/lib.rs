use futures::channel::{mpsc, oneshot};
use futures::stream::StreamExt;
use js_sys::Uint8Array;
use log::{debug, error, trace};
use std::collections::BTreeMap;
use std::rc::Rc;
use todorpc::{Call, Error as RPCError, Result as RPCResult, Subscribe};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{window, BinaryType, CloseEvent, EventTarget, MessageEvent, WebSocket};

struct Req {
    sender: mpsc::UnboundedSender<RPCResult<Vec<u8>>>,
    is_call: bool,
    channel_id: u32,
    data: Vec<u8>,
}
struct Recv {
    msg_id: u32,
    data: RPCResult<Vec<u8>>,
}

struct Timeout {
    msg_id: u32,
}

enum Op {
    Req(Req),
    Deconnect,
    Recv(Recv),
    Timeout(Timeout),
    Opened,
}

async fn wait_for(timeout: i32) {
    let (tx, rx) = oneshot::channel();
    let ontimeout = Closure::once(Box::new(move || {
        let _ = tx.send(());
    }) as Box<dyn FnOnce()>);
    window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            ontimeout.as_ref().unchecked_ref(),
            timeout,
        )
        .unwrap();
    let _ = rx.await;
}

impl Drop for Ws {
    fn drop(&mut self) {
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
    }
}

struct Ws {
    ws: WebSocket,
    _onopen: Closure<dyn FnMut(EventTarget)>,
    _onclose: Closure<dyn FnMut(CloseEvent)>,
    _onmessage: Closure<dyn FnMut(MessageEvent)>,
}

async fn ws_init(url: &str, tx: mpsc::UnboundedSender<Op>, timeout: i32) -> Ws {
    let ws = loop {
        match WebSocket::new(&url) {
            Ok(ws) => break ws,
            Err(e) => {
                error!("{:?}", e);
                wait_for(timeout).await;
            }
        };
    };
    let ws_ref = ws.clone();
    let tx_ref = tx.clone();
    let onopen = Closure::once(Box::new(move |_: EventTarget| {
        if let Err(_) = tx_ref.unbounded_send(Op::Opened) {
            ws_ref.close().unwrap();
        }
    }) as Box<dyn FnOnce(EventTarget)>);
    ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
    let tx_ref = tx.clone();
    let onmessage = Closure::wrap(Box::new(move |e: MessageEvent| {
        let response = e.data();
        let mut bytes = Uint8Array::new(&response).to_vec();
        match read_msg(&mut bytes) {
            Ok((msg, id)) => {
                if let Err(_) = tx_ref.unbounded_send(Op::Recv(Recv {
                    msg_id: id,
                    data: Ok(msg.to_owned()),
                })) {
                    trace!("wsrpc closed");
                }
            }
            Err(e) => {
                debug!("{:?}", e);
                return;
            }
        };
    }) as Box<dyn FnMut(MessageEvent)>);
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    ws.set_binary_type(BinaryType::Arraybuffer);
    let tx_ref = tx.clone();
    let onclose = Closure::once(Box::new(move |_: CloseEvent| {
        trace!("websocket closed");
        let _ = tx_ref.unbounded_send(Op::Deconnect);
    }) as Box<dyn FnOnce(CloseEvent)>);
    ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
    //todo connect timeout
    Ws {
        ws,
        _onopen: onopen,
        _onclose: onclose,
        _onmessage: onmessage,
    }
}

async fn connect(url: String, timeout: i32) -> mpsc::UnboundedSender<Op> {
    let (tx, mut rx) = mpsc::unbounded();
    let (open_tx, mut open_rx) = mpsc::unbounded();
    let tx_ref = tx.clone();
    spawn_local(async move {
        let mut id: u32 = 0;
        let mut events = BTreeMap::<u32, (mpsc::UnboundedSender<RPCResult<Vec<u8>>>, bool)>::new();
        let mut never_open = true;
        let mut ws = ws_init(&url, tx_ref.clone(), timeout).await;

        while let Some(op) = rx.next().await {
            let open_tx = open_tx.clone();
            let tx_ref = tx_ref.clone();
            match op {
                Op::Deconnect => {
                    let removeds = events;
                    id = 0;
                    events = BTreeMap::new();
                    for removed in removeds.values() {
                        let _ = removed.0.unbounded_send(Err(RPCError::ConnectionReset));
                    }
                    wait_for(timeout).await;
                    ws = ws_init(&url, tx_ref, timeout).await;
                    trace!("deconnect");
                }
                Op::Recv(recv) => {
                    let mut remove = false;
                    if let Some(event) = events.get(&recv.msg_id) {
                        if let Err(_) = event.0.unbounded_send(recv.data) {
                            remove = true;
                        }
                        remove = event.1 || remove;
                    }
                    if remove {
                        let _ = events.remove(&recv.msg_id);
                    }
                    trace!("recv {}", recv.msg_id);
                }
                Op::Timeout(timeout) => {
                    if let Some(event) = events.remove(&timeout.msg_id) {
                        trace!("timeout {}", timeout.msg_id);
                        let _ = event.0.unbounded_send(Err(RPCError::Timeout));
                    }
                }
                Op::Req(req) => {
                    trace!("request {}", id);
                    if id == u32::max_value() {
                        ws.ws.close().unwrap();
                        trace!("message id overflow");
                        let _ = req.sender.unbounded_send(Err(RPCError::ConnectionReset));
                        continue;
                    };
                    id += 1;
                    if req.is_call {
                        spawn_local(async move {
                            wait_for(timeout).await;
                            let _ = tx_ref.unbounded_send(Op::Timeout(Timeout { msg_id: id }));
                        });
                    }
                    let _ = events.insert(id, (req.sender, req.is_call));
                    if let Err(e) = send(req.channel_id, id, req.data, &ws.ws) {
                        debug!("{:?}", e);
                        if let Some(event) = events.remove(&id) {
                            let _ = event.0.unbounded_send(Err(e));
                        }
                    }
                }
                Op::Opened => {
                    if never_open {
                        trace!("opened");
                        let _ = open_tx.unbounded_send(());
                    }
                    never_open = false;
                }
            }
        }
    });
    open_rx.next().await;
    tx
}

#[derive(Clone)]
pub struct WSRpc {
    tx: mpsc::UnboundedSender<Op>,
    timeout: i32,
}

impl WSRpc {
    pub async fn connect(url: &str, timeout: i32) -> WSRpc {
        let tx = connect(url.to_owned(), timeout).await;
        WSRpc { tx, timeout }
    }
    pub async fn call<C: Call>(&self, params: C) -> RPCResult<C::Return> {
        if let Err(res) = params.verify() {
            return Ok(res);
        };
        let (tx, mut rx) = mpsc::unbounded();
        let data = bincode::serialize(&params).map_err(|e| {
            debug!("{}", e);
            RPCError::SerializeFaild
        })?;
        self.tx
            .unbounded_send(Op::Req(Req {
                sender: tx,
                is_call: true,
                channel_id: C::rpc_channel(),
                data,
            }))
            .map_err(|_| RPCError::ConnectionDroped)?;
        let bytes = match rx.next().await {
            Some(r) => r?,
            None => return Err(RPCError::ConnectionReset),
        };
        Ok(bincode::deserialize(&bytes).map_err(|e| {
            debug!("{}", e);
            RPCError::DeserializeFaild
        })?)
    }
    pub fn subscribe<R: Subscribe>(
        &self,
        param: R,
    ) -> RPCResult<mpsc::UnboundedReceiver<RPCResult<R::Return>>> {
        let (res_tx, res_rx) = mpsc::unbounded::<RPCResult<R::Return>>();
        if let Err(res) = param.verify() {
            let _ = res_tx.unbounded_send(Ok(res));
            return Ok(res_rx);
        };
        let this = self.clone();
        let param = Rc::new(param);
        let mut rx = self.subscribe_inner(param.clone().as_ref())?;
        spawn_local(async move {
            loop {
                if let Some(msg) = rx.next().await {
                    match msg {
                        Err(e) => {
                            if let Err(_) = res_tx.unbounded_send(Err(e.clone())) {
                                trace!("subscribe {} stop", R::rpc_channel());
                                break;
                            }
                        }
                        Ok(msg) => {
                            let result = bincode::deserialize(&msg).map_err(|e| {
                                debug!("{}", e);
                                RPCError::DeserializeFaild
                            });
                            if let Err(_) = res_tx.unbounded_send(result) {
                                trace!("subscribe {} stop", R::rpc_channel());
                                break;
                            }
                        }
                    }
                    continue;
                }
                wait_for(this.timeout).await;
                loop {
                    trace!("try subscribe {} again", R::rpc_channel());
                    if let Ok(new_rx) = this.subscribe_inner(param.as_ref()) {
                        rx = new_rx;
                        break;
                    }
                    wait_for(this.timeout).await;
                }
            }
        });
        Ok(res_rx)
    }
    fn subscribe_inner<R: Subscribe>(
        &self,
        param: &R,
    ) -> RPCResult<mpsc::UnboundedReceiver<RPCResult<Vec<u8>>>> {
        let (tx, rx) = mpsc::unbounded::<RPCResult<Vec<u8>>>();
        let data = bincode::serialize(param).map_err(|e| {
            debug!("{}", e);
            RPCError::SerializeFaild
        })?;
        self.tx
            .unbounded_send(Op::Req(Req {
                sender: tx,
                is_call: false,
                channel_id: R::rpc_channel(),
                data,
            }))
            .map_err(|_| RPCError::ConnectionDroped)?;
        Ok(rx)
    }
}

fn send(channel_id: u32, msg_id: u32, data: Vec<u8>, ws: &WebSocket) -> RPCResult<Vec<u8>> {
    if data.len() > u16::max_value() as usize {
        return Err(RPCError::MessageTooBig);
    }
    let len = (data.len() as u16).to_be_bytes();
    let channel = channel_id.to_be_bytes();
    let msg_id = msg_id.to_be_bytes();
    let bytes: Vec<u8> = len
        .iter()
        .chain(channel.iter())
        .chain(msg_id.iter())
        .cloned()
        .chain(data.into_iter())
        .collect();
    ws.send_with_u8_array(&bytes).map_err(|e| {
        debug!("{:?}", e);
        RPCError::TransportError
    })?;
    Ok(bytes)
}

fn read_msg(n: &mut [u8]) -> RPCResult<(&mut [u8], u32)> {
    use std::convert::TryInto;
    if n.len() < 12 {
        return Err(RPCError::TransportError);
    }
    let len = u64::from_be_bytes(n[0..8].try_into().unwrap()) as usize;
    let msg_id = u32::from_be_bytes(n[8..12].try_into().unwrap());

    if n.len() != len + 12 {
        return Err(RPCError::TransportError);
    }
    let msg = &mut n[12..(len + 12)];
    Ok((msg, msg_id))
}
