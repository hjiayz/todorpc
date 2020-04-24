use quinn::Connection;
use quinn::RecvStream;
use quinn::VarInt;
use serde::de::DeserializeOwned;
use std::marker::Unpin;
use todorpc::*;
pub use todorpc_client_core::{async_scall as async_call, scall as call, ssubscribe as subscribe};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct QuicClient {
    conn: Connection,
}

async fn send_param<S: RPC, W: AsyncWriteExt + Unpin>(param: &S, sender: &mut W) -> Result<()> {
    let ser = bincode::serialize(&param)?;
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
        let (mut send, recv) = self.conn.open_bi().await.map_err(map_io_error)?;

        send_param(param, &mut send).await?;
        send.finish().await.map_err(map_io_error)?;
        Ok(recv)
    }
}
