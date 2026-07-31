use std::{path::PathBuf, time::Duration};

use nigiri_rs::{Liquid, NigiriClient, NigiriConfig};
use serde::Deserialize;
use url::Url;

#[cfg(feature = "bitcoin-rpc-types")]
use nigiri_rs::Bitcoin;

fn fake_client<N: nigiri_rs::NigiriNetwork>() -> NigiriClient<N> {
    let executable =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-nigiri.sh");
    NigiriClient::with_config(NigiriConfig {
        chopsticks_url: Url::parse("http://127.0.0.1:1").unwrap(),
        esplora_url: Url::parse("http://127.0.0.1:1").unwrap(),
        executable,
        timeout: Duration::from_secs(2),
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
async fn liquid_rpc_uses_caller_record_with_native_elements_hash() {
    let client = fake_client::<Liquid>();
    let info: LiquidBlockchainInfo = client
        .rpc("getblockchaininfo", std::iter::empty::<&str>())
        .await
        .unwrap();

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
    let client = fake_client::<Bitcoin>();
    let info: nigiri_rs::bitcoin_rpc_types::v30::GetBlockchainInfo = client
        .rpc("getblockchaininfo", std::iter::empty::<&str>())
        .await
        .unwrap();

    assert_eq!(info.chain, "regtest");
    assert_eq!(info.blocks, 101);
    assert!(!info.initial_block_download);
}
