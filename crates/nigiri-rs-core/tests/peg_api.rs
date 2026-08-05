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

/// Reads one HTTP request off a stream and returns its JSON body.
///
/// Shared by [`scripted_server`] and [`scripted_server_allowing_shortfall`], which differ only
/// in how long they wait for the connection to arrive in the first place.
async fn read_scripted_request(stream: &mut tokio::net::TcpStream) -> Value {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = stream.read(&mut buffer).await.unwrap();
        if count == 0 {
            panic!("connection closed before a full scripted request arrived");
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
                return serde_json::from_slice(payload).unwrap();
            }
        }
    }
}

/// Writes a scripted status and body as an HTTP response.
async fn write_scripted_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}

/// Serves one scripted response per connection and returns every request body it parsed.
///
/// Panics if a connection does not arrive within [`REQUIRED_CONNECTION_TIMEOUT`] of the previous
/// one completing: an under-provisioned test almost always means an assertion is wired to the
/// wrong request, and a loud failure here is more useful than a silent, truncated request list.
async fn scripted_server(
    responses: Vec<(&'static str, String)>,
) -> (Url, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(responses.len());
        for (status, body) in responses {
            let (mut stream, _) =
                tokio::time::timeout(REQUIRED_CONNECTION_TIMEOUT, listener.accept())
                    .await
                    .expect("a scripted request arrives")
                    .unwrap();
            requests.push(read_scripted_request(&mut stream).await);
            write_scripted_response(&mut stream, status, &body).await;
        }
        requests
    });
    (Url::parse(&format!("http://{address}/")).unwrap(), task)
}

/// How long [`scripted_server`] and the first `expected` slots of
/// [`scripted_server_allowing_shortfall`] wait for a connection.
///
/// Generous on purpose: this suite runs alongside Docker-backed tests under
/// `cargo test --workspace`, where CPU contention routinely causes multi-hundred-millisecond
/// scheduling delays on a loopback `accept()`. A slot a correct implementation is actually going
/// to use must tolerate that; only a slot it is expected to *decline* gets a short timeout.
const REQUIRED_CONNECTION_TIMEOUT: Duration = Duration::from_secs(3);

/// Like [`scripted_server`], but scripted responses at index `expected` and beyond that never
/// get a matching connection within `grace` are simply left unserved, returning whatever
/// requests arrived before then, rather than panicking.
///
/// The first `expected` slots still wait the full [`REQUIRED_CONNECTION_TIMEOUT`] — they are
/// connections a correct implementation really does make, and must not be flaky under load. Only
/// slots beyond `expected` use `grace`, because their whole purpose is to prove a scripted
/// response is deliberately left *unconsumed* by correct code. [`scripted_server`]'s exact-count
/// panic cannot express that — a test that scripts one more response than a correct
/// implementation ever asks for would otherwise hang for the full accept timeout and then fail
/// with a spurious `JoinError::Panic`, not the assertion the test is actually about. A short,
/// deliberate grace period on just the trailing slots converts that from "hangs and fails for
/// the wrong reason" into "resolves quickly with the right answer" — without also making the
/// required slots flaky, which an across-the-board short timeout would.
async fn scripted_server_allowing_shortfall(
    responses: Vec<(&'static str, String)>,
    expected: usize,
    grace: Duration,
) -> (Url, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(responses.len());
        for (index, (status, body)) in responses.into_iter().enumerate() {
            let timeout = if index < expected {
                REQUIRED_CONNECTION_TIMEOUT
            } else {
                grace
            };
            let Ok(Ok((mut stream, _))) = tokio::time::timeout(timeout, listener.accept()).await
            else {
                break;
            };
            requests.push(read_scripted_request(&mut stream).await);
            write_scripted_response(&mut stream, status, &body).await;
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

// Catches a regression that mines the wrong number of blocks before claiming. One block short and
// the claim is rejected; the arithmetic is off by one because faucet already mines one.
#[tokio::test]
async fn complete_peg_in_mines_to_the_reported_depth() {
    let mining_address = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";
    let (peg, liquid_requests, bitcoin_requests) = connected_peg(
        vec![
            ok(json!({
                "mainchain_address": MAINCHAIN_ADDRESS,
                "claim_script": CLAIM_SCRIPT,
            })),
            ok(Value::String(CLAIM_TXID.to_owned())),
        ],
        vec![
            // faucet: sendtoaddress, then getnewaddress + generatetoaddress for its own block.
            ok(Value::String(MAINCHAIN_TXID.to_owned())),
            ok(Value::String(mining_address.to_owned())),
            ok(json!([format!("{}", "ee".repeat(32))])),
            // complete_peg_in: an address to mine the remaining depth to.
            ok(Value::String(mining_address.to_owned())),
            ok(json!([format!("{}", "ff".repeat(32))])),
            // claim_peg_in: the deposit, then its proof.
            ok(json!({"hex": RAW_TX_HEX, "confirmations": 8})),
            ok(Value::String(PROOF_HEX.to_owned())),
        ],
    )
    .await;

    let pegged = peg
        .complete_peg_in(bitcoin::Amount::from_sat(100_000))
        .await
        .expect("a full peg-in completes");

    assert_eq!(pegged.mainchain_txid.to_string(), MAINCHAIN_TXID);
    assert_eq!(pegged.claim_txid.to_string(), CLAIM_TXID);
    assert_eq!(pegged.amount, bitcoin::Amount::from_sat(100_000));

    let bitcoin_requests = bitcoin_requests.await.unwrap();
    // Depth 8, one block already mined by faucet, so seven remain.
    let generates: Vec<&Value> = bitcoin_requests
        .iter()
        .filter(|request| request["method"] == "generatetoaddress")
        .collect();
    assert_eq!(generates.len(), 2);
    assert_eq!(generates[0]["params"][0], json!(1));
    assert_eq!(generates[1]["params"][0], json!(7));

    let liquid_requests = liquid_requests.await.unwrap();
    assert_eq!(liquid_requests[1]["method"], "getpeginaddress");
    assert_eq!(liquid_requests[2]["method"], "claimpegin");
}

// Catches a regression that gives up the first time the node says a deposit is not deep enough.
// The Liquid node's view of the mainchain lags the mainchain itself: against a real Elements node,
// a claim was rejected at exactly the reported depth of 8 and accepted at 11. Without the retry,
// one-shot peg-in is intermittently broken.
#[tokio::test]
async fn complete_peg_in_retries_while_the_node_lags_the_chain() {
    let mining_address = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";
    let not_deep_enough = (
        "500 Internal Server Error",
        json!({
            "result": null,
            "error": {"code": -8, "message": "needs more confirmations to be sent"},
            "id": "nigiri-rs",
        })
        .to_string(),
    );

    let (peg, liquid_requests, bitcoin_requests) = connected_peg(
        vec![
            ok(json!({
                "mainchain_address": MAINCHAIN_ADDRESS,
                "claim_script": CLAIM_SCRIPT,
            })),
            not_deep_enough.clone(),
            not_deep_enough,
            ok(Value::String(CLAIM_TXID.to_owned())),
        ],
        vec![
            // faucet: sendtoaddress, getnewaddress, generatetoaddress.
            ok(Value::String(MAINCHAIN_TXID.to_owned())),
            ok(Value::String(mining_address.to_owned())),
            ok(json!([format!("{}", "ee".repeat(32))])),
            // complete_peg_in: mining address, then the bulk mine to depth.
            ok(Value::String(mining_address.to_owned())),
            ok(json!([format!("{}", "ff".repeat(32))])),
            // fetched once, up front: the deposit and its proof cannot change once mature, so a
            // rejected claimpegin never asks the Bitcoin node for either again.
            ok(json!({"hex": RAW_TX_HEX, "confirmations": 8})),
            ok(Value::String(PROOF_HEX.to_owned())),
            // retry 1: one block, no re-fetch.
            ok(json!([format!("{}", "1a".repeat(32))])),
            // retry 2: one block, no re-fetch, then the resubmitted claim is accepted.
            ok(json!([format!("{}", "1b".repeat(32))])),
        ],
    )
    .await;

    let pegged = peg
        .complete_peg_in(bitcoin::Amount::from_sat(100_000))
        .await
        .expect("the claim succeeds once the node catches up");

    assert_eq!(pegged.claim_txid.to_string(), CLAIM_TXID);

    let bitcoin_requests = bitcoin_requests.await.unwrap();
    let generates: Vec<&Value> = bitcoin_requests
        .iter()
        .filter(|request| request["method"] == "generatetoaddress")
        .collect();
    // One from faucet, seven to reach depth, then one per retry.
    assert_eq!(generates.len(), 4);
    assert_eq!(generates[2]["params"][0], json!(1));
    assert_eq!(generates[3]["params"][0], json!(1));

    let liquid_requests = liquid_requests.await.unwrap();
    let claims = liquid_requests
        .iter()
        .filter(|request| request["method"] == "claimpegin")
        .count();
    assert_eq!(claims, 3);
}

// Catches a regression that retries a permanent failure — a transport error or a malformed
// response — as though it were the "not deep enough yet" case, burning the whole retry budget
// before the real error surfaces.
//
// This scripts one more bitcoin response than a correct implementation will ever ask for, so
// that a wrongly-retrying implementation both issues and gets served an extra `generatetoaddress`
// call, changing the count below from 2 to 3. Without that extra response, a wrongly-issued call
// would just be refused by an exhausted mock — indistinguishable, from the test's point of view,
// from a correct implementation that never made the call at all — which is exactly the gap the
// review round that added this test found. It uses `scripted_server_allowing_shortfall` instead
// of `scripted_server` (and so builds the pair directly rather than through `connected_peg`)
// because a correct implementation deliberately leaves that extra response unconsumed, and
// `scripted_server` treats an unconsumed response as a bug worth a panic.
#[tokio::test]
async fn complete_peg_in_does_not_retry_a_permanent_failure() {
    let mining_address = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";
    let grace = Duration::from_millis(300);

    // `expected` below is a count of required slots, not a byte offset: every slot at that index
    // or later gets the short `grace` timeout instead of `REQUIRED_CONNECTION_TIMEOUT`.
    let (liquid_url, liquid_requests) = scripted_server_allowing_shortfall(
        vec![
            ok(sidechain_info(REGTEST_GENESIS, 8)),
            ok(json!({
                "mainchain_address": MAINCHAIN_ADDRESS,
                "claim_script": CLAIM_SCRIPT,
            })),
        ],
        2, // both slots are required: no trap on the Liquid side.
        grace,
    )
    .await;
    let (bitcoin_url, bitcoin_requests) = scripted_server_allowing_shortfall(
        vec![
            ok(Value::String(REGTEST_GENESIS.to_owned())),
            // faucet: sendtoaddress, then getnewaddress + generatetoaddress for its own block.
            ok(Value::String(MAINCHAIN_TXID.to_owned())),
            ok(Value::String(mining_address.to_owned())),
            ok(json!([format!("{}", "ee".repeat(32))])),
            // complete_peg_in: an address to mine the remaining depth to.
            ok(Value::String(mining_address.to_owned())),
            ok(json!([format!("{}", "ff".repeat(32))])),
            // claim_peg_in: a deposit lookup that comes back malformed, not "not deep enough" —
            // a non-envelope body with a success status maps to NigiriError::InvalidResponse.
            // This is the 7th and last required slot.
            ("200 OK", "not JSON".to_owned()),
            // Deliberately scripted to be consumed only by a wrongly-retrying implementation. Do
            // not remove this thinking it is dead: without it, this test cannot tell a correct
            // fail-fast from the pre-fix bug, per the block comment above. It is slot index 7,
            // at or beyond `expected`, so it gets the short `grace` timeout, not the generous one.
            ok(json!([format!("{}", "1a".repeat(32))])),
        ],
        7, // seven required slots; the trailing generatetoaddress is the trap.
        grace,
    )
    .await;

    let peg = Peg::connect(client::<Bitcoin>(bitcoin_url), client::<Liquid>(liquid_url))
        .await
        .expect("the scripted pair connects");

    peg.complete_peg_in(bitcoin::Amount::from_sat(100_000))
        .await
        .expect_err("a malformed response must not be retried into a fake success");

    let bitcoin_requests = bitcoin_requests.await.unwrap();
    let generates: Vec<&Value> = bitcoin_requests
        .iter()
        .filter(|request| request["method"] == "generatetoaddress")
        .collect();
    // Only the two generatetoaddress calls that faucet and the initial mine-to-depth already
    // perform; the fix must not mine any further blocks chasing an unretryable error.
    assert_eq!(generates.len(), 2);

    let liquid_requests = liquid_requests.await.unwrap();
    assert!(
        liquid_requests
            .iter()
            .all(|request| request["method"] != "claimpegin"),
        "a permanent failure at the deposit lookup must never reach claimpegin"
    );
}

const PEG_OUT_TXID: &str = "abababababababababababababababababababababababababababababababab";
const RELEASE_TXID: &str = "babababababababababababababababababababababababababababababababa";
/// The peg-out output's `scriptPubKey.hex`, captured from a live Elements node and reproduced
/// byte-for-byte on a second, differently-pinned image.
const GOLDEN_PEG_OUT_SCRIPT: &str = "6a2006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f160014153a100bf13cf08f49d13163e49df5a51d186626";
/// The destination Bitcoin address that script pays, captured alongside it.
const GOLDEN_DESTINATION: &str = "bcrt1qz5apqzl38ncg7jw3x937f80455w3se3xfhd0f5";

/// Built by parsing rather than by `json!`, so the peg-out value stays an exact decimal literal.
/// `arbitrary_precision` preserves it; writing it as a Rust float would not.
fn peg_out_transaction(script_hex: &str, value_btc: &str) -> Value {
    let ordinary = "0014389ffce9cd9ae88dcc0631e88a821ffdbe9bfe26";
    serde_json::from_str(&format!(
        r#"{{"vout":[
            {{"value":0.5,"scriptPubKey":{{"hex":"{ordinary}"}}}},
            {{"value":{value_btc},"scriptPubKey":{{"hex":"{script_hex}"}}}}
        ]}}"#
    ))
    .expect("the scripted transaction is valid JSON")
}

// Catches a regression in the simulated federation: reading the destination out of the peg-out
// output is what makes a consumer's sendtomainchain genuinely verified. Take the destination from
// the caller instead and a malformed peg-out still pays.
#[tokio::test]
async fn release_peg_out_pays_the_decoded_destination() {
    let (peg, liquid_requests, bitcoin_requests) = connected_peg(
        vec![ok(peg_out_transaction(GOLDEN_PEG_OUT_SCRIPT, "0.00010000"))],
        vec![
            ok(Value::String(RELEASE_TXID.to_owned())),
            ok(Value::String(
                "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080".to_owned(),
            )),
            ok(json!([format!("{}", "cd".repeat(32))])),
        ],
    )
    .await;

    let liquid_txid: elements::Txid = PEG_OUT_TXID.parse().unwrap();
    let released = peg
        .release_peg_out(&liquid_txid)
        .await
        .expect("a valid peg-out is released");

    assert_eq!(released.destination.to_string(), GOLDEN_DESTINATION);
    assert_eq!(released.amount, bitcoin::Amount::from_sat(10_000));
    assert_eq!(released.bitcoin_txid.to_string(), RELEASE_TXID);

    let liquid_requests = liquid_requests.await.unwrap();
    assert_eq!(liquid_requests[1]["method"], "getrawtransaction");
    assert_eq!(liquid_requests[1]["params"], json!([PEG_OUT_TXID, 1]));

    let bitcoin_requests = bitcoin_requests.await.unwrap();
    assert_eq!(bitcoin_requests[1]["method"], "sendtoaddress");
    assert_eq!(bitcoin_requests[1]["params"][0], json!(GOLDEN_DESTINATION));
}

// Catches a regression that pays out against a transaction that never pegged out.
#[tokio::test]
async fn release_peg_out_rejects_a_transaction_with_no_peg_out() {
    let (peg, _liquid, _bitcoin) = connected_peg(
        vec![ok(json!({
            "vout": [
                { "value": 1.0, "scriptPubKey": { "hex": "0014389ffce9cd9ae88dcc0631e88a821ffdbe9bfe26" } }
            ]
        }))],
        vec![],
    )
    .await;

    let liquid_txid: elements::Txid = PEG_OUT_TXID.parse().unwrap();
    let error = peg
        .release_peg_out(&liquid_txid)
        .await
        .expect_err("a transaction with no peg-out must be rejected");

    let NigiriError::PegOutputNotFound { liquid_txid } = &error else {
        panic!("expected PegOutputNotFound, got {error}");
    };
    assert_eq!(liquid_txid, PEG_OUT_TXID);
}

/// A peg-out-shaped `OP_RETURN` naming a 32-byte genesis that is not the regtest one, followed by
/// a plausible destination script push. Hand-built rather than node-captured — like
/// `peg/output.rs`'s rejection vectors — because only the golden vector needs to come from a real
/// node; this one only needs the right shape to exercise the "structurally a peg-out, wrong
/// chain" branch.
const WRONG_CHAIN_PEG_OUT_SCRIPT: &str = "6a201111111111111111111111111111111111111111111111111111111111111111160014389ffce9cd9ae88dcc0631e88a821ffdbe9bfe26";

/// Three outputs: an ordinary payment, a peg-out-shaped output for a foreign chain, then the real
/// golden peg-out. Used to prove the foreign-chain output does not shadow the genuine one.
fn transaction_with_wrong_chain_before_golden(value_btc: &str) -> Value {
    let ordinary = "0014389ffce9cd9ae88dcc0631e88a821ffdbe9bfe26";
    serde_json::from_str(&format!(
        r#"{{"vout":[
            {{"value":0.5,"scriptPubKey":{{"hex":"{ordinary}"}}}},
            {{"value":0.5,"scriptPubKey":{{"hex":"{WRONG_CHAIN_PEG_OUT_SCRIPT}"}}}},
            {{"value":{value_btc},"scriptPubKey":{{"hex":"{GOLDEN_PEG_OUT_SCRIPT}"}}}}
        ]}}"#
    ))
    .expect("the scripted transaction is valid JSON")
}

// Catches a regression that rejects a transaction outright the first time it sees a
// peg-out-shaped output for a different parent chain, instead of continuing to scan for the
// genuine one. A wrong-chain OP_RETURN before the real peg-out must not shadow it.
#[tokio::test]
async fn release_peg_out_skips_a_wrong_chain_output_and_finds_the_real_one() {
    let (peg, _liquid, bitcoin_requests) = connected_peg(
        vec![ok(transaction_with_wrong_chain_before_golden("0.00010000"))],
        vec![
            ok(Value::String(RELEASE_TXID.to_owned())),
            ok(Value::String(
                "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080".to_owned(),
            )),
            ok(json!([format!("{}", "cd".repeat(32))])),
        ],
    )
    .await;

    let liquid_txid: elements::Txid = PEG_OUT_TXID.parse().unwrap();
    let released = peg
        .release_peg_out(&liquid_txid)
        .await
        .expect("the genuine peg-out is found past the wrong-chain output");

    assert_eq!(released.destination.to_string(), GOLDEN_DESTINATION);
    assert_eq!(released.amount, bitcoin::Amount::from_sat(10_000));

    let bitcoin_requests = bitcoin_requests.await.unwrap();
    assert_eq!(bitcoin_requests[1]["method"], "sendtoaddress");
    assert_eq!(bitcoin_requests[1]["params"][0], json!(GOLDEN_DESTINATION));
}

// Catches a regression that drops the diagnostic once a mismatch is no longer fatal on sight: a
// caller who genuinely points at a foreign-chain peg-out must still learn why it was rejected.
#[tokio::test]
async fn release_peg_out_reports_a_wrong_chain_mismatch_when_nothing_else_matches() {
    let (peg, _liquid, _bitcoin) = connected_peg(
        vec![ok(json!({
            "vout": [
                { "value": 0.5, "scriptPubKey": { "hex": WRONG_CHAIN_PEG_OUT_SCRIPT } }
            ]
        }))],
        vec![],
    )
    .await;

    let liquid_txid: elements::Txid = PEG_OUT_TXID.parse().unwrap();
    let error = peg
        .release_peg_out(&liquid_txid)
        .await
        .expect_err("a transaction with only a wrong-chain peg-out must be rejected");

    let NigiriError::PegOutputMalformed {
        liquid_txid,
        detail,
    } = &error
    else {
        panic!("expected PegOutputMalformed, got {error}");
    };
    assert_eq!(liquid_txid, PEG_OUT_TXID);
    assert!(
        detail.contains("parent chain"),
        "unhelpful detail: {detail}"
    );
}

/// [`GOLDEN_PEG_OUT_SCRIPT`] with its 21-byte P2WPKH destination push replaced by a single
/// non-empty byte that names no standard address template (not p2pkh, p2sh, or a witness
/// program). The parent genesis push is untouched, so this still matches this pair's parent.
const NON_STANDARD_DESTINATION_SCRIPT: &str =
    "6a2006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f0151";

// Catches a regression that accepts a peg-out whose destination push cannot be turned into a
// standard Bitcoin address, which would leave the simulated federation with nowhere valid to pay.
#[tokio::test]
async fn release_peg_out_rejects_a_non_standard_destination_script() {
    let (peg, _liquid, _bitcoin) = connected_peg(
        vec![ok(peg_out_transaction(
            NON_STANDARD_DESTINATION_SCRIPT,
            "0.00010000",
        ))],
        vec![],
    )
    .await;

    let liquid_txid: elements::Txid = PEG_OUT_TXID.parse().unwrap();
    let error = peg
        .release_peg_out(&liquid_txid)
        .await
        .expect_err("a non-standard destination script must be rejected");

    let NigiriError::PegOutputMalformed {
        liquid_txid,
        detail,
    } = &error
    else {
        panic!("expected PegOutputMalformed, got {error}");
    };
    assert_eq!(liquid_txid, PEG_OUT_TXID);
    assert!(
        detail.contains("not a standard address"),
        "unhelpful detail: {detail}"
    );
}

// Catches a regression that accepts a peg-out output with no explicit value, which would leave
// the simulated federation with no amount to pay out.
#[tokio::test]
async fn release_peg_out_rejects_a_peg_out_output_with_no_value() {
    let (peg, _liquid, _bitcoin) = connected_peg(
        vec![ok(peg_out_transaction(GOLDEN_PEG_OUT_SCRIPT, "null"))],
        vec![],
    )
    .await;

    let liquid_txid: elements::Txid = PEG_OUT_TXID.parse().unwrap();
    let error = peg
        .release_peg_out(&liquid_txid)
        .await
        .expect_err("a peg-out output with no value must be rejected");

    let NigiriError::PegOutputMalformed {
        liquid_txid,
        detail,
    } = &error
    else {
        panic!("expected PegOutputMalformed, got {error}");
    };
    assert_eq!(liquid_txid, PEG_OUT_TXID);
    assert!(
        detail.contains("no explicit value"),
        "unhelpful detail: {detail}"
    );
}

// Catches a regression that accepts a peg-out value that cannot be parsed as an amount — for
// example a negative number the node would never actually send but a hand-rolled decoder might
// wave through — which would panic or silently misreport further down the release path.
#[tokio::test]
async fn release_peg_out_rejects_a_value_that_is_not_an_amount() {
    let (peg, _liquid, _bitcoin) = connected_peg(
        vec![ok(peg_out_transaction(GOLDEN_PEG_OUT_SCRIPT, "-1"))],
        vec![],
    )
    .await;

    let liquid_txid: elements::Txid = PEG_OUT_TXID.parse().unwrap();
    let error = peg
        .release_peg_out(&liquid_txid)
        .await
        .expect_err("a value that is not an amount must be rejected");

    let NigiriError::PegOutputMalformed {
        liquid_txid,
        detail,
    } = &error
    else {
        panic!("expected PegOutputMalformed, got {error}");
    };
    assert_eq!(liquid_txid, PEG_OUT_TXID);
    assert!(
        detail.contains("is not an amount"),
        "unhelpful detail: {detail}"
    );
}

// Catches a regression that sends the wrong sendtomainchain vector. A live node rejects it; this
// pins the shape so the mistake surfaces without Docker.
#[tokio::test]
async fn send_to_mainchain_sends_an_exact_decimal_amount() {
    let (peg, liquid_requests, _bitcoin) =
        connected_peg(vec![ok(Value::String(PEG_OUT_TXID.to_owned()))], vec![]).await;

    let sent = peg
        .send_to_mainchain(
            "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080",
            bitcoin::Amount::from_sat(1),
        )
        .await
        .expect("a peg-out is sent");

    assert_eq!(sent.to_string(), PEG_OUT_TXID);

    let liquid_requests = liquid_requests.await.unwrap();
    assert_eq!(liquid_requests[1]["method"], "sendtomainchain");
    assert_eq!(
        liquid_requests[1]["params"],
        serde_json::from_str::<Value>(
            r#"["bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080",0.00000001]"#
        )
        .unwrap()
    );
}
