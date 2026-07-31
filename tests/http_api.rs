use std::{path::PathBuf, time::Duration};

use bitcoin::Amount;
use nigiri_rs::{Bitcoin, Liquid, NigiriClient, NigiriConfig, NigiriError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

async fn one_shot_server(status: &str, body: String) -> (Url, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let status = status.to_owned();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).await.unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
            let header_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4);
            if let Some(header_end) = header_end {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + content_length {
                    break;
                }
            }
        }
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8(request).unwrap()
    });
    (Url::parse(&format!("http://{address}/")).unwrap(), task)
}

fn config(base: Url) -> NigiriConfig {
    NigiriConfig {
        chopsticks_url: base.clone(),
        esplora_url: base,
        executable: PathBuf::from("nigiri"),
        timeout: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn block_height_parses_a_bounded_success_body() {
    let (base, request) = one_shot_server("200 OK", "123\n".to_owned()).await;
    let client = NigiriClient::<Bitcoin>::with_config(config(base)).unwrap();

    assert_eq!(client.block_height().await.unwrap(), 123);
    assert!(
        request
            .await
            .unwrap()
            .starts_with("GET /blocks/tip/height ")
    );
}

#[tokio::test]
async fn malformed_height_is_an_invalid_response_without_the_body() {
    let secret = "not-a-height-caller-secret";
    let (base, _) = one_shot_server("200 OK", secret.to_owned()).await;
    let client = NigiriClient::<Liquid>::with_config(config(base)).unwrap();

    let error = client.block_height().await.unwrap_err();
    assert!(matches!(
        error,
        NigiriError::InvalidResponse {
            operation: "block height",
            ..
        }
    ));
    assert!(!error.to_string().contains(secret));
}

#[tokio::test]
async fn status_bodies_are_bounded() {
    let (base, _) = one_shot_server("500 Internal Server Error", "x".repeat(100_000)).await;
    let client = NigiriClient::<Bitcoin>::with_config(config(base)).unwrap();

    let error = client.block_height().await.unwrap_err();
    let NigiriError::HttpStatus { body, .. } = error else {
        panic!("expected HTTP status error");
    };
    assert!(body.len() <= 65_550, "retained {} bytes", body.len());
}

#[tokio::test]
async fn status_errors_redact_caller_address_material() {
    let address = "bcrt1qcaller-address-material";
    let (base, _) = one_shot_server(
        "400 Bad Request",
        format!("faucet rejected address {address}"),
    )
    .await;
    let client = NigiriClient::<Bitcoin>::with_config(config(base)).unwrap();

    let error = client.faucet(address, None).await.unwrap_err();

    let NigiriError::HttpStatus { body, .. } = error else {
        panic!("expected HTTP status error");
    };
    assert!(!body.contains(address));
}

#[tokio::test]
async fn faucet_serializes_one_satoshi_as_an_exact_btc_decimal() {
    let txid = "44".repeat(32);
    let (base, request) = one_shot_server("200 OK", format!(r#"{{"txId":"{txid}"}}"#)).await;
    let client = NigiriClient::<Bitcoin>::with_config(config(base)).unwrap();

    let parsed = client
        .faucet("bcrt1qfixture", Some(Amount::from_sat(1)))
        .await
        .unwrap();

    assert_eq!(parsed.to_string(), txid);
    let request = request.await.unwrap();
    assert!(request.starts_with("POST /faucet "));
    assert!(request.contains(r#""amount":0.00000001"#));
    assert!(!request.contains("1e-8"));
}
