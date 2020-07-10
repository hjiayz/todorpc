use anyhow::Result;
use futures::stream::StreamExt;
use futures::TryFutureExt;
use log::error;
use quinn::Connecting;
use quinn::Connection;
use quinn::Incoming;
use quinn::SendStream;
use std::mem::transmute;
use std::sync::Arc;
use todorpc::{Message, Response};
pub use todorpc_server_core::{
    token, Channels, ConnectionInfo, Context, ContextWithSender, TokenCommand,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

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
        let mut count = 0u128;
        while let Some(connecting) = self.incoming.next().await {
            let id = count;
            count += 1u128;
            tokio::spawn(
                handle_connection(connecting, self.channels.clone(), id).unwrap_or_else(move |e| {
                    error!("connection failed: {reason}", reason = e.to_string())
                }),
            );
        }
    }
}

async fn handle_connection(conn: Connecting, channels: Arc<Channels>, id: u128) -> Result<()> {
    let quinn::NewConnection {
        connection,
        mut bi_streams,
        ..
    } = conn.await?;
    let token = token();
    let connection_info = Arc::new(ConnectionHandle(connection));
    while let Some(stream) = bi_streams.next().await {
        let stream = match stream {
            Err(quinn::ConnectionError::ApplicationClosed(info)) => {
                error!("connection closed: {reason}", reason = info.to_string());
                return Ok(());
            }
            Err(e) => {
                return Err(e.into());
            }
            Ok(s) => s,
        };
        tokio::spawn(
            handle_request(
                stream,
                channels.clone(),
                connection_info.clone(),
                token.clone(),
                id,
            )
            .unwrap_or_else(move |e| error!("failed: {reason}", reason = e.to_string())),
        );
    }
    channels.on_close(token, connection_info, id).await;
    Ok(())
}

async fn handle_request(
    (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
    channels: Arc<Channels>,
    connection_info: Arc<ConnectionHandle>,
    token: UnboundedSender<TokenCommand>,
    id: u128,
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
    let msg = Message { channel_id, msg };
    let (tx, mut rx) = unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let result = write_stream(&mut send, msg).await;
            if let Err(e) = result {
                error!("{}", e);
                break;
            }
        }
    });
    let (upload_tx, fut) = channels.on_message(msg, tx, token, connection_info, id);
    if let Some(fut) = fut {
        tokio::spawn(fut);
    }
    if let Some(upload_tx) = upload_tx {
        loop {
            let mut buf = [0u8; 2];
            recv.read_exact(&mut buf).await?;
            let len = u16::from_be_bytes(buf) as usize;
            let mut msg = Vec::with_capacity(len);
            unsafe {
                msg.set_len(len);
            };
            recv.read_exact(&mut msg).await?;
            if let Err(e) = upload_tx.send(msg) {
                error!("{:?}", e);
                break;
            }
        }
    }

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
