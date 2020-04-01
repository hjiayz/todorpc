use define::*;
use native_tls::{Identity, TlsAcceptor};
use std::result::Result as StdResult;
use todorpc::*;
use todorpc_server_tcp::TcpRPCServer;
use todorpc_server_websocket::*;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

async fn foo(res: Result<Foo>, _ctx: Context) -> u32 {
    match res {
        Err(_) => 0,
        Ok(val) => val.0 % 20,
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
            println!("send faild: {:?}", e);
            break;
        };
    }
}

#[tokio::main]
async fn main() -> StdResult<(), Box<dyn std::error::Error>> {
    let mut chan = Channels::new();
    chan = chan.set_call(foo);
    chan = chan.set_subscribe(bar);
    let chan = chan.finish();

    let chan2 = chan.clone();
    tokio::spawn(async move {
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:8081")
            .await
            .unwrap();
        println!("tcp://127.0.0.1:8081 started");
        TcpRPCServer::new(chan2, listener2).run().await;
    });

    let chan3 = chan.clone();
    tokio::spawn(async move {
        let listener3 = tokio::net::TcpListener::bind("127.0.0.1:8082")
            .await
            .unwrap();
        let mut file = File::open("localhost.p12").await.unwrap();
        let mut identity = vec![];
        file.read_to_end(&mut identity).await.unwrap();
        let identity = Identity::from_pkcs12(&identity, "changeit").unwrap();
        let tls = TlsAcceptor::new(identity).unwrap();
        println!("wss://localhost:8082 started");
        WSRPCServer::new_tls(chan3, listener3, tls).run().await;
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
    println!("ws://localhost:8080 started");
    WSRPCServer::new(chan, listener).run().await;

    Ok(())
}
