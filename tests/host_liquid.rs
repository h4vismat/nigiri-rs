mod support;

use std::time::Duration;

use bitcoin::Amount;
use nigiri_rs::{Liquid, NigiriClient};
use serde::Deserialize;
use support::{BoxError, HostChainLock, signed_wallet_transaction};

#[derive(Debug, Deserialize)]
struct LiquidBlockchainInfo {
    chain: String,
    blocks: u64,
    bestblockhash: elements::BlockHash,
}

#[tokio::test]
#[ignore = "requires host Nigiri"]
async fn liquid_complete_shared_and_asset_contract() -> Result<(), BoxError> {
    let _lock = HostChainLock::acquire()?;
    let client = NigiriClient::<Liquid>::new();
    client.wait_ready().await?;
    assert!(client.block_height().await? > 0);
    assert_eq!(client.esplora_url().port(), Some(30_001));

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
    let signed = signed_wallet_transaction(&destination.to_string()).await?;
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
#[ignore = "requires host Nigiri"]
async fn liquid_reorg_restores_the_test_created_tip() -> Result<(), BoxError> {
    let _lock = HostChainLock::acquire()?;
    let client = NigiriClient::<Liquid>::new();
    client.wait_ready().await?;
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
#[ignore = "requires host Nigiri"]
async fn liquid_public_rpc_deserializes_native_elements_types() -> Result<(), BoxError> {
    // Deliberately no HostChainLock: this test only reads. Its assertions (a
    // nonzero height, hashes that deserialize) hold even against a tip observed
    // mid-reorg by a neighbouring test, so the exclusive mutation lock would only
    // serialize work that does not mutate. See README, "Host integration tests".
    let client = NigiriClient::<Liquid>::new();
    client.wait_ready().await?;

    let height: u64 = client.rpc("getblockcount", ()).await?;
    let _: elements::BlockHash = client.rpc("getbestblockhash", ()).await?;
    let info: LiquidBlockchainInfo = client.rpc("getblockchaininfo", ()).await?;

    assert_eq!(info.chain, "liquidregtest");
    assert!(height > 0);
    assert!(info.blocks > 0);
    assert_eq!(info.bestblockhash.to_string().len(), 64);
    Ok(())
}
