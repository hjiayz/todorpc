use define::*;
use std::time::Duration;
use todorpc_client_tcp::TcpClient;
use tokio::stream::StreamExt;
use tokio::time::delay_for;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client =
        TcpClient::connect("127.0.0.1:8081".parse().unwrap(), Duration::from_secs(5)).await;
    let client2 = client.clone();
    tokio::spawn(async move {
        let mut stream = client.subscribe(Bar).await.unwrap();
        while let Some(res) = stream.next().await {
            match res {
                Ok((s, i)) => println!("bar: {} {}", s, i),
                Err(e) => println!("bar: {:?}", e),
            }
        }
    });
    let mut i = 0;
    loop {
        delay_for(Duration::from_millis(1000)).await;
        match client2.call(Foo(i)).await {
            Ok(val) => println!("foo: {}", val),
            Err(e) => {
                println!("foo: {:?}", e);
            }
        };
        i += 1;
    }
}
