mod support;

use std::time::Duration;

use bitcoin::Amount;
use nigiri_rs::{Bitcoin, NigiriClient};
use support::{BoxError, HostChainLock, signed_wallet_transaction};

#[tokio::test]
#[ignore = "requires host Nigiri"]
async fn bitcoin_complete_shared_contract() -> Result<(), BoxError> {
    let _lock = HostChainLock::acquire()?;
    let client = NigiriClient::<Bitcoin>::new();
    client.wait_ready().await?;
    assert!(client.block_height().await? > 0);
    assert_eq!(client.esplora_url().port(), Some(30_000));

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
    let destination_text = destination.to_string();
    let signed = signed_wallet_transaction(false, &destination_text).await?;
    let broadcast_txid = client.broadcast_tx(&signed).await?;
    client
        .wait_for_confirmation(&broadcast_txid, Duration::from_secs(30))
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires host Nigiri"]
async fn bitcoin_reorg_restores_the_test_created_tip() -> Result<(), BoxError> {
    let _lock = HostChainLock::acquire()?;
    let client = NigiriClient::<Bitcoin>::new();
    client.wait_ready().await?;
    let baseline = client.best_block_hash().await?;
    let address = client.new_address().await?;
    let address_text = address.to_string();
    let created = client.generate_to_address(2, &address_text).await?;
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
async fn bitcoin_public_rpc_deserializes_native_and_core_v30_types() -> Result<(), BoxError> {
    let client = NigiriClient::<Bitcoin>::new();
    client.wait_ready().await?;

    let height: u64 = client
        .rpc("getblockcount", std::iter::empty::<&str>())
        .await?;
    assert!(height > 0);

    let _: bitcoin::BlockHash = client
        .rpc("getbestblockhash", std::iter::empty::<&str>())
        .await?;

    #[cfg(feature = "bitcoin-rpc-types")]
    {
        let info: nigiri_rs::bitcoin_rpc_types::v30::GetBlockchainInfo = client
            .rpc("getblockchaininfo", std::iter::empty::<&str>())
            .await?;
        assert_eq!(info.chain, "regtest");
        assert!(!info.best_block_hash.is_empty());
    }

    Ok(())
}
