use bincode;
use futures::channel::oneshot;
use std::cell::RefCell;
use std::io::{Error as IoError, Read, Write};
use std::mem::transmute;
use todorpc::{Call, Error as RPCError, Response, Result as RPCResult, Subscribe};

pub trait Connect<S> {
    fn is_connected(&self) -> bool;
    fn send<F: FnOnce(&Self, RPCResult<()>) + 'static + Send>(&self, bytes: &[u8], cb: F);
    fn on_msg<F2: FnOnce(&Self, u32) + 'static + Send>(&self, f: S, cb: F2);
    fn close_msg_handle<F: FnOnce(Option<S>) + 'static + Send>(&self, msg_id: u32, f: F);
}

pub trait ConnectNoSend: Connect<Box<dyn Fn(RPCResult<Vec<u8>>) -> bool + 'static>> {}
pub trait ConnectSend: Connect<Box<dyn Fn(RPCResult<Vec<u8>>) -> bool + 'static + Send>> {}

fn send_msg<S: Fn(RPCResult<Vec<u8>>) -> bool + 'static, C: Connect<S>>(
    conn: &C,
    channel_id: u32,
    msg_id: u32,
    msg: &[u8],
) {
    let msg_id_buf = msg_id.to_be_bytes();
    let len = msg.len();
    let len_buf = (len as u64).to_be_bytes();
    let channel_id_buf = channel_id.to_be_bytes();
    let h: [u8; 16] = unsafe { transmute((len_buf, channel_id_buf, msg_id_buf)) };
    let mut buf = Vec::with_capacity(len + 16);
    if buf.write_all(&h).is_err() {
        conn.close_msg_handle(msg_id, |f| {
            if let Some(on_result) = f {
                on_result(Err(RPCError::IoError("write header faild".to_string())));
            }
        })
    }
    if buf.write_all(&msg).is_err() {
        conn.close_msg_handle(msg_id, |f| {
            if let Some(on_result) = f {
                on_result(Err(RPCError::IoError("write msg faild".to_string())));
            }
        })
    }
    conn.send(&buf, move |conn, res| {
        if let Err(e) = res {
            conn.close_msg_handle(msg_id, |f| {
                if let Some(on_result) = f {
                    on_result(Err(e));
                }
            })
        }
    })
}

pub fn call<C, R, F>(param: R, conn: &C, on_result: F)
where
    R: Call,
    C: ConnectNoSend + 'static,
    F: Fn(RPCResult<R::Return>) + 'static,
{
    if !conn.is_connected() {
        return on_result(Err(RPCError::NoConnected));
    }
    if !param.verify() {
        return on_result(Err(RPCError::VerifyFailed));
    }
    let msg = match bincode::serialize(&param) {
        Ok(msg) => msg,
        Err(_) => return on_result(Err(RPCError::BincodeError)),
    };
    let channel_id = R::rpc_channel();
    let on_msg = move |res: RPCResult<Vec<u8>>| {
        let param = res.and_then(|bytes| bincode::deserialize(&bytes).map_err(RPCError::from));
        on_result(param);
        true
    };
    let cb = move |conn: &C, msg_id: u32| send_msg(conn, channel_id, msg_id, &msg);
    conn.on_msg(Box::new(on_msg), cb);
}

pub fn scall<C, R, F>(param: R, conn: &C, on_result: F)
where
    R: Call,
    C: ConnectSend + 'static,
    F: Fn(RPCResult<R::Return>) + 'static + Send,
{
    if !conn.is_connected() {
        return on_result(Err(RPCError::NoConnected));
    }
    if !param.verify() {
        return on_result(Err(RPCError::VerifyFailed));
    }
    let msg = match bincode::serialize(&param) {
        Ok(msg) => msg,
        Err(_) => return on_result(Err(RPCError::BincodeError)),
    };
    let channel_id = R::rpc_channel();
    let on_msg = move |res: RPCResult<Vec<u8>>| {
        let param = res.and_then(|bytes| bincode::deserialize(&bytes).map_err(RPCError::from));
        on_result(param);
        true
    };
    let cb = move |conn: &C, msg_id: u32| send_msg(conn, channel_id, msg_id, &msg);
    conn.on_msg(Box::new(on_msg), cb);
}

pub async fn async_call<C, R>(param: R, conn: &C) -> RPCResult<R::Return>
where
    R: Call,
    C: ConnectNoSend + 'static,
{
    let (sender, receiver) = oneshot::channel::<RPCResult<R::Return>>();
    let sender = RefCell::new(Some(sender));
    call(param, conn, move |result| {
        sender
            .borrow_mut()
            .take()
            .unwrap()
            .send(result)
            .ok()
            .unwrap();
    });
    receiver.await.unwrap()
}

pub async fn async_scall<C, R>(param: R, conn: &C) -> RPCResult<R::Return>
where
    R: Call,
    C: ConnectSend + 'static,
{
    let (sender, receiver) = oneshot::channel::<RPCResult<R::Return>>();
    let sender = RefCell::new(Some(sender));
    scall(param, conn, move |result| {
        sender
            .borrow_mut()
            .take()
            .unwrap()
            .send(result)
            .ok()
            .unwrap();
    });
    receiver.await.unwrap()
}

pub fn subscribe<C, R, F>(param: R, conn: &C, on_result: F)
where
    R: Subscribe,
    C: ConnectNoSend + 'static,
    F: Fn(RPCResult<R::Return>) -> bool + 'static,
{
    if !conn.is_connected() {
        on_result(Err(RPCError::NoConnected));
        return;
    }
    if !param.verify() {
        on_result(Err(RPCError::VerifyFailed));
        return;
    }
    let msg = match bincode::serialize(&param) {
        Ok(msg) => msg,
        Err(_) => {
            on_result(Err(RPCError::BincodeError));
            return;
        }
    };
    let channel_id = R::rpc_channel();
    let on_msg = move |res: RPCResult<Vec<u8>>| {
        let param = res.and_then(|bytes| bincode::deserialize(&bytes).map_err(RPCError::from));
        on_result(param)
    };
    let cb = move |conn: &C, msg_id: u32| {
        send_msg(conn, channel_id, msg_id, &msg);
    };
    conn.on_msg(Box::new(on_msg), cb);
}

pub fn ssubscribe<C, R, F>(param: R, conn: &C, on_result: F)
where
    R: Subscribe,
    C: ConnectSend + 'static,
    F: Fn(RPCResult<R::Return>) -> bool + 'static + Send,
{
    if !conn.is_connected() {
        on_result(Err(RPCError::NoConnected));
        return;
    }
    if !param.verify() {
        on_result(Err(RPCError::VerifyFailed));
        return;
    }
    let msg = match bincode::serialize(&param) {
        Ok(msg) => msg,
        Err(_) => {
            on_result(Err(RPCError::BincodeError));
            return;
        }
    };
    let channel_id = R::rpc_channel();
    let on_msg = move |res: RPCResult<Vec<u8>>| {
        let param = res.and_then(|bytes| bincode::deserialize(&bytes).map_err(RPCError::from));
        on_result(param)
    };
    let cb = move |conn: &C, msg_id: u32| {
        send_msg(conn, channel_id, msg_id, &msg);
    };
    conn.on_msg(Box::new(on_msg), cb);
}

fn from_io_result(src: IoError) -> RPCError {
    RPCError::IoError(src.to_string())
}

pub fn read_msg<R: Read>(n: &mut R) -> RPCResult<Response> {
    let mut h = [0u8; 12];
    n.read_exact(&mut h).map_err(from_io_result)?;
    let (len_buf, msg_id_buf): ([u8; 8], [u8; 4]) = unsafe { transmute(h) };
    let len = u64::from_be_bytes(len_buf) as usize;
    let msg_id = u32::from_be_bytes(msg_id_buf);

    let mut msg_bytes = Vec::with_capacity(len);
    unsafe {
        msg_bytes.set_len(len);
    };
    n.read_exact(&mut msg_bytes).map_err(from_io_result)?;
    let msg = msg_bytes;
    Ok(Response { msg_id, msg })
}
