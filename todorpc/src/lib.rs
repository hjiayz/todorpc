use serde::de::DeserializeOwned;
pub use serde::{Deserialize, Serialize};
use std::result::Result as StdResult;

pub type Result<T, E = Error> = StdResult<T, E>;

#[derive(Clone, Debug)]
pub enum Error {
    DeserializeFaild,
    SerializeFaild,
    ConnectionDroped,
    SubscribeDroped,
    TransportError,
    VerifyFailed,
    ConnectionReset,
    UnknownChannelID,
    MessageTooBig,
    ConnectionFailed,
    Timeout,
}

pub trait Verify {
    fn verify(&self) -> bool {
        true
    }
}

pub trait RPC: Verify + DeserializeOwned + Serialize + Send + Sync + 'static {
    type Return: DeserializeOwned + Serialize + 'static + Send + Sync;
    fn rpc_channel() -> u32;
}

pub trait Call: RPC {}

pub trait Subscribe: RPC {}

#[derive(Debug)]
pub struct Message {
    pub channel_id: u32,
    pub msg: Vec<u8>,
}

#[derive(Debug)]
pub struct Response {
    pub msg: Vec<u8>,
}

#[macro_export]
macro_rules! define {
    ($typ:ident $name:ty[$id:literal]->$ret:ty) => {
        impl RPC for $name {
            type Return = $ret;
            fn rpc_channel() -> u32 {
                $id
            }
        }

        impl $typ for $name {}
    };
}

#[macro_export]
macro_rules! call {
    ($name:ty[$id:literal]->$ret:ty) => {
        define!(Call $name[$id]->$ret);
    };
}

#[macro_export]
macro_rules! subs {
    ($name:ty[$id:literal]->$ret:ty) => {
        define!(Subscribe $name[$id]->$ret);
    };
}
