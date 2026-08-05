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

/// Connects a Peg against two servers whose first scripted response is the pair check.
async fn connected_peg(
    mut liquid: Vec<(&'static str, String)>,
    mut bitcoin: Vec<(&'static str, String)>,
) -> (
    Peg,
    tokio::task::JoinHandle<Vec<Value>>,
    tokio::task::JoinHandle<Vec<Value>>,
) {
    liquid.insert(0, ok(sidechain_info(REGTEST_GENESIS, 8)));
    bitcoin.insert(0, ok(Value::String(REGTEST_GENESIS.to_owned())));

    let (liquid_url, liquid_requests) = scripted_server(liquid).await;
    let (bitcoin_url, bitcoin_requests) = scripted_server(bitcoin).await;

    let peg = Peg::connect(client::<Bitcoin>(bitcoin_url), client::<Liquid>(liquid_url))
        .await
        .expect("the scripted pair connects");

    (peg, liquid_requests, bitcoin_requests)
}

const MAINCHAIN_ADDRESS: &str = "bcrt1qwamhwamhwamhwamhwamhwamhwamhwamhwamhwamhwamhwamhwamsyzj6cv";
const CLAIM_SCRIPT: &str = "0014389ffce9cd9ae88dcc0631e88a821ffdbe9bfe26";
const MAINCHAIN_TXID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const CLAIM_TXID: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const RAW_TX_HEX: &str = "02000000000101aabb";
const PROOF_HEX: &str = "0000002011223344";

// Catches a regression that stops asking the Liquid node for a peg-in address, or that mangles
// the two fields it returns.
#[tokio::test]
async fn peg_in_request_returns_the_address_and_claim_script() {
    let (peg, liquid_requests, _bitcoin) = connected_peg(
        vec![ok(json!({
            "mainchain_address": MAINCHAIN_ADDRESS,
            "claim_script": CLAIM_SCRIPT,
        }))],
        vec![],
    )
    .await;

    let request = peg
        .peg_in_request()
        .await
        .expect("a peg-in address is issued");

    assert_eq!(request.mainchain_address.to_string(), MAINCHAIN_ADDRESS);
    assert_eq!(request.claim_script, CLAIM_SCRIPT);

    let liquid_requests = liquid_requests.await.unwrap();
    assert_eq!(liquid_requests[1]["method"], "getpeginaddress");
    assert_eq!(liquid_requests[1]["params"], json!([]));
}

// Catches a regression in the claim vector: the wrong RPC, the wrong argument order, or a proof
// requested for the wrong transaction. A live node rejects all three.
#[tokio::test]
async fn claim_peg_in_sends_the_raw_transaction_and_its_proof() {
    let (peg, liquid_requests, bitcoin_requests) = connected_peg(
        vec![ok(Value::String(CLAIM_TXID.to_owned()))],
        vec![
            ok(json!({"hex": RAW_TX_HEX, "confirmations": 8})),
            ok(Value::String(PROOF_HEX.to_owned())),
        ],
    )
    .await;

    let mainchain_txid: bitcoin::Txid = MAINCHAIN_TXID.parse().unwrap();

    let claimed = peg
        .claim_peg_in(&mainchain_txid)
        .await
        .expect("a mature deposit claims");

    assert_eq!(claimed.to_string(), CLAIM_TXID);

    let bitcoin_requests = bitcoin_requests.await.unwrap();
    assert_eq!(bitcoin_requests[1]["method"], "getrawtransaction");
    assert_eq!(bitcoin_requests[1]["params"], json!([MAINCHAIN_TXID, true]));
    assert_eq!(bitcoin_requests[2]["method"], "gettxoutproof");
    assert_eq!(bitcoin_requests[2]["params"], json!([[MAINCHAIN_TXID]]));

    let liquid_requests = liquid_requests.await.unwrap();
    assert_eq!(liquid_requests[1]["method"], "claimpegin");
    assert_eq!(liquid_requests[1]["params"], json!([RAW_TX_HEX, PROOF_HEX]));
}

// Catches a regression that submits a claim before the deposit is mature. The node would reject
// it with an opaque message; this reports the two numbers the caller needs.
#[tokio::test]
async fn claim_peg_in_refuses_an_immature_deposit() {
    let (peg, _liquid, _bitcoin) = connected_peg(
        vec![],
        vec![ok(json!({"hex": RAW_TX_HEX, "confirmations": 3}))],
    )
    .await;

    let mainchain_txid: bitcoin::Txid = MAINCHAIN_TXID.parse().unwrap();
    let error = peg
        .claim_peg_in(&mainchain_txid)
        .await
        .expect_err("an immature deposit must be refused");

    assert!(matches!(
        error,
        NigiriError::PegInImmature { have: 3, need: 8 }
    ));
}
