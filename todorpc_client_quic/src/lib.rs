use quinn::Connection;
use quinn::Endpoint;
use quinn::EndpointBuilder;
use quinn::RecvStream;
use quinn::VarInt;
use serde::de::DeserializeOwned;
use std::marker::Unpin;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, Ordering::SeqCst},
    Arc,
};
use todorpc::*;
pub use todorpc_client_core::{async_scall as async_call, scall as call, ssubscribe as subscribe};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio::time::{delay_for, Duration};

#[derive(Clone)]
pub struct QuicClient {
    conn: Connection,
}

async fn send_param<S: RPC, W: AsyncWriteExt + Unpin>(param: &S, sender: &mut W) -> Result<()> {
    let ser = bincode::serialize(&param)?;
    if ser.len() > u16::max_value() as usize {
        return Err(Error::IoError("message length too big".to_string()));
    }
    let len_buf = (ser.len() as u16).to_be_bytes();
    let channel_buf = S::rpc_channel().to_be_bytes();
    sender.write_all(&len_buf).await.map_err(map_io_error)?;
    sender.write_all(&channel_buf).await.map_err(map_io_error)?;
    sender.write_all(&ser).await.map_err(map_io_error)?;
    Ok(())
}

fn map_io_error<E: std::error::Error>(e: E) -> Error {
    Error::IoError(e.to_string())
}

async fn read_result<D: DeserializeOwned, R: AsyncReadExt + Unpin>(recv: &mut R) -> Result<D> {
    let mut len_buf = [0u8; 8];
    recv.read_exact(&mut len_buf).await.map_err(map_io_error)?;
    let len = u64::from_be_bytes(len_buf) as usize;
    let mut msg_bytes = Vec::with_capacity(len);
    unsafe {
        msg_bytes.set_len(len);
    };
    recv.read_exact(&mut msg_bytes)
        .await
        .map_err(map_io_error)?;
    Ok(bincode::deserialize(&msg_bytes)?)
}

impl QuicClient {
    pub fn connect(conn: Connection) -> QuicClient {
        QuicClient { conn }
    }
    pub async fn call<R: Call>(&self, param: &R) -> Result<R::Return> {
        let mut recv = self.req(param).await?;
        let result = read_result(&mut recv).await?;
        let _ = recv.stop(VarInt::from_u32(0));
        Ok(result)
    }
    pub async fn subscribe<R: Subscribe>(
        &self,
        param: &R,
    ) -> Result<mpsc::UnboundedReceiver<Result<R::Return>>> {
        async fn try_read<D: DeserializeOwned>(
            recv: &mut RecvStream,
            tx: &mpsc::UnboundedSender<Result<D>>,
        ) -> Result<()> {
            let res = read_result(recv).await?;
            tx.send(Ok(res)).map_err(|_| Error::ChannelClosed)?;
            Ok(())
        }
        let (tx, rx) = mpsc::unbounded_channel::<Result<R::Return>>();
        let mut recv = self.req(param).await?;
        tokio::spawn(async move {
            while try_read(&mut recv, &tx).await.is_ok() {}
            let _ = recv.stop(VarInt::from_u32(0));
        });
        Ok(rx)
    }
    async fn req<R: RPC>(&self, param: &R) -> Result<RecvStream> {
        if !param.verify() {
            return Err(Error::VerifyFailed);
        }
        let (mut send, recv) = self.conn.open_bi().await.map_err(|_| Error::NoConnected)?;

        send_param(param, &mut send).await?;
        send.finish().await.map_err(map_io_error)?;
        Ok(recv)
    }
}

pub struct ConnectInfo {
    timeout: Duration,
    addr: SocketAddr,
    hostname: String,
    ep: Endpoint,
}

impl ConnectInfo {
    async fn try_connect(&self) -> Result<QuicClient> {
        let conn = self
            .ep
            .connect(&self.addr, &self.hostname)
            .map_err(|e| Error::IoError(e.to_string()))?
            .await
            .map_err(|e| Error::IoError(e.to_string()))?
            .connection;
        Ok(QuicClient::connect(conn))
    }
    async fn connect(&self) -> QuicClient {
        loop {
            if let Ok(client) = self.try_connect().await {
                return client;
            }
            delay_for(self.timeout).await
        }
    }
}

pub struct Retry {
    info: Arc<ConnectInfo>,
    retrying: AtomicBool,
    client: Arc<RwLock<Option<QuicClient>>>,
}

impl Retry {
    pub async fn new<H: Into<String>>(
        timeout: i32,
        remote_addr: SocketAddr,
        hostname: H,
        builder: EndpointBuilder,
    ) -> Result<Arc<Retry>> {
        let (endpoint, _) = builder
            .bind(&"[::]:0".parse().unwrap())
            .map_err(|e| Error::Other(e.to_string()))?;
        let info = Arc::new(ConnectInfo {
            timeout: Duration::from_millis(timeout as u64),
            addr: remote_addr,
            hostname: hostname.into(),
            ep: endpoint,
        });
        let client = Arc::new(RwLock::new(Some(info.connect().await)));
        let retry = Retry {
            info,
            client,
            retrying: AtomicBool::new(false),
        };
        Ok(Arc::new(retry))
    }
    async fn get_client(&self) -> Result<QuicClient> {
        self.client.read().await.clone().ok_or(Error::NoConnected)
    }
    pub async fn call<C: Call>(&self, params: &C) -> Result<C::Return> {
        match self.get_client().await?.call(params).await {
            Err(Error::NoConnected) => {
                self.retry().await;
                Err(Error::NoConnected)
            }
            other => other,
        }
    }
    pub async fn subscribe<S: Subscribe>(
        &self,
        params: &S,
    ) -> Result<mpsc::UnboundedReceiver<Result<S::Return>>> {
        match self.get_client().await?.subscribe(params).await {
            Err(Error::NoConnected) => {
                self.retry().await;
                Err(Error::NoConnected)
            }
            other => other,
        }
    }
    async fn retry(&self) {
        self.retrying.store(true, SeqCst);
        *self.client.write().await = None;
        let client = self.client.clone();
        let info = self.info.clone();
        tokio::spawn(async move {
            *client.write().await = Some(info.connect().await);
        });
        self.retrying.store(false, SeqCst);
    }
}
