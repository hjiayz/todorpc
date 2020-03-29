use bincode::{deserialize, serialize};
use serde::Serialize;
use std::any::TypeId;
use std::collections::BTreeMap;
use std::future::Future;
use std::marker::PhantomData;
use std::mem::transmute;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use todorpc::{Call, Error, Message, Result as RPCResult, Subscribe};
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::Error as IoError;
use tokio::io::ErrorKind as IoErrorKind;
use tokio::io::Result as IoResult;
use tokio::stream::{Stream, StreamExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

pub trait IoStream: Send + Sync + 'static {
    type ReadStream: AsyncRead + Unpin + Send + Sync;
    type WriteStream: AsyncWrite + Unpin + Send + Sync;
    fn remote_address(&self) -> Option<String>;
    fn split(self) -> (Self::ReadStream, Self::WriteStream);
}

pub async fn read_stream<R: AsyncRead + Unpin>(n: &mut R) -> IoResult<Message> {
    let mut h = [0u8; 16];
    n.read_exact(&mut h).await?;
    let (len_buf, channel_id_buf, msg_id_buf): ([u8; 8], [u8; 4], [u8; 4]) =
        unsafe { transmute(h) };
    let len = u64::from_be_bytes(len_buf) as usize;
    let channel_id = u32::from_be_bytes(channel_id_buf);
    let msg_id = u32::from_be_bytes(msg_id_buf);
    if len > (1 << 14) {
        return Err(IoError::new(IoErrorKind::Other, "msg length invalid"));
    }
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

pub async fn write_stream<W: AsyncWrite + Unpin>(n: &mut W, m: Message) -> IoResult<()> {
    let msg = &m.msg;
    let len = msg.len();
    let len_buf = (msg.len() as u64).to_be_bytes();
    let channel_id_buf = m.channel_id.to_be_bytes();
    let msg_id_buf = m.msg_id.to_be_bytes();
    let h: [u8; 16] = unsafe { transmute((len_buf, channel_id_buf, msg_id_buf)) };
    let mut buf = Vec::with_capacity(16 + len);
    buf.extend_from_slice(&h);
    buf.extend_from_slice(msg.as_slice());
    n.write_all(&buf).await?;
    Ok(())
}

pub async fn on_stream<S: IoStream>(iostream: S, channels: Arc<Channels>) {
    let token = Arc::new(RwLock::new(vec![]));
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
        tokio::spawn(async move {
            channels2.on_message(msg, tx2, token2).await;
        });
    }
}
pub struct ContextWithSender<T> {
    sender: UnboundedSender<Message>,
    ctx: Context,
    pd: PhantomData<T>,
}

fn send<T: Serialize + 'static>(
    sender: &UnboundedSender<Message>,
    channel_id: u32,
    msg_id: u32,
    msg: &T,
) -> RPCResult<()> {
    if TypeId::of::<T>() == TypeId::of::<()>() {
        return Ok(());
    }
    let msg = serialize(msg)?;
    sender
        .send(Message {
            msg,
            channel_id,
            msg_id,
        })
        .map_err(|_| Error::ChannelClosed)?;
    Ok(())
}

impl<T: Serialize + 'static> ContextWithSender<T> {
    pub fn send(&self, msg: &T) -> RPCResult<()> {
        send(&self.sender, self.ctx.channel_id(), self.ctx.msg_id(), msg)
    }
    pub fn channel_id(&self) -> u32 {
        self.ctx.channel_id()
    }
    pub fn msg_id(&self) -> u32 {
        self.ctx.msg_id()
    }
    pub fn set_token(&mut self, new_token: &[u8]) -> RPCResult<()> {
        self.ctx.set_token(new_token)
    }
    pub fn get_token(&mut self) -> RPCResult<Vec<u8>> {
        self.ctx.get_token()
    }
}

pub struct Context {
    channel_id: u32,
    msg_id: u32,
    token: Arc<RwLock<Vec<u8>>>,
}

impl Context {
    pub fn channel_id(&self) -> u32 {
        self.channel_id
    }
    pub fn msg_id(&self) -> u32 {
        self.msg_id
    }
    pub fn set_token(&mut self, new_token: &[u8]) -> RPCResult<()> {
        let mut token = self
            .token
            .try_write()
            .map_err(|_| Error::TrySetTokenFaild)?;
        (*token) = new_token.to_vec();
        Ok(())
    }
    pub fn get_token(&mut self) -> RPCResult<Vec<u8>> {
        let token = self
            .token
            .try_read()
            .map_err(|_| Error::TryReadTokenFaild)?;
        Ok(token.to_vec())
    }
}

pub struct Channels {
    channels: BTreeMap<
        u32,
        Box<
            dyn Fn(
                    &[u8],
                    UnboundedSender<Message>,
                    u32,
                    Arc<RwLock<Vec<u8>>>,
                ) -> Pin<Box<dyn Future<Output = ()> + Send + Sync>>
                + Send
                + Sync,
        >,
    >,
}

impl Channels {
    pub fn new() -> Channels {
        Channels {
            channels: BTreeMap::new(),
        }
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
                      sender: UnboundedSender<Message>,
                      msg_id: u32,
                      token: Arc<RwLock<Vec<u8>>>| {
                    let param = deserialize(bytes).map_err(Error::from).and_then(|repo: R| {
                        if repo.verify() {
                            Ok(repo)
                        } else {
                            Err(Error::VerifyFailed)
                        }
                    });
                    let channel_id = R::rpc_channel();
                    let ctx = Context {
                        channel_id,
                        msg_id,
                        token,
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
                      sender: UnboundedSender<Message>,
                      msg_id: u32,
                      token: Arc<RwLock<Vec<u8>>>| {
                    let param = deserialize(bytes).map_err(Error::from).and_then(|repo: R| {
                        if repo.verify() {
                            Ok(repo)
                        } else {
                            Err(Error::VerifyFailed)
                        }
                    });
                    let channel_id = R::rpc_channel();
                    let ctx = Context {
                        channel_id,
                        msg_id,
                        token,
                    };
                    let result = p(param, ctx);
                    Box::pin(async move {
                        let msg = result.await;
                        if let Err(e) = send(&sender, R::rpc_channel(), msg_id, &msg) {
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
        unbounded_channel: UnboundedSender<Message>,
        token: Arc<RwLock<Vec<u8>>>,
    ) {
        if let Some(f) = self.channels.get(&msg.channel_id) {
            f(&msg.msg, unbounded_channel, msg.msg_id, token).await;
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