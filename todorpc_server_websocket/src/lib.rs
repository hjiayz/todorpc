use futures::ready;
use futures::sink::Sink;
use futures::stream::{SplitSink, SplitStream, Stream, StreamExt};
use std::cmp;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{self, Poll};
pub use todorpc_server_core::{Channels, Context, ContextWithSender};
use todorpc_server_core::{IoStream, Server};
use tokio::io::{
    AsyncRead, AsyncWrite, Error as IoError, ErrorKind as IoErrorKind, Result as TIoResult,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tungstenite::Message;

pub struct WsStream(WebSocketStream<TcpStream>);

pub struct WsRead {
    stream: SplitStream<WebSocketStream<TcpStream>>,
    state: Option<(Vec<u8>, usize)>,
}
pub struct WsWrite(SplitSink<WebSocketStream<TcpStream>, Message>);

fn map_err<E: std::error::Error + Sync + Send + 'static>(e: E) -> IoError {
    IoError::new(IoErrorKind::Other, e)
}

impl AsyncRead for WsRead {
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

impl AsyncWrite for WsWrite {
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

impl IoStream for WsStream {
    type ReadStream = WsRead;
    type WriteStream = WsWrite;
    fn remote_address(&self) -> Option<String> {
        self.0
            .get_ref()
            .peer_addr()
            .map(|addr| format!("{}", addr))
            .ok()
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

async fn map_tcpstream(rts: TIoResult<TcpStream>) -> Option<WsStream> {
    use tokio_tungstenite::accept_async;
    if let Err(e) = rts {
        println!("{}", e);
        return None;
    }
    let ts = rts.unwrap();
    let rws = accept_async(ts).await;
    if let Err(e) = rws {
        println!("{}", e);
        return None;
    }
    let ws = rws.unwrap();
    Some(WsStream(ws))
}

pub struct WSRPCServer {
    server: Server<Pin<Box<dyn Stream<Item = WsStream> + Send + Sync>>>,
}
impl WSRPCServer {
    pub fn new(channels: Arc<Channels>, tcplistener: TcpListener) -> WSRPCServer {
        let stream = Box::pin(tcplistener.filter_map(map_tcpstream));
        WSRPCServer {
            server: Server::new(channels, stream),
        }
    }
    pub async fn run(self) {
        self.server.run().await
    }
}
