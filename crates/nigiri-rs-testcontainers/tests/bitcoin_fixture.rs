//! The Bitcoin contracts, proven against a fixture instead of a host Nigiri installation.
//!
//! Each test owns its own chain, so nothing here needs the exclusive mutation lock the host suite
//! required: a reorg in one test cannot be observed by another. Every test is ignored because it
//! pulls pinned images and talks to a real daemon.

use std::time::Duration;

use bitcoin::Amount;
use nigiri_rs::NigiriClient;
use nigiri_rs_testcontainers::{Bitcoin, Fixture};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Builds a fully signed wallet transaction through the node's own RPC.
///
/// The host suite shelled out to `nigiri rpc` for this. A fixture has no CLI, so the same three
/// calls go through the typed client, which is also what a consumer of this crate would do.
async fn signed_wallet_transaction(
    client: &NigiriClient<Bitcoin>,
    destination: &str,
) -> Result<String, BoxError> {
    let outputs = serde_json::Value::Object(
        [(destination.to_owned(), serde_json::json!(0.0001))]
            .into_iter()
            .collect(),
    );
    let raw: String = client
        .rpc(
            "createrawtransaction",
            (Vec::<serde_json::Value>::new(), outputs),
        )
        .await?;
    let funded: serde_json::Value = client.rpc("fundrawtransaction", (&raw,)).await?;
    let funded_hex = funded["hex"].as_str().ok_or("funding omitted hex")?;
    let signed: serde_json::Value = client
        .rpc("signrawtransactionwithwallet", (funded_hex,))
        .await?;
    if signed["complete"] != serde_json::Value::Bool(true) {
        return Err("wallet did not completely sign fixture transaction".into());
    }
    Ok(signed["hex"]
        .as_str()
        .ok_or("signing omitted hex")?
        .to_owned())
}

// The whole read/fund/query/broadcast contract the host suite covered, now on a chain this test
// owns. Both writes are confirmed through Esplora, which is what proves the indexer is genuinely
// following the node rather than merely being reachable.
#[tokio::test]
#[ignore = "requires Docker and pulls pinned Bitcoin images"]
async fn bitcoin_complete_shared_contract() -> Result<(), BoxError> {
    let fixture = Fixture::<Bitcoin>::start().await?;
    let client = fixture.client();

    assert!(client.block_height().await? > 0);
    // The mapped port is whatever the runtime chose, never the fixed container port.
    assert_ne!(client.esplora_url().port(), Some(30_000));

    let wallet_address = client.new_address().await?;
    let wallet_address_text = wallet_address.to_string();
    let funding_txid = client
        .faucet(&wallet_address_text, Some(Amount::from_sat(100_000)))
        .await?;
    client
        .wait_for_confirmation(&funding_txid, Duration::from_secs(30))
        .await?;
    assert!(!client.get_utxos(&wallet_address_text).await?.is_empty());
    assert!(client.has_funds(&wallet_address_text).await?);
    assert_eq!(
        client.get_address_info(&wallet_address_text).await?.address,
        wallet_address.as_unchecked().clone()
    );
    assert!(client.get_tx_status(&funding_txid).await?.confirmed);
    assert_eq!(client.get_tx(&funding_txid).await?.txid, funding_txid);

    let destination = client.new_address().await?;
    let signed = signed_wallet_transaction(client, &destination.to_string()).await?;
    let broadcast_txid = client.broadcast_tx(&signed).await?;
    client
        .wait_for_confirmation(&broadcast_txid, Duration::from_secs(30))
        .await?;
    assert!(client.get_tx_status(&broadcast_txid).await?.confirmed);

    Ok(())
}

// Invalidate and reconsider, with no lock: the fixture owns its chain, so the reorg this test
// performs is invisible to every other test. That isolation is the point of the migration.
#[tokio::test]
#[ignore = "requires Docker and pulls pinned Bitcoin images"]
async fn bitcoin_reorg_restores_the_test_created_tip() -> Result<(), BoxError> {
    let fixture = Fixture::<Bitcoin>::start().await?;
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

// The public generic RPC, including the typed Core v30 responses behind `bitcoin-rpc-types`.
#[tokio::test]
#[ignore = "requires Docker and pulls pinned Bitcoin images"]
async fn bitcoin_public_rpc_deserializes_native_and_core_v30_types() -> Result<(), BoxError> {
    let fixture = Fixture::<Bitcoin>::start().await?;
    let client = fixture.client();

    let height: u64 = client.rpc("getblockcount", ()).await?;
    assert_eq!(height, 101, "a ready fixture has mined exactly 101 blocks");

    let _: bitcoin::BlockHash = client.rpc("getbestblockhash", ()).await?;

    #[cfg(feature = "bitcoin-rpc-types")]
    {
        let info: nigiri_rs::bitcoin_rpc_types::v30::GetBlockchainInfo =
            client.rpc("getblockchaininfo", ()).await?;
        assert_eq!(info.chain, "regtest");
        assert!(!info.best_block_hash.is_empty());
    }

    Ok(())
}

/// Headroom for two fixtures starting at once on a cold CI runner.
///
/// Parallelism itself is close to free: on an idle machine one fixture is ready in about 3 seconds
/// and two at once in about 4.4, so the second costs roughly a second. An earlier note here claimed
/// 103 seconds for two against 6 for one and blamed contention between the two 101-block mines.
/// That measurement was taken while unrelated runaway processes were saturating every core; it says
/// nothing about this crate. Re-measured on an idle host, twice: 4.46s and 4.37s.
///
/// The budget stays above the 60-second default anyway, because the first run on a CI runner pulls
/// two images inside it and that, not mining, is what can actually take minutes.
const PARALLEL_STARTUP_BUDGET: Duration = Duration::from_secs(120);

// Two fixtures at once must be genuinely independent: distinct mapped endpoints, and a chain each
// that the other cannot see. This is what the host suite could never test, because there was only
// ever one node.
#[tokio::test]
#[ignore = "requires Docker and pulls pinned Bitcoin images"]
async fn concurrent_fixtures_are_independent() -> Result<(), BoxError> {
    let (left, right) = tokio::try_join!(
        Fixture::<Bitcoin>::builder()
            .startup_timeout(PARALLEL_STARTUP_BUDGET)
            .start(),
        Fixture::<Bitcoin>::builder()
            .startup_timeout(PARALLEL_STARTUP_BUDGET)
            .start(),
    )?;

    let left_electrum = left.electrum_endpoint().port();
    let right_electrum = right.electrum_endpoint().port();
    assert_ne!(left_electrum, right_electrum);
    assert_ne!(
        left.client().esplora_url(),
        right.client().esplora_url(),
        "each fixture must own its own indexer endpoint"
    );

    // Mining on one chain must not move the other. Both heights are read from the nodes rather than
    // through `block_height()`, which reports the indexer's view: a fixture guarantees the two agree
    // when it becomes ready, not that the indexer keeps pace with blocks mined afterwards.
    async fn node_height<C: nigiri_rs_testcontainers::FixtureChain>(
        fixture: &Fixture<C>,
    ) -> Result<u64, nigiri_rs::NigiriError> {
        fixture.client().rpc("getblockcount", ()).await
    }

    let right_before = node_height(&right).await?;
    let address = left.client().new_address().await?;
    left.client()
        .generate_to_address(3, &address.to_string())
        .await?;

    assert_eq!(node_height(&left).await?, 104);
    assert_eq!(
        node_height(&right).await?,
        right_before,
        "one fixture's chain must not be visible to another"
    );

    Ok(())
}
