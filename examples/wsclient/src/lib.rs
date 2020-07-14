use define::*;
use futures::channel::{mpsc, oneshot};
use futures::stream::StreamExt;
use js_sys::{AsyncIterator, Error, Function, Object, Promise};
use lazy_static::lazy_static;
use log::debug;
use todorpc_web::WSRpc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{future_to_promise, spawn_local, JsFuture};

fn connect(uri: &'static str) -> mpsc::UnboundedSender<oneshot::Sender<WSRpc>> {
    let (tx, mut rx) = mpsc::unbounded::<oneshot::Sender<WSRpc>>();
    spawn_local(async move {
        let rpc = WSRpc::connect(uri, 5000).await;
        debug!("connect {} finished", uri);
        while let Some(ws_tx) = rx.next().await {
            let _ = ws_tx.send(rpc.clone());
        }
    });
    tx
}

lazy_static! {
    static ref CONN: mpsc::UnboundedSender<oneshot::Sender<WSRpc>> =
        connect("ws://localhost:8080/");
    static ref TLSCONN: mpsc::UnboundedSender<oneshot::Sender<WSRpc>> =
        connect("wss://localhost:8082/");
}

async fn get_connect(conn: &mpsc::UnboundedSender<oneshot::Sender<WSRpc>>) -> WSRpc {
    let (tx, rx) = oneshot::channel();
    let _ = conn.unbounded_send(tx);
    rx.await.unwrap()
}

async fn get_conn(is_tls: bool) -> WSRpc {
    if is_tls {
        get_connect(&TLSCONN).await
    } else {
        get_connect(&CONN).await
    }
}

#[wasm_bindgen(start)]
pub fn start_init() {
    use log::Level;
    console_error_panic_hook::set_once();
    console_log::init_with_level(Level::Trace).expect("error initializing log");
}

async fn async_call_foo(val: u32, is_tls: bool) -> Result<JsValue, JsValue> {
    let con = get_conn(is_tls).await;
    con.call(Foo(val))
        .await
        .map(JsValue::from)
        .map_err(|e| JsValue::from(format!("{:?}", e)))
}

#[wasm_bindgen]
pub fn call_foo(val: u32, is_tls: bool) -> Promise {
    future_to_promise(async_call_foo(val, is_tls))
}

#[wasm_bindgen]
pub fn subscribe_bar(cb: Function, is_tls: bool) {
    spawn_local(async move {
        let con = get_conn(is_tls).await;
        let mut stream = match con.subscribe(Bar) {
            Ok(stream) => stream,
            Err(e) => {
                cb.call1(
                    &JsValue::UNDEFINED,
                    &JsValue::from(Error::new(&format!("{:?}", e))),
                )
                .unwrap();
                return;
            }
        };
        while let Some(res) = stream.next().await {
            match res {
                Ok(val) => cb.call1(
                    &JsValue::UNDEFINED,
                    &JsValue::from(&format!("{},{}", val.0, val.1)),
                ),
                Err(e) => cb.call1(
                    &JsValue::UNDEFINED,
                    &JsValue::from(Error::new(&format!("{:?}", e))),
                ),
            }
            .unwrap();
        }
    });
}

async fn async_upload_sample(stream: AsyncIterator, is_tls: bool) -> Result<JsValue, JsValue> {
    let con = get_conn(is_tls).await;
    let (tx, rx) = mpsc::unbounded();
    spawn_local(async move {
        while let Ok(promise) = stream.next() {
            if let Ok(s) = JsFuture::from(promise).await {
                let obj = Object::from(s);
                let values = Object::values(&obj);
                let done = values.get(1).as_bool().unwrap();
                if done {
                    break;
                }
                let value = values.get(0).as_string().unwrap();
                let _ = tx.unbounded_send(value);
            }
        }
    });
    con.upload(UploadSample, rx)
        .await
        .map(|_| JsValue::UNDEFINED)
        .map_err(|e| JsValue::from(format!("{:?}", e)))
}

#[wasm_bindgen]
pub fn upload_sample(stream: AsyncIterator, is_tls: bool) -> Promise {
    future_to_promise(async_upload_sample(stream, is_tls))
}
