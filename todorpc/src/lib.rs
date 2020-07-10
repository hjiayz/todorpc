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
    ConnectionReset,
    UnknownChannelID,
    MessageTooBig,
    ConnectionFailed,
    Timeout,
}

pub trait Verify: RPC {
    fn verify(&self) -> Result<(), Self::Return> {
        Ok(())
    }
    fn verify_result(self) -> Result<Self, Self::Return> {
        self.verify().map(|_| self)
    }
}

pub trait RPC: DeserializeOwned + Serialize + Send + Sync + 'static {
    type Return: DeserializeOwned + Serialize + 'static + Send + Sync;
    fn rpc_channel() -> u32;
}

pub trait Call: RPC + Verify {}

pub trait Subscribe: RPC + Verify {}

pub trait Upload: RPC + Verify {
    type UploadStream: DeserializeOwned + Serialize + 'static + Send + Sync;
}

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
