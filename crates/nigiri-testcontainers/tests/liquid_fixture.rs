//! Docker-gated Liquid fixture tests. Each starts its own throwaway regtest stack.

use nigiri_testcontainers::{Fixture, Liquid};

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
#[ignore = "requires Docker and pulls pinned Liquid images"]
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
#[ignore = "requires Docker and pulls pinned Liquid images"]
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
        "a Liquid fixture mines exactly one block; its funds come from genesis"
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
    assert_ne!(port, 0);
    assert_ne!(
        port, 50_001,
        "the Electrum endpoint must be the mapped port, not the fixed container port"
    );

    drop(fixture);
}
