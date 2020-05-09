use log::{debug, info};
use quinn::Connection;
use quinn::Endpoint;
use quinn::EndpointBuilder;
use quinn::RecvStream;
use quinn::SendStream;
use quinn::VarInt;
use serde::de::DeserializeOwned;
use std::marker::Unpin;
use std::net::SocketAddr;
use todorpc::*;
use tokio::io::AsyncReadExt;
use tokio::stream::StreamExt;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot::{channel, Sender};
use tokio::time::{delay_for, Duration};

#[derive(Clone)]
pub struct QuicClient {
    timeout: Duration,
    tx: UnboundedSender<Sender<(SendStream, RecvStream)>>,
}

async fn send_param<S: RPC>(param: &S, sender: &mut SendStream) -> Result<()> {
    let ser = bincode::serialize(&param).map_err(|_| Error::SerializeFaild)?;
    if ser.len() > u16::max_value() as usize {
        return Err(Error::MessageTooBig);
    }
    let len_buf = (ser.len() as u16).to_be_bytes();
    let channel_buf = S::rpc_channel().to_be_bytes();
    sender.write_all(&len_buf).await.map_err(map_io_error)?;
    sender.write_all(&channel_buf).await.map_err(map_io_error)?;
    sender.write_all(&ser).await.map_err(map_io_error)?;
    //sender.finish().await.map_err(map_io_error)?;
    Ok(())
}

fn map_io_error<E: std::error::Error>(e: E) -> Error {
    debug!("{}", e);
    Error::TransportError
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
    Ok(bincode::deserialize(&msg_bytes).map_err(|_| Error::DeserializeFaild)?)
}

impl QuicClient {
    pub fn connect(
        timeout: i32,
        remote_addr: SocketAddr,
        hostname: &str,
        builder: EndpointBuilder,
    ) -> Result<QuicClient> {
        let (endpoint, _) = builder.bind(&"[::]:0".parse().unwrap()).map_err(|e| {
            debug!("{}", e);
            Error::ConnectionFailed
        })?;
        let timeout = Duration::from_millis(timeout as u64);
        let info = ConnectInfo {
            timeout: timeout.clone(),
            addr: remote_addr,
            hostname: hostname.into(),
            ep: endpoint,
        };
        let (tx, mut rx) = unbounded_channel::<Sender<(SendStream, RecvStream)>>();
        tokio::spawn(async move {
            let mut conn = info.connect().await;
            while let Some(stream_tx) = rx.next().await {
                loop {
                    let stream = match conn.open_bi().await {
                        Ok(stream) => stream,
                        Err(e) => {
                            debug!("{}", e);
                            conn.close(VarInt::default(), &[]);
                            conn = info.connect().await;
                            continue;
                        }
                    };
                    if let Err(_) = stream_tx.send(stream) {
                        debug!("connect request channel closed");
                    }
                    break;
                }
            }
        });
        Ok(QuicClient { tx, timeout })
    }
    pub async fn call<R: Call>(&self, param: R) -> Result<R::Return> {
        let mut recv = self.req(&param).await?;
        let result = read_result(&mut recv).await?;
        let _ = recv.stop(VarInt::from_u32(0));
        Ok(result)
    }
    pub async fn subscribe<R: Subscribe>(
        &self,
        param: R,
    ) -> Result<UnboundedReceiver<Result<R::Return>>> {
        async fn try_read<D: DeserializeOwned, R: Subscribe>(
            mut recv: RecvStream,
            tx: &UnboundedSender<Result<D>>,
            client: &QuicClient,
            param: &R,
        ) -> Result<RecvStream> {
            let res = match read_result(&mut recv).await {
                Ok(res) => Ok(res),
                Err(e) => {
                    debug!("{:?}", e);
                    recv = client.re_req(param).await?;
                    Err(Error::ConnectionReset)
                }
            };
            tx.send(res).map_err(|_| {
                info!("subscribe end");
                Error::ConnectionReset
            })?;
            Ok(recv)
        }
        let (tx, rx) = unbounded_channel::<Result<R::Return>>();
        let mut recv = self.req(&param).await?;
        let client = self.clone();
        tokio::spawn(async move {
            while let Ok(new_recv) = try_read(recv, &tx, &client, &param).await {
                recv = new_recv
            }
        });
        Ok(rx)
    }
    async fn req<R: RPC>(&self, param: &R) -> Result<RecvStream> {
        if !param.verify() {
            return Err(Error::VerifyFailed);
        }
        let (tx, rx) = channel();
        self.tx.send(tx).map_err(|_| Error::ConnectionDroped)?;
        let (mut send, recv) = rx.await.map_err(|_| Error::ConnectionDroped)?;

        send_param(param, &mut send).await?;
        Ok(recv)
    }
    async fn re_req<R: RPC>(&self, param: &R) -> Result<RecvStream> {
        let (tx, rx) = channel();
        self.tx.send(tx).map_err(|_| Error::ConnectionDroped)?;
        let (mut send, recv) = rx.await.map_err(|_| Error::ConnectionDroped)?;
        while let Err(e) = send_param(param, &mut send).await {
            debug!("{:?}", e);
            delay_for(self.timeout).await;
        }
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
    async fn try_connect(&self) -> Result<Connection> {
        let socket = std::net::UdpSocket::bind("[::]:0").map_err(|e| {
            debug!("{}", e);
            Error::ConnectionFailed
        })?;
        self.ep.rebind(socket).map_err(|e| {
            debug!("{}", e);
            Error::ConnectionFailed
        })?;
        let conn = self
            .ep
            .connect(&self.addr, &self.hostname)
            .map_err(|e| {
                debug!("{}", e);
                Error::ConnectionFailed
            })?
            .await
            .map_err(|e| {
                debug!("{}", e);
                Error::ConnectionFailed
            })?
            .connection;
        Ok(conn)
    }
    async fn connect(&self) -> Connection {
        loop {
            if let Ok(client) = self.try_connect().await {
                return client;
            }
            delay_for(self.timeout).await
        }
    }
}
