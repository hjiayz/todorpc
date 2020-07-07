use futures::ready;
use futures::sink::Sink;
use futures::stream::{SplitSink, SplitStream, Stream, StreamExt};
use log::error;
use native_tls::TlsAcceptor;
use std::cmp;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{self, Poll};
pub use todorpc_server_core::{Channels, ConnectionInfo, Context, ContextWithSender};
use todorpc_server_core::{IoStream, Server};
use tokio::io::{
    AsyncRead, AsyncWrite, Error as IoError, ErrorKind as IoErrorKind, Result as TIoResult,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tls::{TlsAcceptor as ATlsAcceptor, TlsStream as ATlsStream};
use tokio_tungstenite::WebSocketStream;
use tungstenite::Message;

pub struct WsStream<S>(WebSocketStream<S>);

pub struct WsRead<S> {
    stream: SplitStream<WebSocketStream<S>>,
    state: Option<(Vec<u8>, usize)>,
}
pub struct WsWrite<S>(SplitSink<WebSocketStream<S>, Message>);

fn map_err<E: std::error::Error + Sync + Send + 'static>(e: E) -> IoError {
    IoError::new(IoErrorKind::Other, e)
}

impl<S: AsyncWrite + AsyncRead + Sync + Send + Unpin + 'static> AsyncRead for WsRead<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
        buf: &mut [u8],
    ) -> Poll<TIoResult<usize>> {
        loop {
            match &mut self.state {
                Some((chunk, chunk_start)) => {
                    let len = cmp::min(buf.len(), chunk.len() - *chunk_start);

                    buf[..len].copy_from_slice(&chunk[*chunk_start..*chunk_start + len]);
                    *chunk_start += len;

                    if chunk.len() == *chunk_start {
                        self.state = None;
                    }

                    return Poll::Ready(Ok(len));
                }
                None => match ready!(Pin::new(&mut self.stream).poll_next(cx)) {
                    Some(Ok(chunk)) => {
                        self.state = Some((chunk.into_data(), 0));
                    }
                    Some(Err(err)) => {
                        self.state = None;
                        return Poll::Ready(Err(map_err(err)));
                    }
                    None => {
                        self.state = None;
                        return Poll::Ready(Ok(0));
                    }
                },
            }
        }
    }
}

impl<S: AsyncWrite + AsyncRead + Sync + Send + Unpin + 'static> AsyncWrite for WsWrite<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
        buf: &[u8],
    ) -> Poll<TIoResult<usize>> {
        let sink = Pin::new(&mut self.0);
        let res = ready!(sink.poll_ready(cx));
        if let Err(e) = res {
            return Poll::Ready(Err(map_err(e)));
        };
        let len = buf.len();
        let sink = Pin::new(&mut self.0);
        Poll::Ready(
            sink.start_send(Message::binary(buf))
                .map_err(map_err)
                .map(|_| len),
        )
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<TIoResult<()>> {
        let sink = Pin::new(&mut self.0);
        sink.poll_flush(cx).map_err(map_err)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<TIoResult<()>> {
        let sink = Pin::new(&mut self.0);
        sink.poll_close(cx).map_err(map_err)
    }
}

impl IoStream for WsStream<TcpStream> {
    type ReadStream = WsRead<TcpStream>;
    type WriteStream = WsWrite<TcpStream>;
    fn connection_info(&self) -> Arc<dyn ConnectionInfo> {
        Arc::new(
            self.0
                .get_ref()
                .peer_addr()
                .map(|addr| format!("{}", addr))
                .unwrap_or_default(),
        )
    }
    fn split(self) -> (Self::ReadStream, Self::WriteStream) {
        let (sink, stream) = self.0.split();
        (
            WsRead {
                stream,
                state: None,
            },
            WsWrite(sink),
        )
    }
}

impl IoStream for WsStream<ATlsStream<TcpStream>> {
    type ReadStream = WsRead<ATlsStream<TcpStream>>;
    type WriteStream = WsWrite<ATlsStream<TcpStream>>;
    fn connection_info(&self) -> Arc<dyn ConnectionInfo> {
        Arc::new(
            self.0
                .get_ref()
                .get_ref()
                .peer_addr()
                .map(|addr| format!("{}", addr))
                .unwrap_or_default(),
        )
    }
    fn split(self) -> (Self::ReadStream, Self::WriteStream) {
        let (sink, stream) = self.0.split();
        (
            WsRead {
                stream,
                state: None,
            },
            WsWrite(sink),
        )
    }
}

async fn map_tcpstream(rts: TIoResult<TcpStream>) -> Option<WsStream<TcpStream>> {
    use tokio_tungstenite::accept_async;
    let ts = rts
        .map_err(|e| {
            error!("{}", e);
        })
        .ok()?;
    ts.set_nodelay(true)
        .map_err(|e| {
            error!("{}", e);
        })
        .ok()?;
    let ws = accept_async(ts)
        .await
        .map_err(|e| {
            error!("{}", e);
        })
        .ok()?;
    Some(WsStream(ws))
}

async fn map_tlstream(
    rtls: Option<ATlsStream<TcpStream>>,
) -> Option<WsStream<ATlsStream<TcpStream>>> {
    use tokio_tungstenite::accept_async_with_config;
    use tungstenite::protocol::WebSocketConfig;
    const CONFIG: WebSocketConfig = WebSocketConfig {
        max_send_queue: None,
        max_message_size: Some(64 << 10),
        max_frame_size: Some(64 << 10),
    };
    if let Some(rtls) = &rtls {
        rtls.get_ref()
            .set_nodelay(true)
            .map_err(|e| {
                error!("{}", e);
            })
            .ok()?;
    }
    let ws = accept_async_with_config(rtls?, Some(CONFIG))
        .await
        .map_err(|e| {
            error!("{}", e);
        })
        .ok()?;
    Some(WsStream(ws))
}

pub struct WSRPCServer<S> {
    server: Server<Pin<Box<dyn Stream<Item = WsStream<S>> + Send + Sync>>>,
}

impl WSRPCServer<TcpStream> {
    pub fn new(channels: Arc<Channels>, tcplistener: TcpListener) -> WSRPCServer<TcpStream> {
        let stream = Box::pin(tcplistener.filter_map(map_tcpstream));
        WSRPCServer {
            server: Server::new(channels, stream),
        }
    }
    pub async fn run(self) {
        self.server.run().await
    }
}

impl WSRPCServer<ATlsStream<TcpStream>> {
    pub fn new_tls(
        channels: Arc<Channels>,
        tcplistener: TcpListener,
        tls: TlsAcceptor,
    ) -> WSRPCServer<ATlsStream<TcpStream>> {
        let atls = ATlsAcceptor::from(tls);
        let stream = Box::pin(tcplistener.filter_map(move |rts| {
            let atls = atls.clone();
            async move {
                let rtls = atls.accept(rts.ok()?).await.ok();
                map_tlstream(rtls).await
            }
        }));
        WSRPCServer {
            server: Server::new(channels, stream),
        }
    }
    pub async fn run(self) {
        self.server.run().await
    }
}
