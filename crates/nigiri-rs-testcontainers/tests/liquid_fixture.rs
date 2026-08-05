//! Liquid fixture tests. Each starts its own throwaway regtest stack.
//!
//! Two tests that used to live here now run through `#[nigiri_rs::test]` in
//! `crates/nigiri-rs/tests/macro_smoke.rs`: the `getblockchaininfo` deserialization check, and
//! the asset contract covering `mint` and `faucet_asset`. They had to move crates, not just
//! files — this crate cannot depend on the facade the macro expands into. If you change
//! `mint`, `faucet_asset`, or the typed RPC shapes, their proof is over there.

use nigiri_rs_testcontainers::{Fixture, Liquid};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The genesis hash of Nigiri's `liquidregtest` chain, read from a running Nigiri stack on
/// 2026-08-03.
///
/// This is the guard on the node's argument vector. Elements silently ignores unknown settings in
/// a configuration file and only rejects them on argv, so a parameter that stops taking effect
/// does not necessarily announce itself. A changed genesis here means the chain this fixture
/// builds is no longer the chain Nigiri runs, and the Electrs image indexes.
const NIGIRI_LIQUIDREGTEST_GENESIS: &str =
    "00902a6b70c2ca83b5d9c815d96a0e2f4202179316970d14ea1847dae5b1ca21";

#[tokio::test]
async fn fixture_reproduces_the_nigiri_liquid_chain() {
    let fixture = Fixture::<Liquid>::start()
        .await
        .expect("a pinned Liquid fixture must start against a real daemon");

    let genesis: String = fixture
        .client()
        .rpc("getblockhash", (0_u64,))
        .await
        .expect("a started node serves its genesis hash");

    assert_eq!(
        genesis, NIGIRI_LIQUIDREGTEST_GENESIS,
        "the fixture's chain parameters no longer reproduce Nigiri's liquidregtest chain"
    );
}

#[tokio::test]
async fn fixture_starts_ready_and_funded() {
    let fixture = Fixture::<Liquid>::start()
        .await
        .expect("a pinned Liquid fixture must start against a real daemon");

    let height = fixture
        .client()
        .block_height()
        .await
        .expect("a ready fixture must serve its Esplora tip");
    assert_eq!(
        height, 1,
        "a Liquid fixture mines exactly one block; its funds come from genesis. If this reads 0, \
         the initial mine in Liquid::fund_wallet was removed — which also breaks peg-in, because \
         a node still at height 0 reports initial block download and refuses getpeginaddress \
         under -validatepegin=1. See that function's doc comment."
    );

    // Liquid has no block subsidy, so this balance proves the genesis outputs were connected and
    // rescanned rather than mined for.
    let balance: serde_json::Value = fixture
        .client()
        .rpc("getbalance", ())
        .await
        .expect("a funded wallet reports a balance");
    assert_eq!(
        balance["bitcoin"].as_f64(),
        Some(21_000_000.0),
        "the wallet must hold the genesis free coins: {balance}"
    );

    let port = fixture.electrum_endpoint().port();
    assert_ne!(
        port, 50_001,
        "the Electrum endpoint must be the mapped port, not the fixed container port"
    );
    // The mapped port is whatever the runtime chose, never the fixed container port.
    assert_ne!(fixture.client().esplora_url().port(), Some(30_001));

    drop(fixture);
}

#[tokio::test]
async fn liquid_reorg_restores_the_test_created_tip() -> Result<(), BoxError> {
    let fixture = Fixture::<Liquid>::start().await?;
    let client = fixture.client();
    let baseline = client.best_block_hash().await?;
    let address = client.new_address().await?;
    let created = client.generate_to_address(2, &address.to_string()).await?;
    let test_tip = *created.last().ok_or("mining returned no blocks")?;
    assert_ne!(baseline, test_tip);

    let mutation = async {
        client.invalidate_block(&test_tip).await?;
        assert_ne!(client.best_block_hash().await?, test_tip);
        Ok::<_, nigiri_rs_core::NigiriError>(())
    }
    .await;
    let cleanup = client.reconsider_block(&test_tip).await;
    mutation?;
    cleanup?;
    assert_eq!(client.best_block_hash().await?, test_tip);
    Ok(())
}

// Catches a fixture that leaves the client reporting the fixed container port.
#[tokio::test]
async fn client_reports_the_mapped_electrum_port() {
    let fixture = Fixture::<Liquid>::start()
        .await
        .expect("a pinned Liquid fixture must start against a real daemon");

    let from_client = fixture.client().electrum_endpoint();
    assert_ne!(
        from_client.port(),
        50_001,
        "the client must report the mapped port, not the fixed container port"
    );
}
