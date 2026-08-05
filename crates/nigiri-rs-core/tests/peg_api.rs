//! RPC-shape tests for the peg coordinator, against scripted mock servers.
//!
//! These need no Docker. They assert the exact method names and parameter vectors sent to each
//! chain, which is what a live node would reject if wrong.

use std::time::Duration;

use nigiri_rs_core::{Bitcoin, Liquid, NigiriClient, NigiriConfig, NigiriError, Peg};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

const REGTEST_GENESIS: &str = "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206";

/// Serves one scripted response per connection and returns every request body it parsed.
async fn scripted_server(
    responses: Vec<(&'static str, String)>,
) -> (Url, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(responses.len());
        for (status, body) in responses {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .expect("a scripted request arrives")
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
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
                        let payload = &request[header_end..header_end + content_length];
                        requests.push(serde_json::from_slice(payload).unwrap());
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        requests
    });
    (Url::parse(&format!("http://{address}/")).unwrap(), task)
}

fn ok(result: Value) -> (&'static str, String) {
    (
        "200 OK",
        json!({"result": result, "error": null, "id": "nigiri-rs"}).to_string(),
    )
}

fn client<N: nigiri_rs_core::NigiriNetwork>(node_rpc_url: Url) -> NigiriClient<N> {
    NigiriClient::with_config(NigiriConfig {
        node_rpc_url,
        timeout: Duration::from_secs(2),
        ..Default::default()
    })
    .unwrap()
}

fn sidechain_info(parent: &str, depth: u64) -> Value {
    json!({
        "parent_blockhash": parent,
        "pegin_confirmation_depth": depth,
        "enforce_pak": false,
    })
}

// Catches a regression that stops verifying the pair, which would let a consumer build a Peg from
// two fixtures that have never heard of each other and get confusing failures later.
#[tokio::test]
async fn connect_accepts_a_matching_pair_and_records_the_depth() {
    let (liquid_url, liquid_requests) =
        scripted_server(vec![ok(sidechain_info(REGTEST_GENESIS, 8))]).await;
    let (bitcoin_url, bitcoin_requests) =
        scripted_server(vec![ok(Value::String(REGTEST_GENESIS.to_owned()))]).await;

    let peg = Peg::connect(client::<Bitcoin>(bitcoin_url), client::<Liquid>(liquid_url))
        .await
        .expect("a matching pair connects");

    assert_eq!(peg.pegin_confirmation_depth(), 8);

    let liquid_requests = liquid_requests.await.unwrap();
    assert_eq!(liquid_requests[0]["method"], "getsidechaininfo");
    assert_eq!(liquid_requests[0]["params"], json!([]));

    let bitcoin_requests = bitcoin_requests.await.unwrap();
    assert_eq!(bitcoin_requests[0]["method"], "getblockhash");
    assert_eq!(bitcoin_requests[0]["params"], json!([0]));
}

// Catches a regression that pairs a Liquid node with a Bitcoin node it does not treat as its
// parent chain. Every peg call would then fail deep inside claimpegin instead of here.
#[tokio::test]
async fn connect_rejects_a_mismatched_parent() {
    let other_genesis = "11".repeat(32);
    let (liquid_url, _) = scripted_server(vec![ok(sidechain_info(REGTEST_GENESIS, 8))]).await;
    let (bitcoin_url, _) = scripted_server(vec![ok(Value::String(other_genesis))]).await;

    let error = Peg::connect(client::<Bitcoin>(bitcoin_url), client::<Liquid>(liquid_url))
        .await
        .expect_err("a mismatched parent must be rejected");

    let NigiriError::PegNotConfigured { detail } = &error else {
        panic!("expected PegNotConfigured, got {error}");
    };
    assert!(detail.contains("parent"), "unhelpful detail: {detail}");
}
