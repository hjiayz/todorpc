use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use todorpc::*;
pub use todorpc_client_core::{async_scall as async_call, scall as call, ssubscribe as subscribe};
use todorpc_client_core::{Connect, ConnectSend};
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncRead, AsyncReadExt, Error as IoError};
use tokio::net::TcpStream;
use tokio::stream::StreamExt;
use tokio::sync::mpsc;
use tokio::time::{delay_for, Duration};

fn map_io_err(src: IoError) -> Error {
    Error::IoError(format!("{:?}", src))
}

async fn read_msg_async<R: AsyncRead + Unpin>(n: &mut R) -> Result<Response> {
    use std::mem::transmute;
    let mut h = [0u8; 12];
    n.read_exact(&mut h).await.map_err(map_io_err)?;
    let (len_buf, msg_id_buf): ([u8; 8], [u8; 4]) = unsafe { transmute(h) };
    let len = u64::from_be_bytes(len_buf) as usize;
    let msg_id = u32::from_be_bytes(msg_id_buf);

    let mut msg_bytes = Vec::with_capacity(len);
    unsafe {
        msg_bytes.set_len(len);
    };
    n.read_exact(&mut msg_bytes).await.map_err(map_io_err)?;
    let msg = msg_bytes;
    Ok(Response { msg_id, msg })
}

struct Inner {
    on_msgs: Mutex<BTreeMap<u32, Box<dyn Fn(Result<Vec<u8>>) -> bool + Send>>>,
    is_connected: AtomicBool,
    next_id: AtomicU32,
}

#[derive(Clone)]
pub struct TcpClient {
    sender: mpsc::UnboundedSender<Vec<u8>>,
    inner: Arc<Inner>,
}

impl TcpClient {
    pub async fn connect(stream: TcpStream) -> TcpClient {
        let (mut read, mut write) = tokio::io::split(stream);
        let on_msgs: Mutex<BTreeMap<u32, Box<dyn Fn(Result<Vec<u8>>) -> bool + Send>>> =
            Mutex::new(BTreeMap::new());
        let (sender, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let inner = Arc::new(Inner {
            on_msgs,
            is_connected: AtomicBool::new(true),
            next_id: AtomicU32::new(0),
        });
        let client = TcpClient { sender, inner };
        let inner2 = client.inner.clone();
        tokio::spawn(async move {
            loop {
                if !inner2.is_connected.load(Ordering::SeqCst) {
                    break;
                }
                let bytes = match rx.next().await {
                    Some(bytes) => bytes,
                    None => break,
                };
                if let Err(e) = write.write_all(&bytes).await {
                    println!("{:?}", e);
                    break;
                }
            }

            let lock = loop {
                if let Ok(lock) = inner2.on_msgs.try_lock() {
                    break lock;
                }
                delay_for(Duration::from_millis(0)).await;
            };

            for f in lock.values() {
                f(Err(Error::ChannelClosed));
            }
            drop(lock);
            inner2.is_connected.store(false, Ordering::SeqCst);
        });
        let inner3 = client.inner.clone();
        tokio::spawn(async move {
            loop {
                if !inner3.is_connected.load(Ordering::SeqCst) {
                    break;
                }
                let msg = match read_msg_async(&mut read).await {
                    Err(e) => {
                        println!("{:?}", e);
                        break;
                    }
                    Ok(msg) => msg,
                };

                let mut lock = loop {
                    if let Ok(lock) = inner3.on_msgs.try_lock() {
                        break lock;
                    }
                    delay_for(Duration::from_millis(0)).await;
                };

                let f = match lock.get_mut(&msg.msg_id) {
                    None => {
                        println!("msg_id {:?} miss", msg.msg_id);
                        break;
                    }
                    Some(f) => f,
                };
                if f(Ok(msg.msg)) {
                    lock.remove(&msg.msg_id);
                }
            }
            inner3.is_connected.store(false, Ordering::SeqCst);
        });
        client
    }
}

impl Connect<Box<dyn Fn(Result<Vec<u8>>) -> bool + 'static + Send>> for TcpClient {
    fn is_connected(&self) -> bool {
        self.inner.is_connected.load(Ordering::SeqCst)
    }
    fn send<F: FnOnce(&Self, Result<()>) + 'static + Send>(&self, bytes: &[u8], cb: F) {
        if let Err(_) = self.sender.send(bytes.to_owned()) {
            cb(self, Err(Error::ChannelClosed))
        };
    }
    fn on_msg<F2: FnOnce(&Self, u32) + 'static + Send>(
        &self,
        f: Box<dyn Fn(Result<Vec<u8>>) -> bool + 'static + Send>,
        cb: F2,
    ) {
        let client = self.clone();
        tokio::task::spawn(async move {
            let mut lock = loop {
                if let Ok(lock) = client.inner.on_msgs.try_lock() {
                    break lock;
                }
                delay_for(Duration::from_millis(0)).await;
            };
            let next_id = client.inner.next_id.fetch_add(1, Ordering::SeqCst);
            if next_id == u32::max_value() {
                client.inner.is_connected.store(false, Ordering::SeqCst);
            }
            lock.insert(next_id, f);
            drop(lock);
            cb(&client, next_id);
        });
    }
    fn close_msg_handle<
        F: FnOnce(Option<Box<dyn Fn(Result<Vec<u8>>) -> bool + 'static + Send>>) + Send + 'static,
    >(
        &self,
        msg_id: u32,
        cb: F,
    ) {
        let client = self.clone();
        tokio::task::spawn(async move {
            let mut lock = loop {
                if let Ok(lock) = client.inner.on_msgs.try_lock() {
                    break lock;
                }
                delay_for(Duration::from_millis(0)).await;
            };
            let f = lock.remove(&msg_id);
            drop(lock);
            cb(f);
        });
    }
}

impl ConnectSend for TcpClient {}
