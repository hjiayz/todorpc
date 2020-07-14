use define::*;
use log::{error, info};
use native_tls::{Identity, TlsAcceptor};
use pretty_env_logger::formatted_builder;
use std::sync::Arc;
use todorpc::*;
use todorpc_server_tcp::TcpRPCServer;
use todorpc_server_websocket::*;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::stream::StreamExt;

async fn foo(res: Result<Foo>, _ctx: Context) -> u32 {
    match res {
        Err(_) => 0,
        Ok(val) => val.0,
    }
}

async fn bar(res: Result<Bar>, ctx: ContextWithSender<(String, u32)>) {
    res.unwrap();
    use tokio::time::{delay_for, Duration};
    let mut i = 0u32;
    loop {
        delay_for(Duration::from_millis(1000)).await;
        i += 1;
        if let Err(e) = ctx.send(&(i.to_string(), i)) {
            error!("send failed: {:?}", e);
            break;
        };
    }
}

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::SeqCst;

static UPLOAD_ID: AtomicU64 = AtomicU64::new(0);

async fn upload_sample(res: Result<UploadSample>, ctx: ContextWithUpload<String>) {
    let id = UPLOAD_ID.fetch_add(1, SeqCst);
    res.unwrap();
    let info = ctx.connection_info();
    let remote_address = &info.remote_address();
    let protocol = info.protocol();
    let mut stream = ctx.into_stream();
    while let Some(s) = stream.next().await {
        println!("{}://{} {} : {}", protocol, remote_address, id, s.unwrap());
    }
    println!("upload sample finished");
}

async fn listen_quic(chan: Arc<Channels>, identity: &[u8]) {
    use quinn::{Certificate, CertificateChain, Endpoint, PrivateKey, ServerConfigBuilder};
    use std::net::SocketAddr;
    use std::str::FromStr;
    use todorpc_server_quic::QuicRPCServer;
    let mut server_config = ServerConfigBuilder::default();
    let pfx = p12::PFX::parse(identity).unwrap();
    let mut x509s = pfx.cert_x509_bags("changeit").unwrap();
    x509s.pop();
    let cert = x509s.pop().unwrap();
    let key = pfx.key_bags("changeit").unwrap().pop().unwrap();
    let cert_chain = CertificateChain::from_certs(Certificate::from_der(&cert));
    let key = PrivateKey::from_der(&key).unwrap();
    server_config.certificate(cert_chain, key).unwrap();
    let mut endpoint = Endpoint::builder();
    endpoint.listen(server_config.build());

    let incoming = {
        let (endpoint, incoming) = endpoint
            .bind(&SocketAddr::from_str("127.0.0.1:8084").unwrap())
            .unwrap();
        info!("quic://{} started", endpoint.local_addr().unwrap());
        incoming
    };
    QuicRPCServer::new(chan, incoming).run().await
}

async fn listen_tcp(chan: Arc<Channels>) {
    let listener1 = tokio::net::TcpListener::bind("127.0.0.1:8081")
        .await
        .unwrap();
    info!("tcp://127.0.0.1:8081 started");
    TcpRPCServer::new(chan, listener1).run().await;
}

async fn listen_wssrpc(chan: Arc<Channels>, identity: &[u8]) {
    let listener2 = tokio::net::TcpListener::bind("127.0.0.1:8082")
        .await
        .unwrap();
    let identity = Identity::from_pkcs12(&identity, "changeit").unwrap();
    let tls = TlsAcceptor::new(identity).unwrap();
    info!("wss://localhost:8082 started");
    WSRPCServer::new_tls(chan, listener2, tls).run().await;
}

async fn listen_wsrpc(chan: Arc<Channels>) {
    let listener0 = tokio::net::TcpListener::bind("127.0.0.1:8080")
        .await
        .unwrap();
    info!("ws://localhost:8080 started");
    WSRPCServer::new(chan, listener0).run().await;
}

#[tokio::main]
async fn main() {
    formatted_builder()
        .filter_level(log::LevelFilter::Debug)
        .init();
    let mut file = File::open("localhost.p12")
        .await
        .or(File::open("./examples/server/localhost.p12").await)
        .unwrap();
    let mut identity = vec![];
    file.read_to_end(&mut identity).await.unwrap();

    let identity2 = identity.clone();

    let mut chan = Channels::new();
    chan = chan.set_call(foo);
    chan = chan.set_subscribe(bar);
    chan = chan.set_upload(upload_sample);
    let chan = chan.finish();

    let chan_cloned = chan.clone();
    tokio::spawn(async move { listen_tcp(chan_cloned).await });

    let chan_cloned = chan.clone();
    tokio::spawn(async move { listen_wssrpc(chan_cloned, &identity).await });

    let chan_cloned = chan.clone();
    tokio::spawn(async move { listen_quic(chan_cloned, &identity2).await });

    listen_wsrpc(chan).await;
}
