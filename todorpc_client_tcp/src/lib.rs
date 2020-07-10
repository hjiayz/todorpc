use log::{debug, info, trace};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use todorpc::*;
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncRead, AsyncReadExt, Error as IoError};
use tokio::net::TcpStream;
use tokio::stream::Stream;
use tokio::stream::StreamExt;
use tokio::sync::mpsc::*;
use tokio::sync::oneshot;
use tokio::time::delay_for;

fn map_io_err(e: IoError) -> Error {
    debug!("{}", e);
    Error::TransportError
}

fn pack(req: &Req, msg_id: u32) -> Result<Vec<u8>> {
    upload(req.channel_id, &req.data, msg_id)
}

fn upload(channel_id: u32, data: &[u8], msg_id: u32) -> Result<Vec<u8>> {
    if data.len() > u16::max_value() as usize {
        return Err(Error::MessageTooBig);
    }
    let len_buf = (data.len() as u16).to_be_bytes();
    let channel_buf = channel_id.to_be_bytes();
    let msg_buf = msg_id.to_be_bytes();
    let data = len_buf
        .iter()
        .chain(channel_buf.iter())
        .chain(msg_buf.iter())
        .chain(data)
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
    timeout_id: u128,
}

struct Req {
    sender: UnboundedSender<Result<Vec<u8>>>,
    channel_id: u32,
    data: Vec<u8>,
}

struct ReqNoCall {
    sender: UnboundedSender<Result<Vec<u8>>>,
    msg_id_sender: oneshot::Sender<u32>,
    channel_id: u32,
    data: Vec<u8>,
}

struct Uploading {
    upload_ok_tx: oneshot::Sender<()>,
    data: Vec<u8>,
    msg_id: u32,
}

enum Op {
    Deconnect,
    Recv(Recv),
    Timeout(Timeout),
    Req(Req),
    Uploading(Uploading),
    ReqNoCall(ReqNoCall),
    EndNoCall(u32),
}

#[derive(Clone)]
pub struct TcpClient {
    ops: UnboundedSender<Op>,
    retry: Duration,
}

fn timeout_checker(id: u128, tx: UnboundedSender<Op>, retry: Duration) -> u128 {
    let new_id = id + 1;
    tokio::spawn(async move {
        delay_for(retry).await;
        let _ = tx.send(Op::Timeout(Timeout { timeout_id: new_id }));
    });
    new_id
}

impl TcpClient {
    pub async fn connect(addr: SocketAddr, retry: Duration) -> TcpClient {
        let (req_tx, mut req_rx) = unbounded_channel::<(Vec<u8>, Option<oneshot::Sender<()>>)>();
        let (ops_tx, mut ops_rx) = unbounded_channel::<Op>();
        let ops_tx_ref4 = ops_tx.clone();
        tokio::spawn(async move {
            let mut regs = BTreeMap::new();
            let mut id = 0;
            let mut timeout_id = 0u128;
            while let Some(op) = ops_rx.next().await {
                match op {
                    Op::Req(mut req) => {
                        if id == u32::max_value() {
                            let _ = ops_tx_ref4.send(Op::Deconnect);
                            trace!("message id overflow");
                            if req.sender.send(Err(Error::ConnectionReset)).is_err() {
                                trace!("requester droped");
                            }
                            continue;
                        };
                        id += 1;

                        match pack(&mut req, id) {
                            Ok(data) => {
                                let _ = regs.insert(id, (req.sender, true));
                                let _ = req_tx.send((data, None));
                            }
                            Err(e) => {
                                if req.sender.send(Err(e)).is_err() {
                                    trace!("requester droped");
                                }
                                continue;
                            }
                        }
                        timeout_id = timeout_checker(timeout_id, ops_tx_ref4.clone(), retry);
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
                        timeout_id = timeout_checker(timeout_id, ops_tx_ref4.clone(), retry);
                    }
                    Op::Timeout(timeout) => {
                        if timeout_id == timeout.timeout_id {
                            let _ = ops_tx_ref4.send(Op::Deconnect);
                        }
                    }
                    Op::ReqNoCall(req) => {
                        if id == u32::max_value() {
                            let _ = ops_tx_ref4.send(Op::Deconnect);
                            trace!("message id overflow");
                            if req.sender.send(Err(Error::ConnectionReset)).is_err() {
                                trace!("requester droped");
                            }
                            continue;
                        };
                        id += 1;
                        if let Err(_) = req.msg_id_sender.send(id) {
                            trace!("requester droped");
                            continue;
                        }
                        match upload(req.channel_id, &req.data, id) {
                            Ok(data) => {
                                let _ = regs.insert(id, (req.sender, false));
                                let _ = req_tx.send((data, None));
                            }
                            Err(e) => {
                                if req.sender.send(Err(e)).is_err() {
                                    trace!("requester droped");
                                }
                                continue;
                            }
                        }
                        timeout_id = timeout_checker(timeout_id, ops_tx_ref4.clone(), retry);
                    }
                    Op::Uploading(uploading) => {
                        match upload(u32::max_value(), &uploading.data, uploading.msg_id) {
                            Ok(data) => {
                                let _ = req_tx.send((data, Some(uploading.upload_ok_tx)));
                            }
                            Err(e) => {
                                debug!("{:?}", e);
                                continue;
                            }
                        }
                        timeout_id = timeout_checker(timeout_id, ops_tx_ref4.clone(), retry);
                    }
                    Op::EndNoCall(msg_id) => {
                        if let None = regs.remove(&msg_id) {
                            debug!("no message id : {}", msg_id);
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
                        if let Err(e) = write.write_all(&req.0).await {
                            info!("{:?}", e);
                            break;
                        }
                        if let Some(tx) = req.1 {
                            if let Err(_) = tx.send(()) {
                                debug!("requester droped");
                            }
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
        if let Err(res) = param.verify() {
            return Ok(res);
        }
        let mut rx = self.req(&param).await?;
        if let Some(msg) = rx.next().await {
            let result = bincode::deserialize(&msg?).map_err(|e| {
                debug!("{}", e);
                Error::DeserializeFaild
            })?;
            return Ok(result);
        };
        Err(Error::ConnectionReset)
    }
    pub async fn upload<R: Upload>(
        &self,
        param: R,
        stream: impl Unpin + Stream<Item = R::UploadStream>,
    ) -> Result<R::Return> {
        if let Err(res) = param.verify() {
            return Ok(res);
        }
        let (mut sender_rx, msg_id) = self.req_no_call(&param).await?;
        let stream = stream.map(|item| {
            bincode::serialize(&item).map_err(|e| {
                debug!("{}", e);
                Error::SerializeFaild
            })
        });
        self.upload_inner(msg_id, stream).await?;
        if let Some(msg) = sender_rx.next().await {
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
        if let Err(res) = param.verify() {
            let _ = res_tx.send(Ok(res));
            return Ok(res_rx);
        }
        let this = self.clone();
        let param = Arc::new(param);
        let (mut rx, mut msg_id) = self.subscribe_inner(param.clone().as_ref()).await?;
        tokio::spawn(async move {
            loop {
                if let Some(msg) = rx.next().await {
                    match msg {
                        Err(e) => {
                            if let Err(_) = res_tx.send(Err(e.clone())) {
                                trace!("subscribe {} end", R::rpc_channel());
                                this.end_req(msg_id);
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
                    if let Ok((new_rx, new_msg_id)) = this.subscribe_inner(param.as_ref()).await {
                        rx = new_rx;
                        msg_id = new_msg_id;
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
    ) -> Result<(UnboundedReceiver<Result<Vec<u8>>>, u32)> {
        Ok(self.req_no_call(param).await?)
    }
    async fn req<R: RPC + Verify>(&self, param: &R) -> Result<UnboundedReceiver<Result<Vec<u8>>>> {
        let (tx, rx) = unbounded_channel::<Result<Vec<u8>>>();
        let ser = bincode::serialize(&param).map_err(|e| {
            debug!("{}", e);
            Error::SerializeFaild
        })?;

        self.ops
            .send(Op::Req(Req {
                sender: tx,
                channel_id: R::rpc_channel(),
                data: ser,
            }))
            .map_err(|_| Error::ConnectionDroped)?;
        Ok(rx)
    }
    async fn req_no_call<R: RPC + Verify>(
        &self,
        param: &R,
    ) -> Result<(UnboundedReceiver<Result<Vec<u8>>>, u32)> {
        let data = bincode::serialize(&param).map_err(|e| {
            debug!("{}", e);
            Error::SerializeFaild
        })?;
        let (sender, sender_rx) = unbounded_channel();
        let (msg_id_sender, msg_id_sender_rx) = oneshot::channel();
        let channel_id = R::rpc_channel();
        let start = ReqNoCall {
            sender,
            msg_id_sender,
            channel_id,
            data,
        };
        if let Err(e) = self.ops.send(Op::ReqNoCall(start)) {
            debug!("{}", e);
            return Err(Error::ConnectionReset);
        }
        let msg_id = msg_id_sender_rx.await.map_err(|e| {
            debug!("{}", e);
            Error::ConnectionReset
        })?;
        Ok((sender_rx, msg_id))
    }
    async fn upload_inner(
        &self,
        msg_id: u32,
        mut stream: impl Unpin + Stream<Item = Result<Vec<u8>>>,
    ) -> Result<()> {
        while let Some(data) = stream.next().await {
            let data = data?;
            let (upload_ok_tx, upload_ok_rx) = oneshot::channel();
            self.ops
                .send(Op::Uploading(Uploading {
                    upload_ok_tx,
                    data: data,
                    msg_id,
                }))
                .map_err(|_| Error::ConnectionReset)?;
            upload_ok_rx.await.map_err(|_| Error::ConnectionReset)?;
        }
        Ok(())
    }
    fn end_req(&self, msg_id: u32) {
        if let Err(_) = self.ops.send(Op::EndNoCall(msg_id)) {
            debug!("end req failed");
        }
    }
}
