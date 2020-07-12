use define::*;
use std::time::Duration;
use todorpc_client_tcp::TcpClient;
use tokio::stream::StreamExt;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::delay_for;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client =
        TcpClient::connect("127.0.0.1:8081".parse().unwrap(), Duration::from_secs(5)).await;
    let client2 = client.clone();
    tokio::spawn(async move {
        let mut stream = client2.subscribe(Bar).await.unwrap();
        while let Some(res) = stream.next().await {
            match res {
                Ok((s, i)) => println!("bar: {} {}", s, i),
                Err(e) => println!("bar: {:?}", e),
            }
        }
    });

    let client2 = client.clone();
    tokio::spawn(async move {
        loop {
            let (tx, rx) = unbounded_channel();
            tokio::spawn(async move {
                for i in 0..10 {
                    tx.send(format!("hi {}", i)).unwrap();
                    delay_for(Duration::from_millis(1000)).await;
                }
            });
            let _ = client2.upload(UploadSample, rx).await;
        }
    });

    let mut i = 0;
    loop {
        delay_for(Duration::from_millis(1000)).await;
        match client.call(Foo(i)).await {
            Ok(val) => println!("foo: {}", val),
            Err(e) => {
                println!("foo: {:?}", e);
            }
        };
        i += 1;
    }
}
