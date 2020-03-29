use define::*;
use std::result::Result as StdResult;
use todorpc::*;
use todorpc_server_tcp::TcpRPCServer;
use todorpc_server_websocket::*;

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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
    let chan2 = chan.clone();
    tokio::spawn(async move {
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:8081")
            .await
            .unwrap();
        TcpRPCServer::new(chan2, listener2).run().await;
    });
    WSRPCServer::new(chan, listener).run().await;
    Ok(())
}
