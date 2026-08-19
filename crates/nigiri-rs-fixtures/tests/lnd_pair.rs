//! Real ready-to-pay guarantees for the two-node LND fixture.

use std::time::Duration;

use bitcoin::OutPoint;
use nigiri_rs_fixtures::LndPair;
use nigiri_rs_lnd::{
    CreateInvoiceRequest, InvoiceState, LndClient, Millisats, PaymentOptions, PaymentState,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
const PAYMENT_AMOUNT: Millisats = Millisats::new(25_000);

// Catches startup returning authenticated nodes without a channel that can settle payments in
// both directions. Each assertion follows the payment hash created by this test, since startup's
// readiness probe deliberately leaves its own invoice and payment in node history.
#[tokio::test]
async fn pair_settles_payments_in_both_directions() -> Result<(), BoxError> {
    let pair = LndPair::start().await?;
    let channel_point = pair.channel_point();

    let alice_before_receive = local_balance(pair.alice(), channel_point).await?;
    let bob_before = local_balance(pair.bob(), channel_point).await?;
    let bob_to_alice = pair
        .alice()
        .create_invoice(CreateInvoiceRequest::new(
            PAYMENT_AMOUNT,
            "bob-to-alice",
            Duration::from_secs(60),
        )?)
        .await?;
    let paid_alice = pair
        .bob()
        .pay_invoice(
            bob_to_alice.invoice(),
            PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(30))?,
        )
        .await?;
    assert_eq!(paid_alice.payment_hash(), bob_to_alice.payment_hash());
    assert_eq!(paid_alice.state(), PaymentState::Succeeded);
    assert_eq!(
        pair.alice()
            .lookup_invoice(bob_to_alice.payment_hash())
            .await?
            .state(),
        InvoiceState::Settled
    );
    tokio::try_join!(
        wait_for_balance_decrease(pair.bob(), channel_point, bob_before, PAYMENT_AMOUNT, "Bob",),
        wait_for_balance_increase(
            pair.alice(),
            channel_point,
            alice_before_receive,
            PAYMENT_AMOUNT,
            "Alice",
        ),
    )?;

    let alice_before = local_balance(pair.alice(), channel_point).await?;
    let bob_before_receive = local_balance(pair.bob(), channel_point).await?;
    let alice_to_bob = pair
        .bob()
        .create_invoice(CreateInvoiceRequest::new(
            PAYMENT_AMOUNT,
            "alice-to-bob",
            Duration::from_secs(60),
        )?)
        .await?;
    let paid_bob = pair
        .alice()
        .pay_invoice(
            alice_to_bob.invoice(),
            PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(30))?,
        )
        .await?;
    assert_eq!(paid_bob.payment_hash(), alice_to_bob.payment_hash());
    assert_eq!(paid_bob.state(), PaymentState::Succeeded);
    assert_eq!(
        pair.bob()
            .lookup_invoice(alice_to_bob.payment_hash())
            .await?
            .state(),
        InvoiceState::Settled
    );
    tokio::try_join!(
        wait_for_balance_decrease(
            pair.alice(),
            channel_point,
            alice_before,
            PAYMENT_AMOUNT,
            "Alice",
        ),
        wait_for_balance_increase(
            pair.bob(),
            channel_point,
            bob_before_receive,
            PAYMENT_AMOUNT,
            "Bob",
        ),
    )?;

    Ok(())
}

async fn local_balance(client: &LndClient, point: OutPoint) -> Result<Millisats, BoxError> {
    client
        .list_channels()
        .await?
        .into_iter()
        .find(|channel| channel.channel_point() == point)
        .map(|channel| channel.local_balance())
        .ok_or_else(|| format!("active channel {point} disappeared").into())
}

async fn wait_for_balance_decrease(
    client: &LndClient,
    point: OutPoint,
    before: Millisats,
    amount: Millisats,
    payer: &'static str,
) -> Result<(), BoxError> {
    let mut observed = before;
    let expected_maximum = before.as_u64().saturating_sub(amount.as_u64());
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            observed = local_balance(client, point).await?;
            if observed.as_u64() <= expected_maximum {
                return Ok::<(), BoxError>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| {
        format!(
            "{payer}'s channel {point} balance did not decrease by {amount:?} after the settled payment: before={before:?}, last={observed:?}"
        )
    })??;
    Ok(())
}

async fn wait_for_balance_increase(
    client: &LndClient,
    point: OutPoint,
    before: Millisats,
    amount: Millisats,
    receiver: &'static str,
) -> Result<(), BoxError> {
    let mut observed = before;
    let expected_minimum = before.as_u64().saturating_add(amount.as_u64());
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            observed = local_balance(client, point).await?;
            if observed.as_u64() >= expected_minimum {
                return Ok::<(), BoxError>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| {
        format!(
            "{receiver}'s channel {point} balance did not increase by {amount:?} after the settled payment: before={before:?}, last={observed:?}"
        )
    })??;
    Ok(())
}
