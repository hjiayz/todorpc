use define::*;
use js_sys::{Error, Function, Promise};
use todorpc_web::{async_call, call, subscribe, WSRpc};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
}

static mut CONN: Option<WSRpc> = None;

async fn connect() -> Result<JsValue, JsValue> {
    let rpc = WSRpc::connect("ws://localhost:8080/")
        .await
        .map_err(|e| JsValue::from(format!("{:?}", e)))?;
    unsafe { CONN = Some(rpc) };
    Ok(JsValue::UNDEFINED)
}

#[wasm_bindgen(start)]
pub fn start_init() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub fn connect_ws() -> Promise {
    future_to_promise(connect())
}

async fn async_call_foo(val: u32) -> Result<JsValue, JsValue> {
    unsafe {
        async_call(Foo(val), CONN.as_ref().unwrap())
            .await
            .map(JsValue::from)
            .map_err(|e| JsValue::from(format!("{:?}", e)))
    }
}

#[wasm_bindgen]
pub fn call_foo(val: u32) -> Promise {
    future_to_promise(async_call_foo(val))
}

#[wasm_bindgen]
pub fn call_foo2(val: u32) -> Promise {
    let mut cb = |resolve: Function, reject: Function| unsafe {
        call(Foo(val), CONN.as_ref().unwrap(), move |res| {
            match res {
                Ok(val) => {
                    resolve
                        .call1(&JsValue::UNDEFINED, &JsValue::from(val))
                        .unwrap();
                }
                Err(e) => {
                    reject
                        .call1(&JsValue::UNDEFINED, &JsValue::from(format!("{:?}", e)))
                        .unwrap();
                }
            };
        });
    };
    Promise::new(&mut cb)
}

#[wasm_bindgen]
pub fn subscribe_bar(cb: Function) {
    unsafe {
        subscribe(Bar, CONN.as_ref().unwrap(), move |res| {
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
            false
        });
    }
}
