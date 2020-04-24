use futures::stream::StreamExt;
use futures::TryFutureExt;
use quinn::Connecting;
use quinn::Incoming;
use std::sync::Arc;
use todorpc::{Message, Response};
pub use todorpc_server_core::{Channels, ConnectionInfo, Context, ContextWithSender};
//use todorpc_server_core::{IoStream, Server};
use anyhow::Result;
use quinn::Connection;
use quinn::SendStream;
use std::mem::transmute;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::RwLock;

pub struct ConnectionHandle(Connection);

impl ConnectionInfo for ConnectionHandle {
    fn remote_address(&self) -> String {
        format!("{}", self.0.remote_address())
    }
}

pub struct QuicRPCServer {
    channels: Arc<Channels>,
    incoming: Incoming,
}

impl QuicRPCServer {
    pub fn new(channels: Arc<Channels>, incoming: Incoming) -> QuicRPCServer {
        QuicRPCServer { channels, incoming }
    }
    pub async fn run(mut self) {
        while let Some(connecting) = self.incoming.next().await {
            tokio::spawn(
                handle_connection(connecting, self.channels.clone()).unwrap_or_else(move |e| {
                    println!("connection failed: {reason}", reason = e.to_string())
                }),
            );
        }
    }
}

async fn handle_connection(conn: Connecting, channels: Arc<Channels>) -> Result<()> {
    let quinn::NewConnection {
        connection,
        mut bi_streams,
        ..
    } = conn.await?;
    let token = Arc::new(RwLock::new(vec![]));
    while let Some(stream) = bi_streams.next().await {
        let stream = match stream {
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                println!("connection closed");
                return Ok(());
            }
            Err(e) => {
                return Err(e.into());
            }
            Ok(s) => s,
        };
        tokio::spawn(
            handle_request(stream, channels.clone(), connection.clone(), token.clone())
                .unwrap_or_else(move |e| println!("failed: {reason}", reason = e.to_string())),
        );
    }
    Ok(())
}

async fn handle_request(
    (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
    channels: Arc<Channels>,
    conn: quinn::Connection,
    token: Arc<RwLock<Vec<u8>>>,
) -> Result<()> {
    let mut buf = [0u8; 6];
    recv.read_exact(&mut buf).await?;
    let (len_buf, channel_id_buf): ([u8; 2], [u8; 4]) = unsafe { transmute(buf) };
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut msg = Vec::with_capacity(len);
    unsafe {
        msg.set_len(len);
    }
    let channel_id = u32::from_be_bytes(channel_id_buf);
    recv.read_exact(&mut msg).await?;
    let msg = Message {
        channel_id,
        msg_id: 0,
        msg,
    };
    let (tx, mut rx) = unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let result = write_stream(&mut send, msg).await;
            if let Err(e) = result {
                println!("{}", e);
                break;
            }
        }
    });
    let connection_info = Arc::new(ConnectionHandle(conn));
    channels.on_message(msg, tx, token, connection_info).await;

    Ok(())
}

pub async fn write_stream(n: &mut SendStream, m: Response) -> Result<()> {
    let msg = &m.msg;
    let len_buf = (msg.len() as u64).to_be_bytes();
    n.write_all(&len_buf).await?;
    n.write_all(msg.as_slice()).await?;
    n.flush().await?;
    Ok(())
}
