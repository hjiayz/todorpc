use log::{debug, info, trace};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use todorpc::*;
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncRead, AsyncReadExt, Error as IoError};
use tokio::net::TcpStream;
use tokio::stream::StreamExt;
use tokio::sync::mpsc::*;
use tokio::time::delay_for;

fn map_io_err(e: IoError) -> Error {
    debug!("{}", e);
    Error::TransportError
}

fn pack(req: &Req, msg_id: u32) -> Result<Vec<u8>> {
    if req.data.len() > u16::max_value() as usize {
        return Err(Error::MessageTooBig);
    }
    let len_buf = (req.data.len() as u16).to_be_bytes();
    let channel_buf = req.channel_id.to_be_bytes();
    let msg_buf = msg_id.to_be_bytes();
    let data = len_buf
        .iter()
        .chain(channel_buf.iter())
        .chain(msg_buf.iter())
        .chain(&req.data)
        .cloned()
        .collect();
    Ok(data)
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

struct Recv {
    msg_id: u32,
    data: Result<Vec<u8>>,
}

struct Timeout {
    msg_id: u32,
}

struct Req {
    sender: UnboundedSender<Result<Vec<u8>>>,
    is_call: bool,
    channel_id: u32,
    data: Vec<u8>,
}

enum Op {
    Deconnect,
    Recv(Recv),
    Timeout(Timeout),
    Req(Req),
}

#[derive(Clone)]
pub struct TcpClient {
    ops: UnboundedSender<Op>,
    retry: Duration,
}

impl TcpClient {
    pub async fn connect(addr: SocketAddr, retry: Duration) -> TcpClient {
        let (req_tx, mut req_rx) = unbounded_channel::<Vec<u8>>();
        let (ops_tx, mut ops_rx) = unbounded_channel::<Op>();
        let ops_tx_ref4 = ops_tx.clone();
        tokio::spawn(async move {
            let mut regs = BTreeMap::new();
            let mut id = 0;
            while let Some(op) = ops_rx.next().await {
                match op {
                    Op::Req(req) => {
                        if id == u32::max_value() {
                            let _ = ops_tx_ref4.send(Op::Deconnect);
                            trace!("message id overflow");
                            if req.sender.send(Err(Error::ConnectionReset)).is_err() {
                                trace!("requester droped");
                            }
                            continue;
                        };
                        id += 1;

                        match pack(&req, id) {
                            Ok(data) => {
                                let _ = regs.insert(id, (req.sender, req.is_call));
                                let _ = req_tx.send(data);
                            }
                            Err(e) => {
                                if req.sender.send(Err(e)).is_err() {
                                    trace!("requester droped");
                                }
                                continue;
                            }
                        }
                        if req.is_call {
                            let ops_tx_ref5 = ops_tx_ref4.clone();
                            tokio::spawn(async move {
                                delay_for(retry).await;
                                let _ = ops_tx_ref5.send(Op::Timeout(Timeout { msg_id: id }));
                            });
                        }
                    }
                    Op::Deconnect => {
                        id = 0;
                        let closed = regs;
                        regs = BTreeMap::new();
                        for reg in closed.values() {
                            let _ = reg.0.send(Err(Error::ConnectionReset));
                        }
                    }
                    Op::Recv(recv) => {
                        if let Some(reg) = regs.get(&recv.msg_id) {
                            if reg.0.send(recv.data).is_err() || reg.1 {
                                regs.remove(&recv.msg_id);
                            }
                        }
                    }
                    Op::Timeout(timeout) => {
                        if let Some(reg) = regs.remove(&timeout.msg_id) {
                            let _ = reg.0.send(Err(Error::Timeout));
                        }
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
                        if let Err(e) = write.write_all(&req).await {
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
        TcpClient { ops: ops_tx, retry }
    }
    pub async fn call<R: Call>(&self, param: R) -> Result<R::Return> {
        let mut rx = self.req(&param, true).await?;
        if let Some(msg) = rx.next().await {
            let result = bincode::deserialize(&msg?).map_err(|e| {
                debug!("{}", e);
                Error::DeserializeFaild
            })?;
            return Ok(result);
        };
        Err(Error::ConnectionReset)
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
                            if let Err(_) = res_tx.send(Err(e.clone())) {
                                trace!("subscribe {} end", R::rpc_channel());
                                break;
                            }
                            debug!("{:?}", e);
                        }
                        Ok(msg) => {
                            let result = bincode::deserialize(&msg).map_err(|e| {
                                debug!("{}", e);
                                Error::DeserializeFaild
                            });
                            if let Err(_) = res_tx.send(result) {
                                trace!("subscribe {} end", R::rpc_channel());
                                break;
                            }
                        }
                    }
                    continue;
                }
                delay_for(this.retry).await;
                loop {
                    trace!("try subscribe {} again", R::rpc_channel());
                    if let Ok(new_rx) = this.subscribe_inner(param.as_ref()).await {
                        rx = new_rx;
                        break;
                    }
                    delay_for(this.retry).await;
                }
            }
        });
        Ok(res_rx)
    }
    async fn subscribe_inner<R: Subscribe>(
        &self,
        param: &R,
    ) -> Result<UnboundedReceiver<Result<Vec<u8>>>> {
        Ok(self.req(param, false).await?)
    }
    async fn req<R: RPC>(
        &self,
        param: &R,
        is_call: bool,
    ) -> Result<UnboundedReceiver<Result<Vec<u8>>>> {
        if !param.verify() {
            return Err(Error::VerifyFailed);
        }
        let ser = bincode::serialize(&param).map_err(|e| {
            debug!("{}", e);
            Error::SerializeFaild
        })?;
        let (tx, rx) = unbounded_channel::<Result<Vec<u8>>>();
        self.ops
            .send(Op::Req(Req {
                sender: tx,
                is_call: is_call,
                channel_id: R::rpc_channel(),
                data: ser,
            }))
            .map_err(|_| Error::ConnectionDroped)?;
        Ok(rx)
    }
}
