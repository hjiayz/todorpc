use serde::{de::DeserializeOwned, Serialize};
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
    pub msg: Vec<u8>,
}

#[derive(Debug)]
pub struct Response {
    pub msg: Vec<u8>,
}

#[macro_export]
macro_rules! define {
    ($typ:ident=>$id:expr=>$name:ident($var:ident:$src:ty)->$ret:ty$verify:block) => {
        #[derive(Deserialize, Serialize)]
        pub struct $name(pub $src);

        impl RPC for $name {
            type Return = $ret;
            fn verify(&self) -> bool {
                let $var = self.0;
                $verify
            }
            fn rpc_channel() -> u32 {
                $id
            }
        }

        impl $typ for $name {}
    };
    ($typ:ident=>$id:expr=>$name:ident->$ret:ty$verify:block) => {
        #[derive(Deserialize, Serialize)]
        pub struct $name;

        impl RPC for $name {
            type Return = $ret;
            fn verify(&self) -> bool {
                $verify
            }
            fn rpc_channel() -> u32 {
                $id
            }
        }

        impl $typ for $name {}
    };
}

#[macro_export]
macro_rules! call {
    ($id:expr=>$name:ident($var:ident:$src:ty)->$ret:ty$verify:block) => {
        define!(Call=>$id=>$name($var:$src)->$ret$verify);
    };
    ($id:expr=>$name:ident->$ret:ty$verify:block) => {
        define!(Call=>$id=>$name->$ret$verify);
    };
}

#[macro_export]
macro_rules! subs {
    ($id:expr=>$name:ident($var:ident:$src:ty)->$ret:ty$verify:block) => {
        define!(Subscribe=>$id=>$name($var:$src)->$ret$verify);
    };
    ($id:expr=>$name:ident->$ret:ty$verify:block) => {
        define!(Subscribe=>$id=>$name->$ret$verify);
    };
}
