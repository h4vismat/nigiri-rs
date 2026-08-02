use std::time::Duration;

use bitcoin::Amount;
use nigiri_rs::{
    Bitcoin, DEFAULT_MAX_RESPONSE_BYTES, Liquid, NigiriClient, NigiriConfig, NigiriError,
};
use serde_json::Value;
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

async fn sequential_rpc_server(
    responses: Vec<(&'static str, String)>,
) -> (Url, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(responses.len());
        for (status, body) in responses {
            let request_number = requests.len() + 1;
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .unwrap_or_else(|_| {
                    panic!("timed out waiting for JSON-RPC request {request_number}")
                })
                .unwrap();
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
                        let body = &request[header_end..header_end + content_length];
                        requests.push(serde_json::from_slice(body).unwrap());
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

fn rpc_response(result: Value) -> String {
    serde_json::json!({"result": result, "error": null, "id": "nigiri-rs"}).to_string()
}

const SEND_TXID: &str = "4444444444444444444444444444444444444444444444444444444444444444";
const BLOCK_HASH: &str = "5555555555555555555555555555555555555555555555555555555555555555";
const BITCOIN_MINING_ADDRESS: &str = "mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn";
const LIQUID_MINING_ADDRESS: &str = "ert1qwhh2n5qypypm0eufahm2pvj8raj9zq5c27cysu";

fn config(base: Url) -> NigiriConfig {
    NigiriConfig {
        chopsticks_url: base.clone(),
        esplora_url: base,
        timeout: Duration::from_secs(2),
        max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        ..Default::default()
    }
}

#[tokio::test]
async fn configured_response_limit_rejects_oversized_esplora_success() {
    let (base, _) = one_shot_server("200 OK", "123456789".to_owned()).await;
    let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
        chopsticks_url: base.clone(),
        esplora_url: base,
        max_response_bytes: 8,
        ..Default::default()
    })
    .unwrap();

    assert!(matches!(
        client.block_height().await.unwrap_err(),
        NigiriError::InvalidResponse { .. }
    ));
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
        NigiriError::InvalidResponse { ref operation, .. }
            if operation.as_ref() == "block height"
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
async fn bitcoin_faucet_sends_exact_amount_then_mines_one_block() {
    let (url, requests) = sequential_rpc_server(vec![
        ("200 OK", rpc_response(serde_json::json!(SEND_TXID))),
        (
            "200 OK",
            rpc_response(serde_json::json!(BITCOIN_MINING_ADDRESS)),
        ),
        ("200 OK", rpc_response(serde_json::json!([BLOCK_HASH]))),
    ])
    .await;
    let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
        node_rpc_url: url,
        timeout: Duration::from_secs(2),
        ..Default::default()
    })
    .unwrap();

    let txid = client
        .faucet("bcrt1qfixture", Some(Amount::from_sat(1)))
        .await
        .unwrap();
    assert_eq!(txid.to_string(), SEND_TXID);

    let requests = requests.await.unwrap();
    assert_eq!(requests[0]["method"], "sendtoaddress");
    assert_eq!(
        requests[0]["params"],
        serde_json::from_str::<Value>(r#"["bcrt1qfixture",0.00000001]"#).unwrap()
    );
    assert_eq!(requests[1]["method"], "getnewaddress");
    assert_eq!(requests[2]["method"], "generatetoaddress");
    assert_eq!(
        requests[2]["params"],
        serde_json::json!([1, BITCOIN_MINING_ADDRESS])
    );
}

#[tokio::test]
async fn bitcoin_faucet_default_amount_is_a_native_json_number() {
    let (url, requests) = sequential_rpc_server(vec![
        ("200 OK", rpc_response(serde_json::json!(SEND_TXID))),
        (
            "200 OK",
            rpc_response(serde_json::json!(BITCOIN_MINING_ADDRESS)),
        ),
        ("200 OK", rpc_response(serde_json::json!([BLOCK_HASH]))),
    ])
    .await;
    let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
        node_rpc_url: url,
        timeout: Duration::from_secs(2),
        ..Default::default()
    })
    .unwrap();

    client.faucet("bcrt1qfixture", None).await.unwrap();

    let requests = requests.await.unwrap();
    let params = requests[0]["params"].as_array().unwrap();
    assert_eq!(params.len(), 2);
    assert!(params[1].is_number());
    assert_eq!(params[1].as_f64(), Some(1.0));
}

#[tokio::test]
async fn liquid_faucet_sends_native_vector_then_mines_one_block() {
    let (url, requests) = sequential_rpc_server(vec![
        ("200 OK", rpc_response(serde_json::json!(SEND_TXID))),
        (
            "200 OK",
            rpc_response(serde_json::json!(LIQUID_MINING_ADDRESS)),
        ),
        ("200 OK", rpc_response(serde_json::json!([BLOCK_HASH]))),
    ])
    .await;
    let client = NigiriClient::<Liquid>::with_config(NigiriConfig {
        node_rpc_url: url,
        timeout: Duration::from_secs(2),
        ..Default::default()
    })
    .unwrap();

    let txid = client
        .faucet("ert1qdestination", Some(Amount::from_sat(1)))
        .await
        .unwrap();
    assert_eq!(txid.to_string(), SEND_TXID);

    let requests = requests.await.unwrap();
    assert_eq!(requests[0]["method"], "sendtoaddress");
    assert_eq!(
        requests[0]["params"],
        serde_json::from_str::<Value>(
            r#"["ert1qdestination",0.00000001,"","",false,false,1,"unset",false,""]"#
        )
        .unwrap()
    );
    assert_eq!(requests[1]["method"], "getnewaddress");
    assert_eq!(requests[2]["method"], "generatetoaddress");
    assert_eq!(
        requests[2]["params"],
        serde_json::json!([1, LIQUID_MINING_ADDRESS])
    );
}

#[tokio::test]
async fn faucet_preserves_committed_txid_when_confirmation_mining_fails() {
    let (url, requests) = sequential_rpc_server(vec![
        ("200 OK", rpc_response(serde_json::json!(SEND_TXID))),
        (
            "200 OK",
            serde_json::json!({
                "result": null,
                "error": {"code": -18, "message": "wallet unavailable"},
                "id": "nigiri-rs"
            })
            .to_string(),
        ),
    ])
    .await;
    let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
        node_rpc_url: url,
        timeout: Duration::from_secs(2),
        ..Default::default()
    })
    .unwrap();

    let error = client.faucet("bcrt1qfixture", None).await.unwrap_err();
    assert!(matches!(
        error,
        NigiriError::PostTransactionMiningFailed {
            ref txid,
            source,
            ..
        } if txid == SEND_TXID && matches!(*source, NigiriError::RpcFailed { code: -18, .. })
    ));

    let requests = requests.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["method"], "sendtoaddress");
    assert_eq!(requests[1]["method"], "getnewaddress");
}

#[tokio::test]
async fn broadcast_uses_node_rpc_then_mines_one_block() {
    let (url, requests) = sequential_rpc_server(vec![
        ("200 OK", rpc_response(serde_json::json!(SEND_TXID))),
        (
            "200 OK",
            rpc_response(serde_json::json!(BITCOIN_MINING_ADDRESS)),
        ),
        ("200 OK", rpc_response(serde_json::json!([BLOCK_HASH]))),
    ])
    .await;
    let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
        node_rpc_url: url,
        timeout: Duration::from_secs(2),
        ..Default::default()
    })
    .unwrap();

    let txid = client.broadcast_tx("02000000000100").await.unwrap();
    assert_eq!(txid.to_string(), SEND_TXID);

    let requests = requests.await.unwrap();
    assert_eq!(requests[0]["method"], "sendrawtransaction");
    assert_eq!(requests[0]["params"], serde_json::json!(["02000000000100"]));
    assert_eq!(requests[1]["method"], "getnewaddress");
    assert_eq!(requests[2]["method"], "generatetoaddress");
}

#[tokio::test]
async fn broadcast_preserves_committed_txid_when_confirmation_mining_fails() {
    let (url, requests) = sequential_rpc_server(vec![
        ("200 OK", rpc_response(serde_json::json!(SEND_TXID))),
        (
            "200 OK",
            rpc_response(serde_json::json!(BITCOIN_MINING_ADDRESS)),
        ),
        (
            "200 OK",
            serde_json::json!({
                "result": null,
                "error": {"code": -1, "message": "mining unavailable"},
                "id": "nigiri-rs"
            })
            .to_string(),
        ),
    ])
    .await;
    let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
        node_rpc_url: url,
        timeout: Duration::from_secs(2),
        ..Default::default()
    })
    .unwrap();

    let error = client.broadcast_tx("02000000000100").await.unwrap_err();
    assert!(matches!(
        error,
        NigiriError::PostTransactionMiningFailed {
            ref txid,
            source,
            ..
        } if txid == SEND_TXID && matches!(*source, NigiriError::RpcFailed { code: -1, .. })
    ));

    let requests = requests.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["method"], "sendrawtransaction");
    assert_eq!(requests[1]["method"], "getnewaddress");
    assert_eq!(requests[2]["method"], "generatetoaddress");
}
