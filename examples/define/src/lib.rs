use serde::*;
use todorpc::*;

#[derive(Deserialize, Serialize)]
pub struct Foo(pub u32);

impl RPC for Foo {
    type Return = u32;
    fn verify(&self) -> bool {
        true
    }
    fn rpc_channel() -> u32 {
        1
    }
}

impl Call for Foo {}

#[derive(Deserialize, Serialize)]
pub struct Bar;

impl RPC for Bar {
    type Return = (String, u32);
    fn verify(&self) -> bool {
        true
    }
    fn rpc_channel() -> u32 {
        2
    }
}
impl Subscribe for Bar {}
