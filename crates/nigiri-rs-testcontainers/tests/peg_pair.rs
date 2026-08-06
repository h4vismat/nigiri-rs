//! Peg round trips against a wired pair. Each test starts four containers.
//!
//! Confirmation is asserted through the node's own `getrawtransaction`, never through Esplora: the
//! indexer catches up on its own schedule after startup, so an assertion on a block mined by the
//! test itself must not route through it.

use bitcoin::Amount;
use nigiri_rs_core::NigiriError;
use nigiri_rs_testcontainers::PegPair;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How many blocks a test will mine while waiting for the Liquid node's view of the mainchain to
/// catch up. Mirrors `CLAIM_RETRY_BLOCKS` in `nigiri-rs-core`: the node rejects a claim at exactly
/// the depth it reports and accepts it a few blocks later.
const CLAIM_RETRY_BLOCKS: u64 = 20;

/// Confirms the given Liquid transaction by mining one block, and returns its confirmation count.
async fn liquid_confirmations(pair: &PegPair, txid: &elements::Txid) -> Result<u64, BoxError> {
    let address = pair.liquid().new_address().await?;
    pair.liquid()
        .generate_to_address(1, &address.to_string())
        .await?;

    let confirmed: serde_json::Value = pair
        .liquid()
        .rpc("getrawtransaction", (txid.to_string(), 1_u64))
        .await?;
    Ok(confirmed["confirmations"].as_u64().unwrap_or_default())
}

// Catches a regression anywhere in the wiring, the claim path, or the one-shot convenience: a
// deposit to a peg-in address must become L-BTC without the caller mining or waiting for anything.
#[tokio::test]
async fn a_deposit_becomes_lbtc_through_complete_peg_in() -> Result<(), BoxError> {
    let pair = PegPair::start().await?;

    let pegged = pair
        .peg()
        .complete_peg_in(Amount::from_sat(100_000))
        .await?;

    assert_eq!(pegged.amount, Amount::from_sat(100_000));
    assert_eq!(
        liquid_confirmations(&pair, &pegged.claim_txid).await?,
        1,
        "the claim must confirm in the Liquid block this test mined"
    );
    Ok(())
}

// Catches a regression in the primitives a wallet team drives themselves, and in the immature-claim
// error a caller sees before the deposit has matured. `complete_peg_in` hides both; this is the
// path a consumer writes by hand.
#[tokio::test]
async fn peg_in_driven_through_the_primitives_credits_the_liquid_wallet() -> Result<(), BoxError> {
    let pair = PegPair::start().await?;
    let peg = pair.peg();

    let request = peg.peg_in_request().await?;
    assert!(
        !request.claim_script.is_empty(),
        "a peg-in request must carry the claim script a caller may submit itself"
    );

    // `faucet` mines one confirming block, so the deposit sits at depth 1 here.
    let deposit = pair
        .bitcoin()
        .faucet(
            &request.mainchain_address.to_string(),
            Some(Amount::from_sat(150_000)),
        )
        .await?;

    let immature = peg
        .claim_peg_in(&deposit)
        .await
        .expect_err("a deposit one block deep must not be claimable");
    let NigiriError::PegInImmature { have, need } = immature else {
        panic!("an immature deposit must be reported as such: {immature}");
    };
    assert_eq!(have, 1);
    assert_eq!(need, peg.pegin_confirmation_depth());

    let mining_address = pair.bitcoin().new_address().await?.to_string();
    pair.bitcoin()
        .generate_to_address(need.saturating_sub(1), &mining_address)
        .await?;

    // The node's view of the mainchain lags the mainchain, so reaching the reported depth is
    // necessary but not sufficient. This is the loop `complete_peg_in` runs on a caller's behalf.
    let mut last = None;
    let mut claim = None;
    for attempt in 0..=CLAIM_RETRY_BLOCKS {
        if attempt > 0 {
            pair.bitcoin()
                .generate_to_address(1, &mining_address)
                .await?;
        }
        match peg.claim_peg_in(&deposit).await {
            Ok(txid) => {
                claim = Some(txid);
                break;
            }
            Err(error) => last = Some(error),
        }
    }
    let claim = claim.ok_or_else(|| {
        format!(
            "a matured deposit must be claimable within {CLAIM_RETRY_BLOCKS} blocks; last error: {}",
            last.expect("the loop records an error on every failed attempt")
        )
    })?;

    assert_eq!(
        liquid_confirmations(&pair, &claim).await?,
        1,
        "the claim must confirm in the Liquid block this test mined"
    );

    // The claim credits the wallet that issued the address, which is what makes the peg-in useful.
    let received: serde_json::Value = pair
        .liquid()
        .rpc("gettransaction", (claim.to_string(),))
        .await?;
    assert!(
        received["details"].is_array(),
        "the claiming wallet must know the transaction it received: {received}"
    );
    Ok(())
}

// Catches a regression in the simulated federation: the destination must be read out of the
// transaction, and the BTC must actually arrive. A consumer encoding a peg-out wrongly gets no
// payout, and that only means something if a correct one pays.
#[tokio::test]
async fn a_peg_out_is_released_to_the_destination_it_encodes() -> Result<(), BoxError> {
    let pair = PegPair::start().await?;

    let destination = pair.bitcoin().new_address().await?;
    let burn = pair
        .peg()
        .send_to_mainchain(&destination.to_string(), Amount::from_sat(10_000))
        .await?;

    let released = pair.peg().release_peg_out(&burn).await?;

    assert_eq!(released.liquid_txid, burn);
    assert_eq!(released.destination, destination);
    assert_eq!(released.amount, Amount::from_sat(10_000));

    // `release_peg_out` mines its own confirming block, inherited from `faucet`.
    let paid: serde_json::Value = pair
        .bitcoin()
        .rpc(
            "getrawtransaction",
            (released.bitcoin_txid.to_string(), 1_u64),
        )
        .await?;
    assert_eq!(
        paid["confirmations"].as_u64(),
        Some(1),
        "the release must be confirmed on the mainchain side: {paid}"
    );
    Ok(())
}

// Catches a regression that treats any transaction as a peg-out, which would let the simulated
// federation pay against something that never burned anything.
#[tokio::test]
async fn releasing_a_transaction_with_no_peg_out_output_is_not_found() -> Result<(), BoxError> {
    let pair = PegPair::start().await?;

    let elsewhere = pair.liquid().new_address().await?;
    let ordinary = pair
        .liquid()
        .faucet(&elsewhere.to_string(), Some(Amount::from_sat(50_000)))
        .await?;

    let error = pair
        .peg()
        .release_peg_out(&ordinary)
        .await
        .expect_err("an ordinary transfer carries no peg-out output");

    assert!(
        matches!(error, NigiriError::PegOutputNotFound { .. }),
        "{error}"
    );
    Ok(())
}
