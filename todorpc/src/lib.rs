use bincode::Error as BincodeError;
use serde::{de::DeserializeOwned, Serialize};
use std::result::Result as StdResult;

pub type Result<T, E = Error> = StdResult<T, E>;

#[derive(Clone, Debug)]
pub enum Error {
    BincodeError,
    ChannelClosed,
    IoError(String),
    VerifyFailed,
    TrySetTokenFaild,
    TryReadTokenFaild,
    NoConnected,
    Other(String),
}

impl From<BincodeError> for Error {
    fn from(_: BincodeError) -> Self {
        Error::BincodeError
    }
}

pub trait RPC: DeserializeOwned + Serialize + Send + Sync + 'static {
    type Return: DeserializeOwned + Serialize + 'static + Send + Sync;
    fn verify(&self) -> bool {
        true
    }
    fn rpc_channel() -> u32;
}

pub trait Call: RPC {}

pub trait Subscribe: RPC {}

#[derive(Debug)]
pub struct Message {
    pub channel_id: u32,
    pub msg_id: u32,
    pub msg: Vec<u8>,
}

#[derive(Debug)]
pub struct Response {
    pub msg_id: u32,
    pub msg: Vec<u8>,
}
