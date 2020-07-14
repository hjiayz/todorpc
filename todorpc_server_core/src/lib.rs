use bincode::{deserialize, serialize};
use log::{debug, error, trace, warn};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::BTreeMap;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use todorpc::{
    Call, Error, Message, Response, Result as RPCResult, Subscribe, Upload, Verify, RPC,
};
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::Result as IoResult;
use tokio::stream::{Stream, StreamExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot::{channel, Sender};

pub trait ConnectionInfo: Sync + Send {
    fn remote_address(&self) -> String;
    fn protocol(&self) -> &'static str;
}

pub trait IoStream: Send + Sync + 'static {
    type ReadStream: AsyncRead + Unpin + Send + Sync;
    type WriteStream: AsyncWrite + Unpin + Send + Sync;
    fn connection_info(&self) -> Arc<dyn ConnectionInfo>;
    fn split(self) -> (Self::ReadStream, Self::WriteStream);
}

pub async fn read_stream<R: AsyncRead + Unpin>(n: &mut R) -> IoResult<(Message, u32)> {
    let mut len_buf = [0u8; 2];
    let mut channel_id_buf = [0u8; 4];
    let mut msg_id_buf = [0u8; 4];
    n.read_exact(&mut len_buf).await?;
    n.read_exact(&mut channel_id_buf).await?;
    n.read_exact(&mut msg_id_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    let channel_id = u32::from_be_bytes(channel_id_buf);
    let msg_id = u32::from_be_bytes(msg_id_buf);
    let mut msg_bytes = vec![0u8; len];
    n.read_exact(&mut msg_bytes).await?;
    let msg = msg_bytes;
    let msg = Message { channel_id, msg };
    Ok((msg, msg_id))
}

pub async fn write_stream<W: AsyncWrite + Unpin>(
    n: &mut W,
    m: Response,
    msg_id: u32,
) -> IoResult<()> {
    let msg = &m.msg;
    let len = msg.len();
    let len_buf = (msg.len() as u64).to_be_bytes();
    let msg_id_buf = msg_id.to_be_bytes();
    let mut buf = Vec::with_capacity(12 + len);
    buf.extend_from_slice(&len_buf);
    buf.extend_from_slice(&msg_id_buf);
    buf.extend_from_slice(msg.as_slice());
    n.write_all(&buf).await?;
    n.flush().await?;
    Ok(())
}

pub fn token() -> UnboundedSender<TokenCommand> {
    let (tx, mut rx) = unbounded_channel();
    tokio::spawn(async move {
        let mut token = Vec::default();
        while let Some(cmd) = rx.recv().await {
            match cmd {
                TokenCommand::GetToken(sender) => {
                    if let Err(e) = sender.send(token.clone()) {
                        debug!("{:?}", e);
                    }
                }
                TokenCommand::SetToken(new_token) => {
                    let len = new_token.len();
                    if len > 1024 {
                        warn!("token len:{}", len);
                    }
                    token = new_token;
                }
            }
        }
    });
    tx
}

pub async fn on_stream<S: IoStream>(iostream: S, channels: Arc<Channels>, id: u128) {
    let token = token();
    let connection_info = iostream.connection_info();
    let (mut rs, mut ws) = iostream.split();
    let (tx, mut rx) = unbounded_channel();
    tokio::spawn(async move {
        while let Some((msg, msg_id)) = rx.recv().await {
            let result = write_stream(&mut ws, msg, msg_id).await;
            if let Err(e) = result {
                error!("{}", e);
                break;
            }
        }
    });
    let mut upload_list: BTreeMap<u32, UnboundedSender<Vec<u8>>> = BTreeMap::new();
    loop {
        let result = read_stream(&mut rs).await;
        if let Err(e) = result {
            error!("{}", e);
            break;
        }
        let (msg, msg_id) = result.unwrap();
        let is_upload_msg = msg.channel_id == u32::max_value();
        if is_upload_msg {
            if let Some(upload_tx) = upload_list.get(&msg_id) {
                if !msg.msg.is_empty() {
                    if let Err(e) = upload_tx.send(msg.msg) {
                        debug!("{}", e);
                    }
                } else {
                    upload_list.remove(&msg_id);
                    trace!("msg {} removed", msg_id);
                }
                continue;
            }
            error!("upload message id error");
            continue;
        }
        let tx2 = tx.clone();
        let (stream_tx, mut stream_rx) = unbounded_channel();
        tokio::spawn(async move {
            while let Some(msg) = stream_rx.recv().await {
                if let Err(e) = tx2.send((msg, msg_id)) {
                    error!("stream closed : {}", e);
                    break;
                };
            }
        });
        let token2 = token.clone();
        let channels2 = channels.clone();
        let connection_info2 = connection_info.clone();
        let (upload_tx, fut) = channels2.on_message(msg, stream_tx, token2, connection_info2, id);
        if let Some(fut) = fut {
            tokio::spawn(async {
                fut.await;
            });
        }
        if let Some(upload_tx) = upload_tx {
            upload_list.insert(msg_id, upload_tx);
        }
    }
    channels.on_close(token, connection_info, id).await;
}

pub struct ContextWithSender<T> {
    sender: UnboundedSender<Response>,
    ctx: Context,
    pd: PhantomData<T>,
}

impl<T> Clone for ContextWithSender<T> {
    fn clone(&self) -> Self {
        ContextWithSender {
            sender: self.sender.clone(),
            ctx: self.ctx.clone(),
            pd: PhantomData,
        }
    }
}

fn send<T: Serialize + 'static>(sender: &UnboundedSender<Response>, msg: &T) -> RPCResult<()> {
    let msg = serialize(msg).map_err(|e| {
        debug!("{}", e);
        Error::SerializeFaild
    })?;
    sender
        .send(Response { msg })
        .map_err(|_| Error::ConnectionReset)?;
    Ok(())
}

pub struct ContextWithUpload<T> {
    recv: UnboundedReceiver<Vec<u8>>,
    ctx: Context,
    pd: PhantomData<T>,
}

impl<T: DeserializeOwned + 'static> ContextWithUpload<T> {
    pub fn into_stream(self) -> impl Stream<Item = RPCResult<T>> {
        self.recv
            .map(|bytes| deserialize(&bytes).map_err(|_| Error::DeserializeFaild))
    }
    pub async fn set_token(&self, new_token: Vec<u8>) {
        self.ctx.set_token(new_token).await
    }
    pub async fn get_token(&self) -> Vec<u8> {
        self.ctx.get_token().await
    }
    pub fn connection_info(&self) -> Arc<dyn ConnectionInfo> {
        self.ctx.connection_info().clone()
    }
    pub fn context(&self) -> &Context {
        &self.ctx
    }
    pub fn id(&self) -> u128 {
        self.ctx.id
    }
}

impl<T: Serialize + 'static> ContextWithSender<T> {
    pub fn send(&self, msg: &T) -> RPCResult<()> {
        send(&self.sender, msg)
    }
    pub async fn set_token(&self, new_token: Vec<u8>) {
        self.ctx.set_token(new_token).await
    }
    pub async fn get_token(&self) -> Vec<u8> {
        self.ctx.get_token().await
    }
    pub fn connection_info(&self) -> Arc<dyn ConnectionInfo> {
        self.ctx.connection_info().clone()
    }
    pub fn context(&self) -> &Context {
        &self.ctx
    }
    pub fn id(&self) -> u128 {
        self.ctx.id
    }
}

pub enum TokenCommand {
    GetToken(Sender<Vec<u8>>),
    SetToken(Vec<u8>),
}

#[derive(Clone)]
pub struct Context {
    connection_info: Arc<dyn ConnectionInfo>,
    token: UnboundedSender<TokenCommand>,
    id: u128,
}

impl Context {
    pub async fn set_token(&self, new_token: Vec<u8>) {
        let _ = self.token.send(TokenCommand::SetToken(new_token));
    }
    pub async fn get_token(&self) -> Vec<u8> {
        let (tx, rx) = channel();
        let _ = self.token.send(TokenCommand::GetToken(tx));
        rx.await.unwrap_or_default()
    }
    pub fn connection_info(&self) -> Arc<dyn ConnectionInfo> {
        self.connection_info.clone()
    }
    pub fn id(&self) -> u128 {
        self.id
    }
}

type OnChannelMsgNoRecv = Box<
    dyn Fn(
            Vec<u8>,
            UnboundedSender<Response>,
            Context,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + Sync>>
        + Send
        + Sync,
>;

type OnChannelMsgWithRecv = Box<
    dyn Fn(
            Vec<u8>,
            UnboundedSender<Response>,
            UnboundedReceiver<Vec<u8>>,
            Context,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + Sync>>
        + Send
        + Sync,
>;

type OnClose =
    Box<dyn Send + Sync + Fn(Context) -> Pin<Box<dyn Future<Output = ()> + Send + Sync>>>;

enum OnChannelMsg {
    WithRecv(OnChannelMsgWithRecv),
    NoRecv(OnChannelMsgNoRecv),
}

#[derive(Default)]
pub struct Channels {
    channels: BTreeMap<u32, OnChannelMsg>,
    on_close: Option<OnClose>,
}

fn decode<R: RPC + Verify>(bytes: &[u8]) -> Result<RPCResult<R>, R::Return> {
    match deserialize::<R>(bytes) {
        Ok(r) => r.verify_result().map(Ok),
        Err(e) => {
            debug!("{}", e);
            Ok(Err(Error::DeserializeFaild))
        }
    }
}

impl Channels {
    pub fn new() -> Channels {
        Self::default()
    }
    pub fn set_subscribe<R, F, P>(mut self, p: P) -> Self
    where
        R: Subscribe,
        F: Future<Output = ()> + Send + Sync + 'static,
        P: (Fn(RPCResult<R>, ContextWithSender<R::Return>) -> F) + 'static + Send + Sync,
    {
        let rpc_channel = R::rpc_channel();
        #[cfg(debug)]
        reserved_check(rpc_channel);
        if self.channels.contains_key(&rpc_channel) {
            panic!("channel id conflict");
        }
        self.channels.insert(
            rpc_channel,
            OnChannelMsg::NoRecv(Box::new(
                move |bytes: Vec<u8>, sender: UnboundedSender<Response>, ctx: Context| {
                    let param = decode::<R>(&bytes);
                    let ctx_with_sender = ContextWithSender::<R::Return> {
                        sender,
                        ctx,
                        pd: PhantomData,
                    };
                    let fut = match param {
                        Ok(param) => Some(p(param, ctx_with_sender)),
                        Err(ret) => {
                            if let Err(e) = ctx_with_sender.send(&ret) {
                                debug!("{:?}", e);
                            }
                            None
                        }
                    };
                    Box::pin(async move {
                        if let Some(fut) = fut {
                            fut.await;
                        }
                    })
                },
            )),
        );
        self
    }
    pub fn set_call<R, F, P>(mut self, p: P) -> Self
    where
        R: Call,
        F: Future<Output = R::Return> + Send + Sync + 'static,
        P: (Fn(RPCResult<R>, Context) -> F) + 'static + Send + Sync,
    {
        let rpc_channel = R::rpc_channel();
        #[cfg(debug)]
        reserved_check(rpc_channel);
        if self.channels.contains_key(&rpc_channel) {
            panic!("channel id conflict");
        }
        self.channels.insert(
            rpc_channel,
            OnChannelMsg::NoRecv(Box::new(
                move |bytes: Vec<u8>, sender: UnboundedSender<Response>, ctx: Context| {
                    let fut = decode::<R>(&bytes).map(|param| p(param, ctx));
                    Box::pin(async move {
                        let msg = match fut {
                            Ok(fut) => fut.await,
                            Err(res) => res,
                        };
                        if let Err(e) = send(&sender, &msg) {
                            error!("{:?}", e);
                        }
                    })
                },
            )),
        );
        self
    }
    pub fn set_upload<R, F, P>(mut self, p: P) -> Self
    where
        R: Upload,
        F: Future<Output = R::Return> + Send + Sync + 'static,
        P: (Fn(RPCResult<R>, ContextWithUpload<R::UploadStream>) -> F) + 'static + Send + Sync,
    {
        let rpc_channel = R::rpc_channel();
        #[cfg(debug)]
        reserved_check(rpc_channel);
        if cfg!(debug) && self.channels.contains_key(&rpc_channel) {
            panic!("channel id conflict");
        }
        self.channels.insert(
            rpc_channel,
            OnChannelMsg::WithRecv(Box::new(
                move |bytes: Vec<u8>,
                      sender: UnboundedSender<Response>,
                      recv: UnboundedReceiver<Vec<u8>>,
                      ctx: Context| {
                    let ctx = ContextWithUpload {
                        recv,
                        ctx,
                        pd: PhantomData,
                    };
                    let fut = decode::<R>(&bytes).map(|param| p(param, ctx));
                    Box::pin(async move {
                        let msg = match fut {
                            Ok(fut) => fut.await,
                            Err(res) => res,
                        };
                        if let Err(e) = send(&sender, &msg) {
                            error!("{:?}", e);
                        }
                    })
                },
            )),
        );
        self
    }
    pub fn set_onclose<F, Fut>(mut self, onclose: F) -> Self
    where
        Fut: Future<Output = ()> + Send + Sync + 'static,
        F: Fn(Context) -> Fut + Send + Sync + 'static,
    {
        self.on_close = Some(Box::new(move |ctx| {
            let fut = onclose(ctx);
            Box::pin(async move { fut.await })
        }));
        self
    }
    pub fn finish(self) -> Arc<Channels> {
        Arc::new(self)
    }

    pub fn on_message(
        &self,
        msg: Message,
        sender: UnboundedSender<Response>,
        token: UnboundedSender<TokenCommand>,
        connection_info: Arc<dyn ConnectionInfo>,
        id: u128,
    ) -> (
        Option<UnboundedSender<Vec<u8>>>,
        Option<impl Future<Output = ()>>,
    ) {
        let ctx = Context {
            connection_info,
            token,
            id,
        };
        if let Some(f) = self.channels.get(&msg.channel_id) {
            match f {
                OnChannelMsg::NoRecv(f) => {
                    let fut = f(msg.msg, sender, ctx);
                    (None, Some(fut))
                }
                OnChannelMsg::WithRecv(f) => {
                    let (tx, rx) = unbounded_channel::<Vec<u8>>();
                    let fut = f(msg.msg, sender, rx, ctx);
                    (Some(tx), Some(fut))
                }
            }
        } else {
            error!("unknown channel id {}", msg.channel_id);
            (None, None)
        }
    }

    pub async fn on_close(
        &self,
        token: UnboundedSender<TokenCommand>,
        connection_info: Arc<dyn ConnectionInfo>,
        id: u128,
    ) {
        let ctx = Context {
            token,
            connection_info,
            id,
        };
        if let Some(ref f) = self.on_close {
            f(ctx).await;
        }
    }
}

pub struct Server<S> {
    channels: Arc<Channels>,
    stream: S,
}

impl<S: Stream> Server<S>
where
    S: Stream + Unpin,
    S::Item: IoStream,
{
    pub fn new(channels: Arc<Channels>, stream: S) -> Server<S> {
        Server { channels, stream }
    }
    pub async fn run(self) {
        let mut stream = self.stream;
        let mut count = 0u128;
        while let Some(iostream) = stream.next().await {
            let channels = self.channels.clone();
            let id = count;
            count += 1u128;
            tokio::spawn(async move { on_stream(iostream, channels, id).await });
        }
    }
}

#[cfg(debug)]
fn reserved_check(rpc_channel: u32) {
    if rpc_channel > (u32::max_value() - 100) {
        panic!("reserved id");
    }
}
