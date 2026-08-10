//! Proves each accepted signature shape actually produces a working test.
//!
//! These start real containers. They are not `#[ignore]`d: the design rejects auto-ignoring
//! because a silently skipped test reports green having verified nothing. If Docker is absent,
//! `FixtureError::RuntimeUnavailable` fails loudly instead.
//!
//! The last two tests are not written for the macro; they are two of the repository's own Liquid
//! tests, moved here and rewritten. A macro exercised only by tests written for it is untested
//! against real use. They live in this crate rather than the fixtures crate because the fixtures
//! crate cannot depend on the facade the macro expands into.

#![cfg(feature = "testcontainers")]

use std::time::Duration;

use bitcoin::Amount;
use nigiri_rs::testcontainers::PegPair;
use nigiri_rs::{Bitcoin, Liquid, NigiriClient};
use serde::Deserialize;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// Catches a regression that stops the attribute degrading to a plain async test when nothing is
// requested. `sqlx::test` accepts this shape too.
#[nigiri_rs::test]
async fn no_fixture_requested_still_runs() {
    tokio::task::yield_now().await;
}

// Catches a regression in the single-fixture path: the wrapper must start a stack, fund it, and
// hand the body a client that can already reach it.
#[nigiri_rs::test]
async fn one_bitcoin_client_is_funded_and_reachable(
    client: NigiriClient<Bitcoin>,
) -> Result<(), BoxError> {
    assert_eq!(client.block_height().await?, 101);

    // The endpoints must be the runtime-mapped ones, not the fixed container ports.
    assert_ne!(client.electrum_endpoint().port(), 50_000);
    assert_ne!(client.esplora_url().port(), Some(30_000));
    Ok(())
}

// Catches a regression in the multi-fixture path. Two chains in one test is the shape a
// cross-chain consumer needs, and it must produce two genuinely independent stacks.
//
// This test is also the only measurement of the concurrent-start path. Five runs each on
// 2026-08-05, warm-up discarded: 4.47s mean starting the pair together against 6.30s awaiting
// them one after the other, with no overlap between the two ranges. Bitcoin mines 101 blocks
// while Liquid mines one, so overlapping the wait is worth roughly the whole Liquid startup.
#[nigiri_rs::test]
async fn two_chains_are_independent(
    bitcoin: NigiriClient<Bitcoin>,
    liquid: NigiriClient<Liquid>,
) -> Result<(), BoxError> {
    assert_ne!(
        bitcoin.electrum_endpoint().port(),
        liquid.electrum_endpoint().port(),
        "each fixture must own its own mapped endpoint"
    );
    assert_eq!(bitcoin.block_height().await?, 101);
    assert_eq!(liquid.block_height().await?, 1);
    Ok(())
}

// Catches a regression that drops the startup_timeout argument, which would silently fall back to
// the 60-second default.
#[nigiri_rs::test(startup_timeout = 120)]
async fn startup_timeout_is_accepted(client: NigiriClient<Bitcoin>) -> Result<(), BoxError> {
    assert_eq!(client.block_height().await?, 101);
    Ok(())
}

// Catches a regression that drops the tokio flavor, which would run this on the current-thread
// runtime instead. Asking the handle which flavor it is under is the only assertion that
// discriminates: everything else in this file behaves identically on either runtime, so a test
// that merely reached the chain would pass with the argument silently thrown away.
#[nigiri_rs::test(flavor = "multi_thread")]
async fn flavor_is_forwarded(client: NigiriClient<Bitcoin>) -> Result<(), BoxError> {
    assert_eq!(
        tokio::runtime::Handle::current().runtime_flavor(),
        tokio::runtime::RuntimeFlavor::MultiThread,
    );
    assert_eq!(client.block_height().await?, 101);
    Ok(())
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
    client: &NigiriClient<Liquid>,
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

// Moved from the fixtures crate's liquid_fixture.rs and rewritten to the macro. Reads only, so it
// converts cleanly: the `Fixture::<Liquid>::start()` preamble becomes the parameter.
#[nigiri_rs::test]
async fn liquid_public_rpc_deserializes_native_elements_types(
    client: NigiriClient<Liquid>,
) -> Result<(), BoxError> {
    // Each fixture owns its own chain, so there is nothing here to serialize against: no other
    // test can observe a reorg or mutation on this node.
    let height: u64 = client.rpc("getblockcount", ()).await?;
    let _: elements::BlockHash = client.rpc("getbestblockhash", ()).await?;
    let info: LiquidBlockchainInfo = client.rpc("getblockchaininfo", ()).await?;

    assert_eq!(info.chain, "liquidregtest");
    assert!(height > 0);
    assert!(info.blocks > 0);
    assert_eq!(info.bestblockhash.to_string().len(), 64);
    Ok(())
}

// Also moved from liquid_fixture.rs. This is the closest thing in the repository to what a real
// consumer does — faucet, UTXO lookup, a signed broadcast, `mint`, and `faucet_asset` — which is
// why it is the one to prove the macro against. Every assertion is unchanged.
#[nigiri_rs::test]
async fn liquid_complete_shared_and_asset_contract(
    client: NigiriClient<Liquid>,
) -> Result<(), BoxError> {
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
    let signed = signed_wallet_transaction(&client, &destination.to_string()).await?;
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

// Catches a regression in the pair parameter: the wrapper must start the wired stack, hand the body
// a `PegPair` that owns it, and keep all four containers alive for the test's duration.
#[nigiri_rs::test(startup_timeout = 180)]
async fn a_peg_pair_parameter_starts_a_wired_stack(peg: PegPair) -> Result<(), BoxError> {
    let pegged = peg.peg().complete_peg_in(Amount::from_sat(100_000)).await?;

    // `pegged.amount` is `complete_peg_in`'s own argument echoed back, so comparing it against the
    // amount just passed in cannot fail no matter what was really pegged in. Ask the Liquid node
    // about the claim instead: `-txindex=1` makes even a mempool transaction retrievable, so this
    // needs no mined block to prove the node genuinely knows it.
    let claim: serde_json::Value = peg
        .liquid()
        .rpc("getrawtransaction", (pegged.claim_txid.to_string(), 1_u64))
        .await?;
    assert!(
        claim["vout"]
            .as_array()
            .is_some_and(|vout| !vout.is_empty()),
        "the Liquid node must know the claim transaction and report its outputs: {claim}"
    );

    // Both halves are reachable through the pair, which is what distinguishes it from two
    // independent stacks. `>= 101` is a floor a pair that pegged nothing already clears — 101 is
    // also the arrival height, so this is not evidence that `complete_peg_in` mined anything.
    // (Not sampled before and after: `block_height` is Esplora-backed and the blocks
    // `complete_peg_in` just mined reach the indexer on its own schedule, so a before/after
    // comparison would be flaky.)
    assert_eq!(peg.liquid().block_height().await?, 1);
    assert!(peg.bitcoin().block_height().await? >= 101);
    Ok(())
}
