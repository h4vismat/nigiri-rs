//! Docker-gated Liquid fixture tests. Each starts its own throwaway regtest stack.

use std::time::Duration;

use bitcoin::Amount;
use nigiri_rs_testcontainers::{Fixture, Liquid};
use serde::Deserialize;

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
    // The mapped port is whatever the runtime chose, never the fixed container port.
    assert_ne!(fixture.client().esplora_url().port(), Some(30_001));

    drop(fixture);
}

#[derive(Debug, Deserialize)]
struct LiquidBlockchainInfo {
    chain: String,
    blocks: u64,
    bestblockhash: elements::BlockHash,
}

/// Builds, funds, blinds, and signs a wallet transaction.
///
/// The blinding step is the difference from Bitcoin: a Liquid output is confidential and cannot be
/// signed until its value and asset are blinded. Note also that Liquid's `createrawtransaction`
/// takes an *array* of output objects where Bitcoin's takes a single object.
async fn signed_wallet_transaction(
    client: &nigiri_rs::NigiriClient<Liquid>,
    destination: &str,
) -> Result<String, BoxError> {
    let outputs = serde_json::json!([{ destination: "0.00010000" }]);
    let raw: String = client
        .rpc("createrawtransaction", (serde_json::json!([]), outputs))
        .await?;

    let funded: serde_json::Value = client.rpc("fundrawtransaction", (raw,)).await?;
    let funded_hex = funded["hex"]
        .as_str()
        .ok_or("fundrawtransaction returned no hex")?;

    let blinded: String = client.rpc("blindrawtransaction", (funded_hex,)).await?;

    let signed: serde_json::Value = client
        .rpc("signrawtransactionwithwallet", (blinded,))
        .await?;
    if signed["complete"] != serde_json::Value::Bool(true) {
        return Err("wallet did not completely sign fixture transaction".into());
    }
    Ok(signed["hex"]
        .as_str()
        .ok_or("signrawtransactionwithwallet returned no hex")?
        .to_owned())
}

#[tokio::test]
#[ignore = "requires Docker and pulls pinned Liquid images"]
async fn liquid_complete_shared_and_asset_contract() -> Result<(), BoxError> {
    let fixture = Fixture::<Liquid>::start().await?;
    let client = fixture.client();
    assert!(client.block_height().await? > 0);

    let wallet_address = client.new_address().await?;
    let funding_txid = client
        .faucet(&wallet_address.to_string(), Some(Amount::from_sat(50_000)))
        .await?;
    client
        .wait_for_confirmation(&funding_txid, Duration::from_secs(30))
        .await?;
    assert!(
        !client
            .get_utxos(&wallet_address.to_string())
            .await?
            .is_empty()
    );
    assert!(client.has_funds(&wallet_address.to_string()).await?);
    assert_eq!(
        client
            .get_address_info(&wallet_address.to_string())
            .await?
            .address,
        wallet_address
    );
    assert!(client.get_tx_status(&funding_txid).await?.confirmed);
    assert_eq!(client.get_tx(&funding_txid).await?.txid, funding_txid);

    let destination = client.new_address().await?;
    let signed = signed_wallet_transaction(client, &destination.to_string()).await?;
    let broadcast_txid = client.broadcast_tx(&signed).await?;
    client
        .wait_for_confirmation(&broadcast_txid, Duration::from_secs(30))
        .await?;

    let minted = client
        .mint(&destination.to_string(), 1_000, "NigiriRsTest", "NRT")
        .await?;
    assert_eq!(minted.issuance_txin.txid.to_string().len(), 64);
    let asset_faucet_txid = client
        .faucet_asset(&destination.to_string(), Amount::ONE_BTC, &minted.asset)
        .await?;
    client
        .generate_to_address(1, &destination.to_string())
        .await?;
    client
        .wait_for_confirmation(&asset_faucet_txid, Duration::from_secs(30))
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker and pulls pinned Liquid images"]
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
        Ok::<_, nigiri_rs::NigiriError>(())
    }
    .await;
    let cleanup = client.reconsider_block(&test_tip).await;
    mutation?;
    cleanup?;
    assert_eq!(client.best_block_hash().await?, test_tip);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker and pulls pinned Liquid images"]
async fn liquid_public_rpc_deserializes_native_elements_types() -> Result<(), BoxError> {
    // Each fixture owns its own chain, so there is nothing here to serialize against: no other
    // test can observe a reorg or mutation on this node.
    let fixture = Fixture::<Liquid>::start().await?;
    let client = fixture.client();

    let height: u64 = client.rpc("getblockcount", ()).await?;
    let _: elements::BlockHash = client.rpc("getbestblockhash", ()).await?;
    let info: LiquidBlockchainInfo = client.rpc("getblockchaininfo", ()).await?;

    assert_eq!(info.chain, "liquidregtest");
    assert!(height > 0);
    assert!(info.blocks > 0);
    assert_eq!(info.bestblockhash.to_string().len(), 64);
    Ok(())
}
