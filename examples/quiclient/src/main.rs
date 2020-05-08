use define::*;
use pretty_env_logger::formatted_builder;
use quinn::CertificateChain;
use todorpc_client_quic::QuicClient;
use tokio::fs;
use tokio::stream::StreamExt;
use tokio::time::{delay_for, Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    formatted_builder()
        .filter_level(log::LevelFilter::Debug)
        .init();
    let mut endpoint = quinn::Endpoint::builder();
    let mut client_config = quinn::ClientConfigBuilder::default();
    let chain: CertificateChain =
        quinn::CertificateChain::from_pem(&fs::read(&"../server/ca.pem").await.unwrap()).unwrap();
    let cert = chain.into_iter().next().unwrap();
    client_config
        .add_certificate_authority(cert.into())
        .unwrap();
    endpoint.default_client_config(client_config.build());
    let client = QuicClient::connect(
        5000,
        "127.0.0.1:8084".parse().unwrap(),
        "localhost",
        endpoint,
    )
    .unwrap();
    let client2 = client.clone();

    tokio::spawn(async move {
        let mut stream = client.subscribe(Bar).await.unwrap();
        while let Some(res) = stream.next().await {
            match res {
                Ok((s, i)) => println!("bar: {} {}", s, i),
                Err(e) => {
                    println!("bar: {:?}", e);
                }
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
