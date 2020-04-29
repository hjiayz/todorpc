use define::*;
use futures::stream::StreamExt;
use js_sys::{Error, Function, Promise};
use std::rc::Rc;
use todorpc_web::Retry;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{future_to_promise, spawn_local};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
}

static mut CONN: Option<Rc<Retry>> = None;
static mut TLSCONN: Option<Rc<Retry>> = None;

async fn connect() -> Result<JsValue, JsValue> {
    let rpc = Retry::new(5000, "ws://localhost:8080/").await;
    unsafe { CONN = Some(rpc) };
    Ok(JsValue::UNDEFINED)
}

async fn tlsconnect() -> Result<JsValue, JsValue> {
    let rpc = Retry::new(5000, "wss://localhost:8082/").await;
    unsafe { TLSCONN = Some(rpc) };
    Ok(JsValue::UNDEFINED)
}

#[wasm_bindgen(start)]
pub fn start_init() {
    use log::Level;
    console_error_panic_hook::set_once();
    console_log::init_with_level(Level::Trace).expect("error initializing log");
}

#[wasm_bindgen]
pub fn connect_ws() -> Promise {
    future_to_promise(connect())
}

#[wasm_bindgen]
pub fn connect_wss() -> Promise {
    future_to_promise(tlsconnect())
}

async fn async_call_foo(val: u32, is_tls: bool) -> Result<JsValue, JsValue> {
    let con = unsafe {
        if is_tls {
            TLSCONN.as_ref().unwrap()
        } else {
            CONN.as_ref().unwrap()
        }
    };
    con.call(&Foo(val))
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
    let con = unsafe {
        if is_tls {
            TLSCONN.as_ref().unwrap()
        } else {
            CONN.as_ref().unwrap()
        }
    };
    spawn_local(async move {
        let mut stream = match con.subscribe(&Bar) {
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
