use futures::stream::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
pub use todorpc_server_core::{Channels, ConnectionInfo, Context, ContextWithSender};
use todorpc_server_core::{IoStream, Server};
use tokio::io::{ReadHalf, Result as TIoResult, WriteHalf};
use tokio::net::{TcpListener, TcpStream as TokioTcpStream};

pub struct TcpStream(TokioTcpStream);

impl IoStream for TcpStream {
    type ReadStream = ReadHalf<TokioTcpStream>;
    type WriteStream = WriteHalf<TokioTcpStream>;
    fn connection_info(&self) -> Arc<dyn ConnectionInfo> {
        Arc::new(
            self.0
                .peer_addr()
                .map(|addr| format!("{}", addr))
                .unwrap_or_default(),
        )
    }
    fn split(self) -> (Self::ReadStream, Self::WriteStream) {
        let (read, write) = tokio::io::split(self.0);
        (read, write)
    }
}

async fn map_tcpstream(rts: TIoResult<TokioTcpStream>) -> Option<TcpStream> {
    if let Err(e) = rts {
        println!("{}", e);
        return None;
    }
    Some(TcpStream(rts.unwrap()))
}

pub struct TcpRPCServer {
    server: Server<Pin<Box<dyn Stream<Item = TcpStream> + Send + Sync>>>,
}
impl TcpRPCServer {
    pub fn new(channels: Arc<Channels>, tcplistener: TcpListener) -> TcpRPCServer {
        let stream = Box::pin(tcplistener.filter_map(map_tcpstream));
        TcpRPCServer {
            server: Server::new(channels, stream),
        }
    }
    pub async fn run(self) {
        self.server.run().await
    }
}
