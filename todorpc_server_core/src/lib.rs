use bincode::{deserialize, serialize};
use log::{debug, error, warn};
use serde::Serialize;
use std::any::TypeId;
use std::collections::BTreeMap;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::transmute;
use std::pin::Pin;
use std::sync::Arc;
use todorpc::{Call, Error, Message, Response, Result as RPCResult, Subscribe, Verify, RPC};
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::Result as IoResult;
use tokio::stream::{Stream, StreamExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::oneshot::{channel, Sender};

pub trait ConnectionInfo: Sync + Send {
    fn remote_address(&self) -> String;
}

impl ConnectionInfo for String {
    fn remote_address(&self) -> String {
        self.to_owned()
    }
}

pub trait IoStream: Send + Sync + 'static {
    type ReadStream: AsyncRead + Unpin + Send + Sync;
    type WriteStream: AsyncWrite + Unpin + Send + Sync;
    fn connection_info(&self) -> Arc<dyn ConnectionInfo>;
    fn split(self) -> (Self::ReadStream, Self::WriteStream);
}

pub async fn read_stream<R: AsyncRead + Unpin>(n: &mut R) -> IoResult<(Message, u32)> {
    let mut h = [0u8; 10];
    n.read_exact(&mut h).await?;
    let (len_buf, channel_id_buf, msg_id_buf): ([u8; 2], [u8; 4], [u8; 4]) =
        unsafe { transmute(h) };
    let len = u16::from_be_bytes(len_buf) as usize;
    let channel_id = u32::from_be_bytes(channel_id_buf);
    let msg_id = u32::from_be_bytes(msg_id_buf);
    let mut msg_bytes = Vec::with_capacity(len);
    unsafe {
        msg_bytes.set_len(len);
    };
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
    let h: [u8; 12] = unsafe { transmute((len_buf, msg_id_buf)) };
    let mut buf = Vec::with_capacity(12 + len);
    buf.extend_from_slice(&h);
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
    loop {
        let result = read_stream(&mut rs).await;
        if let Err(e) = result {
            error!("{}", e);
            break;
        }
        let (msg, msg_id) = result.unwrap();
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
        tokio::spawn(async move {
            channels2
                .on_message(msg, stream_tx, token2, connection_info2, id)
                .await;
        });
    }
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
    if TypeId::of::<T>() == TypeId::of::<()>() {
        return Ok(());
    }
    let msg = serialize(msg).map_err(|e| {
        debug!("{}", e);
        Error::SerializeFaild
    })?;
    sender
        .send(Response { msg })
        .map_err(|_| Error::ConnectionReset)?;
    Ok(())
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

type OnChannelMsg = Box<
    dyn Fn(
            &[u8],
            UnboundedSender<Response>,
            Context,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + Sync>>
        + Send
        + Sync,
>;

#[derive(Default)]
pub struct Channels {
    channels: BTreeMap<u32, OnChannelMsg>,
}

fn decode<R: RPC + Verify>(bytes: &[u8]) -> Result<RPCResult<R>, R::Return> {
    match deserialize::<R>(bytes) {
        Ok(r) => r.verify_result().map(|res| Ok(res)),
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
        if self.channels.contains_key(&rpc_channel) {
            panic!("channel id conflict");
        }
        self.channels.insert(
            rpc_channel,
            Box::new(
                move |bytes: &[u8], sender: UnboundedSender<Response>, ctx: Context| {
                    let param = decode::<R>(bytes);
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
            ),
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
        if self.channels.contains_key(&rpc_channel) {
            panic!("channel id conflict");
        }
        self.channels.insert(
            rpc_channel,
            Box::new(
                move |bytes: &[u8], sender: UnboundedSender<Response>, ctx: Context| {
                    let fut = decode::<R>(bytes).map(|param| p(param, ctx));
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
            ),
        );
        self
    }
    pub fn finish(self) -> Arc<Channels> {
        Arc::new(self)
    }

    pub async fn on_message(
        &self,
        msg: Message,
        unbounded_channel: UnboundedSender<Response>,
        token: UnboundedSender<TokenCommand>,
        connection_info: Arc<dyn ConnectionInfo>,
        id: u128,
    ) {
        let ctx = Context {
            connection_info,
            token,
            id,
        };
        if let Some(f) = self.channels.get(&msg.channel_id) {
            f(&msg.msg, unbounded_channel, ctx).await;
        } else {
            error!("unknown channel id {}", msg.channel_id);
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
