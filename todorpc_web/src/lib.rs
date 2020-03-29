use futures::channel::oneshot;
use js_sys::Object;
use js_sys::Uint8Array;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::rc::Rc;
use todorpc::{Error as RPCError, Result as RPCResult};
pub use todorpc_client_core::{async_call, call, subscribe};
use todorpc_client_core::{read_msg, Connect, ConnectNoSend};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{console, BinaryType, CloseEvent, ErrorEvent, EventTarget, MessageEvent, WebSocket};

struct Inner {
    next_id: u32,
    ws: WebSocket,
    on_msgs: BTreeMap<u32, Box<dyn Fn(RPCResult<Vec<u8>>) -> bool>>,
    onmessage_callback:Option<Closure<dyn FnMut(MessageEvent)>>,
    onerror_callback:Option<Closure<dyn FnMut(ErrorEvent)>>,
    onclose_callback:Option<Closure<dyn FnMut(CloseEvent)>>,
    onopen_callback:Option<Closure<dyn FnMut(EventTarget)>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let onclose_callback = Closure::wrap(Box::new(move |_: CloseEvent| {
        }) as Box<dyn FnMut(CloseEvent)>);
        self.ws.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        onclose_callback.forget();
        self.ws.close().unwrap();
    }
}

#[derive(Clone)]
pub struct WSRpc {
    inner: Rc<RefCell<Inner>>,
}

impl WSRpc {
    pub async fn connect(url: &str) -> RPCResult<WSRpc> {
        let inner = Rc::new(RefCell::new(Inner {
            next_id: 0,
            ws: WebSocket::new(url)
                .map_err(|e| RPCError::Other(String::from(Object::from(e).to_string())))?,
            on_msgs: BTreeMap::new(),
            onmessage_callback:None,
            onerror_callback:None,
            onclose_callback:None,
            onopen_callback:None,
        }));
        let inner2 = Rc::downgrade(&inner);
        let onmessage_callback = Closure::wrap(Box::new(move |e: MessageEvent| {
            let inner3 = inner2.clone();
            let response = e.data();
            let mut bytes = Cursor::new(Uint8Array::new(&response).to_vec());
            let msg = match read_msg(&mut bytes) {
                Ok(msg) => msg,
                Err(e) => {
                    console::log_1(&JsValue::from(&format!("{:?}", e)));
                    inner3.upgrade().unwrap().borrow().ws.close().unwrap();
                    return;
                }
            };
            let msg_id = msg.msg_id;
            let sinner3 = inner3.upgrade().unwrap();
            let borrowed = sinner3.borrow();
            let f = borrowed.on_msgs.get(&msg_id).unwrap();
            if f(Ok(msg.msg)) {
                drop(borrowed);
                let mut borrowed = sinner3.borrow_mut();
                let _ = borrowed.on_msgs.remove(&msg_id).unwrap();
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        let mut borrowed = inner.borrow_mut();
        borrowed.ws.set_binary_type(BinaryType::Arraybuffer);
        borrowed
            .ws
            .set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
        //onmessage_callback.forget();
        let inner5 = Rc::downgrade(&inner);
        let onerror_callback = Closure::wrap(Box::new(move |_: ErrorEvent| {
            let _ = inner5.upgrade().unwrap().borrow().ws.close();
        }) as Box<dyn FnMut(ErrorEvent)>);
        borrowed
            .ws
            .set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
        //onerror_callback.forget();

        let inner6 = Rc::downgrade(&inner);
        let onclose_callback = Closure::wrap(Box::new(move |_: CloseEvent| {
            let sinner6 = inner6.upgrade().unwrap();
            let mut borrowed = sinner6.borrow_mut();
            for f in borrowed.on_msgs.values_mut() {
                f(Err(RPCError::ChannelClosed));
            }
            borrowed.on_msgs.clear();
        }) as Box<dyn FnMut(CloseEvent)>);
        borrowed
            .ws
            .set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));
        //onclose_callback.forget();
        let (sender, receiver) = oneshot::channel::<RPCResult<WSRpc>>();
        let inner7 = inner.clone();
        let onopen_callback = Closure::once(Box::new(move |_: EventTarget| {
            let rpc = WSRpc { inner: inner7 };
            sender.send(Ok(rpc)).ok().unwrap();
        }) as Box<dyn FnOnce(EventTarget)>);
        borrowed
            .ws
            .set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        //onopen_callback.forget();
        borrowed.onmessage_callback=Some(onmessage_callback);
        borrowed.onerror_callback=Some(onerror_callback);
        borrowed.onclose_callback=Some(onclose_callback);
        borrowed.onopen_callback=Some(onopen_callback);
        drop(borrowed);
        drop(inner);
        receiver.await.unwrap()
    }
    pub fn count(&self)->usize{
        Rc::strong_count(&self.inner)
    }
}

impl Connect<Box<dyn Fn(RPCResult<Vec<u8>>) -> bool + 'static>> for WSRpc {
    fn is_connected(&self) -> bool {
        self.inner.borrow().ws.ready_state() == 1
    }
    fn send<F: FnOnce(&Self, RPCResult<()>) + 'static + Send>(&self, bytes: &[u8], cb: F) {
        let result = self
            .inner
            .borrow()
            .ws
            .send_with_u8_array(bytes)
            .map_err(|_| RPCError::IoError("send msg faild".to_string()));
        cb(self, result);
    }
    fn on_msg<F2: FnOnce(&Self, u32) + 'static + Send>(
        &self,
        f: Box<dyn Fn(RPCResult<Vec<u8>>) -> bool + 'static>,
        cb: F2,
    ) {
        let mut borrowed = self.inner.borrow_mut();
        match borrowed.next_id.checked_add(1) {
            Some(next_id) => borrowed.next_id = next_id,
            None => {
                borrowed.ws.close().unwrap();
                cb(self, 0);
                return;
            }
        };
        let id = borrowed.next_id;
        let on_msg = Box::new(f);
        borrowed.on_msgs.insert(id, on_msg);
        let next_id = borrowed.next_id;
        drop(borrowed);
        cb(self, next_id)
    }
    fn close_msg_handle<
        F: FnOnce(Option<Box<dyn Fn(RPCResult<Vec<u8>>) -> bool>>) + Send + 'static,
    >(
        &self,
        msg_id: u32,
        f: F,
    ) {
        let mut borrowed = self.inner.borrow_mut();
        f(borrowed.on_msgs.remove(&msg_id))
    }
}

impl ConnectNoSend for WSRpc {}