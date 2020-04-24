use bincode::{deserialize, serialize};
use serde::Serialize;
use std::any::TypeId;
use std::collections::BTreeMap;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::transmute;
use std::pin::Pin;
use std::sync::Arc;
use todorpc::{Call, Error, Message, Response, Result as RPCResult, Subscribe, RPC};
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::Result as IoResult;
use tokio::stream::{Stream, StreamExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::RwLock;

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

pub async fn read_stream<R: AsyncRead + Unpin>(n: &mut R) -> IoResult<Message> {
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
    let msg = Message {
        channel_id,
        msg_id,
        msg,
    };
    Ok(msg)
}

pub async fn write_stream<W: AsyncWrite + Unpin>(n: &mut W, m: Response) -> IoResult<()> {
    let msg = &m.msg;
    let len = msg.len();
    let len_buf = (msg.len() as u64).to_be_bytes();
    let msg_id_buf = m.msg_id.to_be_bytes();
    let h: [u8; 12] = unsafe { transmute((len_buf, msg_id_buf)) };
    let mut buf = Vec::with_capacity(12 + len);
    buf.extend_from_slice(&h);
    buf.extend_from_slice(msg.as_slice());
    n.write_all(&buf).await?;
    n.flush().await?;
    Ok(())
}

pub async fn on_stream<S: IoStream>(iostream: S, channels: Arc<Channels>) {
    let token = Arc::new(RwLock::new(vec![]));
    let connection_info = iostream.connection_info();
    let (mut rs, mut ws) = iostream.split();
    let (tx, mut rx) = unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let result = write_stream(&mut ws, msg).await;
            if let Err(e) = result {
                println!("{}", e);
                break;
            }
        }
    });
    loop {
        let result = read_stream(&mut rs).await;
        if let Err(e) = result {
            println!("{}", e);
            break;
        }
        let msg = result.unwrap();
        let tx2 = tx.clone();
        let token2 = token.clone();
        let channels2 = channels.clone();
        let connection_info2 = connection_info.clone();
        tokio::spawn(async move {
            channels2
                .on_message(msg, tx2, token2, connection_info2)
                .await;
        });
    }
}
pub struct ContextWithSender<T> {
    sender: UnboundedSender<Response>,
    ctx: Context,
    pd: PhantomData<T>,
}

fn send<T: Serialize + 'static>(
    sender: &UnboundedSender<Response>,
    msg_id: u32,
    msg: &T,
) -> RPCResult<()> {
    if TypeId::of::<T>() == TypeId::of::<()>() {
        return Ok(());
    }
    let msg = serialize(msg)?;
    sender
        .send(Response { msg, msg_id })
        .map_err(|_| Error::ChannelClosed)?;
    Ok(())
}

impl<T: Serialize + 'static> ContextWithSender<T> {
    pub fn send(&self, msg: &T) -> RPCResult<()> {
        send(&self.sender, self.ctx.msg_id, msg)
    }
    pub async fn set_token(&self, new_token: &[u8]) {
        self.ctx.set_token(new_token).await
    }
    pub async fn get_token(&self) -> Vec<u8> {
        self.ctx.get_token().await
    }
    pub fn connection_info(&self) -> Arc<dyn ConnectionInfo> {
        self.ctx.connection_info().clone()
    }
}

pub struct Context {
    connection_info: Arc<dyn ConnectionInfo>,
    msg_id: u32,
    token: Arc<RwLock<Vec<u8>>>,
}

impl Context {
    pub async fn set_token(&self, new_token: &[u8]) {
        let mut token = self.token.write().await;
        *token = new_token.to_vec();
    }
    pub async fn get_token(&self) -> Vec<u8> {
        let token = self.token.read().await;
        token.to_vec()
    }
    pub fn connection_info(&self) -> Arc<dyn ConnectionInfo> {
        self.connection_info.clone()
    }
}

type OnChannelMsg = Box<
    dyn Fn(
            &[u8],
            UnboundedSender<Response>,
            u32,
            Arc<RwLock<Vec<u8>>>,
            Arc<dyn ConnectionInfo>,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + Sync>>
        + Send
        + Sync,
>;

#[derive(Default)]
pub struct Channels {
    channels: BTreeMap<u32, OnChannelMsg>,
}

fn decode<R: RPC>(bytes: &[u8]) -> RPCResult<R> {
    deserialize(bytes).map_err(Error::from).and_then(|repo: R| {
        if repo.verify() {
            Ok(repo)
        } else {
            Err(Error::VerifyFailed)
        }
    })
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
                move |bytes: &[u8],
                      sender: UnboundedSender<Response>,
                      msg_id: u32,
                      token: Arc<RwLock<Vec<u8>>>,
                      connection_info: Arc<dyn ConnectionInfo>| {
                    let param = decode::<R>(bytes);
                    let ctx = Context {
                        msg_id,
                        token,
                        connection_info,
                    };
                    let ctx_with_sender = ContextWithSender::<R::Return> {
                        sender,
                        ctx,
                        pd: PhantomData,
                    };
                    let result = p(param, ctx_with_sender);
                    Box::pin(async move {
                        result.await;
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
                move |bytes: &[u8],
                      sender: UnboundedSender<Response>,
                      msg_id: u32,
                      token: Arc<RwLock<Vec<u8>>>,
                      connection_info: Arc<dyn ConnectionInfo>| {
                    let param = decode::<R>(bytes);
                    let ctx = Context {
                        msg_id,
                        token,
                        connection_info,
                    };
                    let result = p(param, ctx);
                    Box::pin(async move {
                        let msg = result.await;
                        if let Err(e) = send(&sender, msg_id, &msg) {
                            println!("{:?}", e);
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
        token: Arc<RwLock<Vec<u8>>>,
        connection_info: Arc<dyn ConnectionInfo>,
    ) {
        if let Some(f) = self.channels.get(&msg.channel_id) {
            f(
                &msg.msg,
                unbounded_channel,
                msg.msg_id,
                token,
                connection_info,
            )
            .await;
        } else {
            println!("unknown channel id {}", msg.msg_id);
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
        while let Some(iostream) = stream.next().await {
            let channels = self.channels.clone();
            tokio::spawn(async move { on_stream(iostream, channels).await });
        }
    }
}
