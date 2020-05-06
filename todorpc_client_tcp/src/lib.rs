use log::info;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use todorpc::*;
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncRead, AsyncReadExt, Error as IoError};
use tokio::net::TcpStream;
use tokio::stream::StreamExt;
use tokio::sync::mpsc::*;
use tokio::time::delay_for;

fn map_io_err(src: IoError) -> Error {
    Error::IoError(src.to_string())
}

async fn send_param<S: RPC>(param: &S, sender: &UnboundedSender<Op>, msg_id: u32) -> Result<()> {
    let ser = bincode::serialize(&param)?;
    if ser.len() > u16::max_value() as usize {
        return Err(Error::IoError("message length too big".to_string()));
    }
    let len_buf = (ser.len() as u16).to_be_bytes();
    let channel_buf = S::rpc_channel().to_be_bytes();
    let msg_buf = msg_id.to_be_bytes();
    let data = len_buf
        .iter()
        .chain(channel_buf.iter())
        .chain(msg_buf.iter())
        .cloned()
        .chain(ser)
        .collect();
    let req = Op::Req(Req { data });
    sender.send(req).map_err(|_| Error::ChannelClosed)?;
    Ok(())
}

async fn read_msg_async<R: AsyncRead + Unpin>(n: &mut R) -> Result<(Vec<u8>, u32)> {
    use std::mem::transmute;
    let mut h = [0u8; 12];
    n.read_exact(&mut h).await.map_err(map_io_err)?;
    let (len_buf, msg_id_buf): ([u8; 8], [u8; 4]) = unsafe { transmute(h) };
    let len = u64::from_be_bytes(len_buf) as usize;
    let msg_id = u32::from_be_bytes(msg_id_buf);

    let mut msg_bytes = Vec::with_capacity(len);
    unsafe {
        msg_bytes.set_len(len);
    };
    n.read_exact(&mut msg_bytes).await.map_err(map_io_err)?;
    let msg = msg_bytes;
    Ok((msg, msg_id))
}

struct Reg {
    sender: UnboundedSender<Result<Vec<u8>>>,
    msg_id: u32,
    is_call: bool,
}
struct Recv {
    msg_id: u32,
    data: Result<Vec<u8>>,
}

struct Timeout {
    msg_id: u32,
}

struct Req {
    data: Vec<u8>,
}

enum Op {
    Reg(Reg),
    Deconnect,
    Recv(Recv),
    Timeout(Timeout),
    Req(Req),
}

#[derive(Clone)]
pub struct TcpClient {
    id: Arc<AtomicU32>,
    ops: UnboundedSender<Op>,
    retry: Duration,
}

impl TcpClient {
    pub async fn connect(addr: SocketAddr, retry: Duration) -> TcpClient {
        let (req_tx, mut req_rx) = unbounded_channel::<Req>();
        let (ops_tx, mut ops_rx) = unbounded_channel::<Op>();
        let id = Arc::new(AtomicU32::new(0));
        let id_ref = id.clone();
        let ops_tx_ref4 = ops_tx.clone();
        tokio::spawn(async move {
            let mut regs = BTreeMap::new();
            while let Some(op) = ops_rx.next().await {
                match op {
                    Op::Reg(reg) => {
                        let id = reg.msg_id;
                        if reg.is_call {
                            let ops_tx_ref5 = ops_tx_ref4.clone();
                            tokio::spawn(async move {
                                delay_for(retry).await;
                                let _ = ops_tx_ref5.send(Op::Timeout(Timeout { msg_id: id }));
                            });
                        }
                        let _ = regs.insert(id, reg);
                    }
                    Op::Deconnect => {
                        id_ref.store(0, Ordering::SeqCst);
                        let closed = regs;
                        regs = BTreeMap::new();
                        for reg in closed.values() {
                            let _ = reg.sender.send(Err(Error::NoConnected));
                        }
                    }
                    Op::Recv(recv) => {
                        if let Some(reg) = regs.get(&recv.msg_id) {
                            if reg.sender.send(recv.data).is_err() || reg.is_call {
                                regs.remove(&recv.msg_id);
                            }
                        }
                    }
                    Op::Timeout(timeout) => {
                        if let Some(reg) = regs.remove(&timeout.msg_id) {
                            let _ = reg.sender.send(Err(Error::Other("timeout".to_owned())));
                        }
                    }
                    Op::Req(req) => {
                        let _ = req_tx.send(req);
                    }
                }
            }
        });
        let ops_tx_ref = ops_tx.clone();
        let retry2 = retry.clone();
        tokio::spawn(async move {
            loop {
                let ops_tx_ref2 = ops_tx_ref.clone();
                if let Ok(stream) = TcpStream::connect(addr).await {
                    let stream: TcpStream = stream;
                    let (mut read, mut write) = stream.into_split();
                    tokio::spawn(async move {
                        while let Ok((msg, id)) = read_msg_async(&mut read).await {
                            let _ = ops_tx_ref2.send(Op::Recv(Recv {
                                msg_id: id,
                                data: Ok(msg),
                            }));
                        }
                    });

                    while let Some(req) = req_rx.next().await {
                        if let Err(e) = write.write_all(&req.data).await {
                            info!("{:?}", e);
                            break;
                        }
                    }
                    drop(write);
                    let _ = ops_tx_ref.send(Op::Deconnect);
                }
                delay_for(retry2).await;
            }
        });
        TcpClient {
            ops: ops_tx,
            id,
            retry,
        }
    }
    pub async fn call<R: Call>(&self, param: R) -> Result<R::Return> {
        let (tx, mut rx) = unbounded_channel::<Result<Vec<u8>>>();
        let id = self.id.fetch_add(1, Ordering::SeqCst);
        self.ops
            .send(Op::Reg(Reg {
                msg_id: id,
                sender: tx,
                is_call: true,
            }))
            .map_err(|_| Error::ChannelClosed)?;
        self.req(&param, id).await?;
        if let Some(msg) = rx.next().await {
            let result = bincode::deserialize(&msg?)?;
            return Ok(result);
        };
        Err(Error::NoConnected)
    }
    pub async fn subscribe<R: Subscribe>(
        &self,
        param: R,
    ) -> Result<UnboundedReceiver<Result<R::Return>>> {
        let (res_tx, res_rx) = unbounded_channel::<Result<R::Return>>();
        let this = self.clone();
        let param = Arc::new(param);
        let mut rx = self.subscribe_inner(param.clone().as_ref()).await?;
        tokio::spawn(async move {
            loop {
                if let Some(msg) = rx.next().await {
                    match msg {
                        Err(e) => {
                            let _ = res_tx.send(Err(e.clone()));
                            if let Error::NoConnected = e {
                                loop {
                                    if let Ok(new_rx) = this.subscribe_inner(param.as_ref()).await {
                                        rx = new_rx;
                                        break;
                                    }
                                    delay_for(this.retry).await;
                                }
                            }
                        }
                        Ok(msg) => {
                            let result = bincode::deserialize(&msg);
                            let _ = res_tx.send(result.map_err(|e| e.into()));
                        }
                    }
                }
            }
        });
        Ok(res_rx)
    }
    async fn subscribe_inner<R: Subscribe>(
        &self,
        param: &R,
    ) -> Result<UnboundedReceiver<Result<Vec<u8>>>> {
        let (tx, rx) = unbounded_channel::<Result<Vec<u8>>>();
        let id = self.id.fetch_add(1, Ordering::SeqCst);
        self.ops
            .send(Op::Reg(Reg {
                msg_id: id,
                sender: tx,
                is_call: false,
            }))
            .map_err(|_| Error::ChannelClosed)?;
        self.req(param, id).await?;
        Ok(rx)
    }
    async fn req<R: RPC>(&self, param: &R, id: u32) -> Result<()> {
        if !param.verify() {
            return Err(Error::VerifyFailed);
        }
        send_param(param, &self.ops, id).await?;
        Ok(())
    }
}
