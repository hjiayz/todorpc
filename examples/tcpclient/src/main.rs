use define::*;
use todorpc_client_tcp::{async_call, subscribe, TcpClient};
use tokio::time::{delay_for, Duration};
use tokio::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stream = net::TcpStream::connect("127.0.0.1:8081").await?;
    let client = TcpClient::connect(stream).await;
    subscribe(Bar, &client, |res| {
        match res {
            Ok((s, i)) => println!("bar: {} {}", s, i),
            Err(e) => println!("foo: {:?}", e),
        }
        false
    });
    let mut i = 0;
    loop {
        //delay_for(Duration::from_millis(1000)).await;
        match async_call(Foo(i), &client).await {
            Ok(val) => println!("foo: {}", val),
            Err(e) => {
                println!("foo: {:?}", e);
                break;
            }
        };
        i += 1;
    }
    Ok(())
}
