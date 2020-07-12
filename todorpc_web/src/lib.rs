use futures::channel::{mpsc, oneshot};
use futures::stream::{Stream, StreamExt};
use js_sys::Uint8Array;
use log::{debug, error, trace};
use std::collections::BTreeMap;
use std::rc::Rc;
use todorpc::{Call, Error as RPCError, Result as RPCResult, Subscribe, Upload};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{window, BinaryType, CloseEvent, EventTarget, MessageEvent, WebSocket};

struct Recv {
    msg_id: u32,
    data: RPCResult<Vec<u8>>,
}

struct Timeout {
    timeout_id: u128,
}

struct Req {
    sender: mpsc::UnboundedSender<RPCResult<Vec<u8>>>,
    channel_id: u32,
    data: Vec<u8>,
}

struct ReqNoCall {
    sender: mpsc::UnboundedSender<RPCResult<Vec<u8>>>,
    msg_id_sender: oneshot::Sender<u32>,
    channel_id: u32,
    data: Vec<u8>,
}

struct Uploading {
    upload_ok_tx: oneshot::Sender<()>,
    data: Vec<u8>,
    msg_id: u32,
}

enum Op {
    Req(Req),
    Deconnect,
    Recv(Recv),
    Timeout(Timeout),
    Opened,
    Uploading(Uploading),
    ReqNoCall(ReqNoCall),
    EndNoCall(u32),
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

fn timeout_checker(id: u128, tx: mpsc::UnboundedSender<Op>, timeout: i32) -> u128 {
    let new_id = id + 1;
    spawn_local(async move {
        wait_for(timeout).await;
        debug!("timed out");
        let _ = tx.unbounded_send(Op::Timeout(Timeout { timeout_id: new_id }));
    });
    new_id
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
        if tx_ref.unbounded_send(Op::Opened).is_err() {
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
                let op = Op::Recv(Recv {
                    msg_id: id,
                    data: Ok(msg.to_owned()),
                });
                if tx_ref.unbounded_send(op).is_err() {
                    trace!("wsrpc closed");
                }
            }
            Err(e) => {
                debug!("{:?}", e);
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

async fn ws_run(
    url: &str,
    tx: mpsc::UnboundedSender<Op>,
    timeout: i32,
) -> mpsc::UnboundedSender<(Vec<u8>, Option<oneshot::Sender<()>>)> {
    let (req_tx, mut req_rx) = mpsc::unbounded::<(Vec<u8>, Option<oneshot::Sender<()>>)>();
    let ws = ws_init(url, tx, timeout).await;
    spawn_local(async move {
        while let Some((bytes, on_ok)) = req_rx.next().await {
            loop {
                let ba = ws.ws.buffered_amount();
                debug!("buffered amount : {}", ba);
                if ba < 1 << 18 {
                    break;
                }
                wait_for(50).await;
            }
            let _ = ws.ws.send_with_u8_array(&bytes).map_err(|e| {
                debug!("{:?}", e);
                RPCError::TransportError
            });
            if let Some(sender) = on_ok {
                let _ = sender.send(());
            }
        }
        debug!("req tx droped");
    });
    req_tx
}

async fn connect(url: String, timeout: i32) -> mpsc::UnboundedSender<Op> {
    let (tx, mut rx) = mpsc::unbounded();
    let (open_tx, mut open_rx) = mpsc::unbounded();
    let tx_ref = tx.clone();
    spawn_local(async move {
        let mut id: u32 = 0;
        let mut events = BTreeMap::<u32, (mpsc::UnboundedSender<RPCResult<Vec<u8>>>, bool)>::new();
        let mut never_open = true;
        let mut timeout_id = 0u128;
        let mut req_tx = ws_run(&url, tx_ref.clone(), timeout).await;
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
                    req_tx = ws_run(&url, tx_ref.clone(), timeout).await;
                    trace!("deconnect");
                }
                Op::Recv(recv) => {
                    let mut remove = false;
                    if let Some(event) = events.get(&recv.msg_id) {
                        if event.0.unbounded_send(recv.data).is_err() {
                            remove = true;
                        }
                        remove = event.1 || remove;
                    }
                    if remove {
                        let _ = events.remove(&recv.msg_id);
                    }
                    trace!("recv {}", recv.msg_id);
                    timeout_id = timeout_checker(timeout_id, tx_ref.clone(), timeout);
                }
                Op::Timeout(timeout) => {
                    if timeout_id == timeout.timeout_id {
                        let _ = tx_ref.unbounded_send(Op::Deconnect);
                    }
                }
                Op::Req(req) => {
                    trace!("request {}", id);
                    if id == u32::max_value() {
                        req_tx = ws_run(&url, tx_ref.clone(), timeout).await;
                        trace!("message id overflow");
                        let _ = tx_ref.unbounded_send(Op::Deconnect);
                        continue;
                    };
                    id += 1;
                    let _ = events.insert(id, (req.sender, true));
                    if let Err(e) = send(req.channel_id, id, req.data, req_tx.clone(), None) {
                        debug!("{:?}", e);
                        if let Some(event) = events.remove(&id) {
                            let _ = event.0.unbounded_send(Err(e));
                        }
                    }
                    timeout_id = timeout_checker(timeout_id, tx_ref.clone(), timeout);
                }
                Op::Opened => {
                    if never_open {
                        trace!("opened");
                        let _ = open_tx.unbounded_send(());
                    }
                    never_open = false;
                    timeout_id = timeout_checker(timeout_id, tx_ref.clone(), timeout);
                }
                Op::ReqNoCall(req) => {
                    trace!("request {}", id);
                    if id == u32::max_value() {
                        req_tx = ws_run(&url, tx_ref.clone(), timeout).await;
                        trace!("message id overflow");
                        let _ = tx_ref.unbounded_send(Op::Deconnect);
                        continue;
                    };
                    id += 1;
                    if req.msg_id_sender.send(id).is_err() {
                        trace!("requester droped");
                        continue;
                    }
                    match send(req.channel_id, id, req.data, req_tx.clone(), None) {
                        Ok(_data) => {
                            let _ = events.insert(id, (req.sender, false));
                        }
                        Err(e) => {
                            if req.sender.unbounded_send(Err(e)).is_err() {
                                trace!("requester droped");
                            }
                            continue;
                        }
                    }
                    timeout_id = timeout_checker(timeout_id, tx_ref.clone(), timeout);
                }
                Op::Uploading(uploading) => {
                    match send(
                        u32::max_value(),
                        uploading.msg_id,
                        uploading.data,
                        req_tx.clone(),
                        Some(uploading.upload_ok_tx),
                    ) {
                        Ok(_data) => {}
                        Err(e) => {
                            debug!("{:?}", e);
                            continue;
                        }
                    }
                    timeout_id = timeout_checker(timeout_id, tx_ref.clone(), timeout);
                }
                Op::EndNoCall(msg_id) => {
                    if events.remove(&msg_id).is_none() {
                        debug!("no message id : {}", msg_id);
                    }
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

type SubscribeInnerResult = RPCResult<(
    mpsc::UnboundedReceiver<RPCResult<Vec<u8>>>,
    oneshot::Receiver<u32>,
)>;

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
        let (mut rx, mut msg_id_rx) = self.subscribe_inner(param.as_ref())?;
        spawn_local(async move {
            loop {
                if let Some(msg) = rx.next().await {
                    match msg {
                        Err(e) => {
                            if res_tx.unbounded_send(Err(e.clone())).is_err() {
                                trace!("subscribe {} stop", R::rpc_channel());
                                if let Ok(msg_id) = msg_id_rx.await {
                                    let _ = this.tx.unbounded_send(Op::EndNoCall(msg_id));
                                }
                                break;
                            }
                        }
                        Ok(msg) => {
                            let result = bincode::deserialize(&msg).map_err(|e| {
                                debug!("{}", e);
                                RPCError::DeserializeFaild
                            });
                            if res_tx.unbounded_send(result).is_err() {
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
                    if let Ok((new_rx, new_msg_id_rx)) = this.subscribe_inner(param.as_ref()) {
                        rx = new_rx;
                        msg_id_rx = new_msg_id_rx;
                        break;
                    }
                    wait_for(this.timeout).await;
                }
            }
        });
        Ok(res_rx)
    }

    fn subscribe_inner<R: Subscribe>(&self, param: &R) -> SubscribeInnerResult {
        let (tx, rx) = mpsc::unbounded::<RPCResult<Vec<u8>>>();
        let data = bincode::serialize(param).map_err(|e| {
            debug!("{}", e);
            RPCError::SerializeFaild
        })?;
        let (msg_id_sender, msg_id_rx) = oneshot::channel();
        self.tx
            .unbounded_send(Op::ReqNoCall(ReqNoCall {
                sender: tx,
                msg_id_sender,
                channel_id: R::rpc_channel(),
                data,
            }))
            .map_err(|_| RPCError::ConnectionDroped)?;
        Ok((rx, msg_id_rx))
    }
    pub async fn upload<R: Upload>(
        &self,
        param: R,
        stream: impl Unpin + Stream<Item = R::UploadStream>,
    ) -> RPCResult<R::Return> {
        if let Err(res) = param.verify() {
            return Ok(res);
        }
        let (tx, rx) = mpsc::unbounded::<RPCResult<Vec<u8>>>();
        let data = bincode::serialize(&param).map_err(|e| {
            debug!("{}", e);
            RPCError::SerializeFaild
        })?;
        let (msg_id_sender, msg_id_rx) = oneshot::channel();
        self.tx
            .unbounded_send(Op::ReqNoCall(ReqNoCall {
                sender: tx,
                msg_id_sender,
                channel_id: R::rpc_channel(),
                data,
            }))
            .map_err(|_| RPCError::ConnectionDroped)?;
        let msg_id = msg_id_rx.await.map_err(|e| {
            debug!("{:?}", e);
            RPCError::ConnectionReset
        })?;
        let stream = stream.map(|item| {
            bincode::serialize(&item).map_err(|e| {
                debug!("{}", e);
                RPCError::SerializeFaild
            })
        });
        let result = self.upload_inner(msg_id, stream, rx).await?;
        bincode::deserialize(&result).map_err(|e| {
            debug!("{}", e);
            RPCError::DeserializeFaild
        })
    }
    async fn upload_inner(
        &self,
        msg_id: u32,
        mut stream: impl Unpin + Stream<Item = RPCResult<Vec<u8>>>,
        mut sender_rx: mpsc::UnboundedReceiver<RPCResult<Vec<u8>>>,
    ) -> RPCResult<Vec<u8>> {
        while let Some(data) = stream.next().await {
            match sender_rx.try_next() {
                Ok(Some(result)) => {
                    return result;
                }
                Ok(None) => return Err(RPCError::ConnectionReset),
                Err(_) => (),
            };
            let data = data?;
            let (upload_ok_tx, upload_ok_rx) = oneshot::channel();
            self.tx
                .unbounded_send(Op::Uploading(Uploading {
                    upload_ok_tx,
                    data,
                    msg_id,
                }))
                .map_err(|_| RPCError::ConnectionReset)?;
            upload_ok_rx.await.map_err(|_| RPCError::ConnectionReset)?;
        }
        let (upload_ok_tx, upload_ok_rx) = oneshot::channel();
        self.tx
            .unbounded_send(Op::Uploading(Uploading {
                upload_ok_tx,
                data: vec![],
                msg_id,
            }))
            .map_err(|_| RPCError::ConnectionReset)?;
        upload_ok_rx.await.map_err(|_| RPCError::ConnectionReset)?;
        sender_rx
            .next()
            .await
            .ok_or_else(|| RPCError::ConnectionReset)?
    }
}

fn send(
    channel_id: u32,
    msg_id: u32,
    data: Vec<u8>,
    req_tx: mpsc::UnboundedSender<(Vec<u8>, Option<oneshot::Sender<()>>)>,
    on_ok: Option<oneshot::Sender<()>>,
) -> RPCResult<()> {
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
    req_tx
        .unbounded_send((bytes, on_ok))
        .map_err(|_| RPCError::ConnectionReset)?;
    Ok(())
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
