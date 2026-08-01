use std::time::Duration;

use nigiri_rs::{Bitcoin, Liquid, NigiriClient, NigiriConfig, NigiriError};
use serde::Deserialize;
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

fn client<N: nigiri_rs::NigiriNetwork>(node_rpc_url: Url) -> NigiriClient<N> {
    NigiriClient::with_config(NigiriConfig {
        node_rpc_url,
        timeout: Duration::from_secs(2),
        ..Default::default()
    })
    .unwrap()
}

#[derive(Debug, Deserialize)]
struct LiquidBlockchainInfo {
    chain: String,
    blocks: i64,
    bestblockhash: elements::BlockHash,
}

#[tokio::test]
async fn public_rpc_sends_typed_json_params_and_deserializes_native_hashes() {
    let hash = "1111111111111111111111111111111111111111111111111111111111111111";
    let (url, request) = one_shot_server(
        "200 OK",
        format!(r#"{{"result":"{hash}","error":null,"id":"nigiri-rs"}}"#),
    )
    .await;
    let client = client::<Bitcoin>(url);

    let parsed: bitcoin::BlockHash = client.rpc("getblockhash", (100_u64,)).await.unwrap();

    assert_eq!(parsed.to_string(), hash);
    let request = request.await.unwrap();
    assert!(request.contains(r#""method":"getblockhash""#));
    assert!(request.contains(r#""params":[100]"#));
}

#[tokio::test]
async fn liquid_rpc_uses_caller_record_with_native_elements_hash() {
    let (url, _) = one_shot_server(
        "200 OK",
        r#"{"result":{"chain":"regtest","blocks":101,"bestblockhash":"5555555555555555555555555555555555555555555555555555555555555555"},"error":null,"id":"nigiri-rs"}"#.to_owned(),
    )
    .await;
    let client = client::<Liquid>(url);
    let info: LiquidBlockchainInfo = client.rpc("getblockchaininfo", ()).await.unwrap();

    assert_eq!(info.chain, "regtest");
    assert_eq!(info.blocks, 101);
    assert_eq!(
        info.bestblockhash.to_string(),
        "5555555555555555555555555555555555555555555555555555555555555555"
    );
}

#[cfg(feature = "bitcoin-rpc-types")]
#[tokio::test]
async fn bitcoin_rpc_uses_reexported_core_v30_response() {
    let (url, _) = one_shot_server(
        "200 OK",
        r#"{"result":{"chain":"regtest","blocks":101,"headers":101,"bestblockhash":"1111111111111111111111111111111111111111111111111111111111111111","bits":"207fffff","target":"7fffff0000000000000000000000000000000000000000000000000000000000","difficulty":1.0,"time":0,"mediantime":0,"verificationprogress":1.0,"initialblockdownload":false,"chainwork":"00","size_on_disk":0,"pruned":false,"warnings":[]},"error":null,"id":"nigiri-rs"}"#.to_owned(),
    )
    .await;
    let client = client::<Bitcoin>(url);
    let info: nigiri_rs::bitcoin_rpc_types::v30::GetBlockchainInfo =
        client.rpc("getblockchaininfo", ()).await.unwrap();

    assert_eq!(info.chain, "regtest");
    assert_eq!(info.blocks, 101);
    assert!(!info.initial_block_download);
}

#[tokio::test]
async fn public_rpc_rejects_an_invalid_runtime_method_before_transport() {
    let client = client::<Bitcoin>(Url::parse("http://127.0.0.1:1/").unwrap());

    let error = client.rpc::<(), _>("invalid-method", ()).await.unwrap_err();

    assert!(matches!(error, NigiriError::InvalidRequest { .. }));
}
