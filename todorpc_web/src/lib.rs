use futures::channel::{mpsc, oneshot};
use futures::stream::{Stream, StreamExt};
use futures::task::{Context, Poll};
use js_sys::Object;
use js_sys::Uint8Array;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use todorpc::{Call, Error as RPCError, Result as RPCResult, Subscribe, RPC};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{window, BinaryType, CloseEvent, EventTarget, MessageEvent, WebSocket};

pub struct SubscribeStream<T>(
    Pin<Box<dyn Stream<Item = RPCResult<T>>>>,
    u32,
    Weak<RefCell<BTreeMap<u32, mpsc::UnboundedSender<RPCResult<Vec<u8>>>>>>,
);

impl<T> Stream for SubscribeStream<T> {
    type Item = RPCResult<T>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<RPCResult<T>>> {
        self.0.as_mut().poll_next(cx)
    }
}

impl<T> Drop for SubscribeStream<T> {
    fn drop(&mut self) {
        if let Some(ptr) = self.2.upgrade() {
            ptr.borrow_mut().remove(&self.1).unwrap();
        }
    }
}

pub struct WSRpc {
    next_id: RefCell<u32>,
    ws: Rc<WebSocket>,
    on_msgs: Rc<RefCell<BTreeMap<u32, mpsc::UnboundedSender<RPCResult<Vec<u8>>>>>>,
    _onmessage: Option<Closure<dyn FnMut(MessageEvent)>>,
    _onclose: Option<Closure<dyn FnMut(CloseEvent)>>,
    _onopen: Option<Closure<dyn FnMut(EventTarget)>>,
}

impl WSRpc {
    pub async fn connect<F: FnOnce() + 'static>(url: &str, on_close: F) -> RPCResult<WSRpc> {
        let ws = WebSocket::new(url)
            .map_err(|e| RPCError::Other(String::from(Object::from(e).to_string())))?;
        let ws = Rc::new(ws);
        let on_msgs = Rc::new(RefCell::new(BTreeMap::<
            u32,
            mpsc::UnboundedSender<RPCResult<Vec<u8>>>,
        >::new()));
        let (sender, mut receiver) = mpsc::unbounded::<RPCResult<()>>();
        let onopen_sender = sender.clone();
        let onopen = Closure::once(Box::new(move |_: EventTarget| {
            onopen_sender.unbounded_send(Ok(())).unwrap();
        }) as Box<dyn FnOnce(EventTarget)>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        let on_msgs_ref = on_msgs.clone();
        let ws_ref = ws.clone();
        let onmessage = Closure::wrap(Box::new(move |e: MessageEvent| {
            let response = e.data();
            let mut bytes = Uint8Array::new(&response).to_vec();
            match read_msg(&mut bytes) {
                Ok((msg, id)) => {
                    if let Err(_) = on_msgs_ref
                        .borrow()
                        .get(&id)
                        .unwrap()
                        .unbounded_send(Ok(msg.to_owned()))
                    {
                        ws_ref.close().unwrap();
                    }
                }
                Err(_e) => {
                    //info?
                    ws_ref.close().unwrap();
                    return;
                }
            };
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        ws.set_binary_type(BinaryType::Arraybuffer);

        let close_sender = sender.clone();
        let on_close_before_open = Closure::once(Box::new(move |_: CloseEvent| {
            close_sender
                .unbounded_send(Err(RPCError::NoConnected))
                .unwrap();
        }) as Box<dyn FnOnce(CloseEvent)>);

        ws.set_onclose(Some(on_close_before_open.as_ref().unchecked_ref()));
        receiver.next().await.unwrap()?;

        let on_msgs_ref2 = on_msgs.clone();
        let onclose = Closure::once(Box::new(move |_: CloseEvent| {
            for sender in on_msgs_ref2.borrow().values() {
                let _ = sender.unbounded_send(Err(RPCError::ChannelClosed));
            }
            on_close();
        }) as Box<dyn FnOnce(CloseEvent)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        drop(on_close_before_open);
        Ok(WSRpc {
            next_id: RefCell::new(0),
            ws,
            on_msgs,
            _onmessage: Some(onmessage),
            _onclose: Some(onclose),
            _onopen: Some(onopen),
        })
    }
    pub async fn call<C: Call>(&self, params: &C) -> RPCResult<C::Return> {
        let id = self.get_next_id()?;
        self.send(id, params)?;
        let (tx, mut rx) = mpsc::unbounded();
        let _ = self.on_msgs.borrow_mut().insert(id, tx);
        let bytes = match rx.next().await {
            Some(r) => r?,
            None => return Err(RPCError::ChannelClosed),
        };
        self.on_msgs.borrow_mut().remove(&id).unwrap();
        Ok(bincode::deserialize(&bytes)?)
    }
    pub fn subscribe<S: Subscribe>(&self, params: &S) -> RPCResult<SubscribeStream<S::Return>> {
        let id = self.get_next_id()?;
        self.send(id, params)?;
        let (tx, rx) = mpsc::unbounded();
        let _ = self.on_msgs.borrow_mut().insert(id, tx);
        let result = rx.map(|result| result.and_then(|src| Ok(bincode::deserialize(&src)?)));
        Ok(SubscribeStream(
            Box::pin(result),
            id,
            Rc::downgrade(&self.on_msgs),
        ))
    }
    fn get_next_id(&self) -> RPCResult<u32> {
        let mut id = self.next_id.borrow_mut();
        let val = *id;
        if val == u32::max_value() {
            self.ws.close().unwrap();
            return Err(RPCError::IoError("msg id overflow".to_owned()));
        };
        *id += 1;
        Ok(val)
    }
    fn send<R: RPC>(&self, msg_id: u32, params: &R) -> RPCResult<()> {
        let data = bincode::serialize(&params)?;
        let len = (data.len() as u16).to_be_bytes();
        let channel = R::rpc_channel().to_be_bytes();
        let msg_id = msg_id.to_be_bytes();
        let bytes: Vec<u8> = len
            .iter()
            .chain(channel.iter())
            .chain(msg_id.iter())
            .cloned()
            .chain(data.into_iter())
            .collect();
        self.ws
            .send_with_u8_array(&bytes)
            .map_err(|_| RPCError::IoError("send msg faild".to_string()))?;
        Ok(())
    }
}

impl Drop for WSRpc {
    fn drop(&mut self) {
        self.ws.close().unwrap();
        self.ws.set_onmessage(None);
        self.ws.set_onerror(None);
        self.ws.set_onopen(None);
        self.ws.set_onclose(None);
    }
}

pub fn read_msg(n: &mut [u8]) -> RPCResult<(&mut [u8], u32)> {
    use std::convert::TryInto;
    if n.len() < 12 {
        return Err(RPCError::IoError("bad message pack".to_owned()));
    }
    let len = u64::from_be_bytes(n[0..8].try_into().unwrap()) as usize;
    let msg_id = u32::from_be_bytes(n[8..12].try_into().unwrap());

    if n.len() != len + 12 {
        return Err(RPCError::IoError("bad message pack".to_owned()));
    }
    let msg = &mut n[12..(len + 12)];
    Ok((msg, msg_id))
}

pub struct Retry {
    conn: Rc<RefCell<Option<WSRpc>>>,
    timeout: i32,
    url: String,
}

impl Retry {
    pub async fn new<S: Into<String>>(timeout: i32, url: S) -> Rc<Retry> {
        let url = url.into();
        let conn: Rc<RefCell<Option<WSRpc>>> = Rc::new(RefCell::new(None));
        let result = Rc::new(Retry { conn, url, timeout });
        let result2 = result.clone();
        result2.connect().await;
        result
    }
    async fn connect(self: Rc<Self>) {
        loop {
            let weak = Rc::downgrade(&self);
            let onclose = move || {
                if let Some(ptr) = weak.upgrade() {
                    *ptr.conn.borrow_mut() = None;
                    spawn_local(ptr.connect())
                }
            };

            if let Ok(conn) = WSRpc::connect(&self.url, onclose).await {
                *self.conn.borrow_mut() = Some(conn);
                break;
            }
            let (tx, rx) = oneshot::channel();
            let ontimeout = Closure::once(Box::new(move || {
                let _ = tx.send(());
            }) as Box<dyn FnOnce()>);
            window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    ontimeout.as_ref().unchecked_ref(),
                    self.timeout,
                )
                .unwrap();
            let _ = rx.await;
        }
    }
    pub async fn call<C: Call>(&self, params: &C) -> RPCResult<C::Return> {
        match self.conn.borrow().as_ref() {
            None => Err(RPCError::NoConnected),
            Some(conn) => conn.call(params).await,
        }
    }
    pub fn subscribe<S: Subscribe>(&self, params: &S) -> RPCResult<SubscribeStream<S::Return>> {
        match self.conn.borrow().as_ref() {
            None => Err(RPCError::NoConnected),
            Some(conn) => conn.subscribe(params),
        }
    }
}
