# Tutorial: settle a Lightning payment

This test starts a funded Bitcoin regtest stack plus two LND nodes, then settles a real BOLT11
payment over their confirmed channel. You need Rust 1.88 or newer and a running Docker daemon; the
first run pulls three pinned images.

## Add the test dependency

```toml
[dev-dependencies]
nigiri-rs = { version = "0.5", features = ["lightning-fixtures"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

`lightning-fixtures` enables `fixtures` and `lnd`, exposing `LndPair` and the Lightning domain
types. The `fixtures` feature alone provides Bitcoin/Liquid fixtures without the LND dependency.
For a direct dependency, enable `nigiri-rs-fixtures` with `features = ["lnd"]`.

## Write the payment test

```rust,no_run
use std::time::Duration;

use nigiri_rs::fixtures::LndPair;
use nigiri_rs::{
    CreateInvoiceRequest, InvoiceState, Millisats, PaymentOptions, PaymentState,
};

#[tokio::test]
async fn alice_pays_bob() -> Result<(), Box<dyn std::error::Error>> {
    let pair = LndPair::start().await?;

    let invoice = pair
        .bob()
        .create_invoice(CreateInvoiceRequest::new(
            Millisats::new(25_000),
            "tutorial payment",
            Duration::from_secs(60),
        )?)
        .await?;
    let payment_hash = invoice.payment_hash();

    let payment = pair
        .alice()
        .pay_invoice(
            invoice.invoice(),
            PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(30))?,
        )
        .await?;
    assert_eq!(payment.payment_hash(), payment_hash);
    assert_eq!(payment.state(), PaymentState::Succeeded);

    let paid = pair.alice().lookup_payment(payment_hash).await?;
    assert_eq!(paid.payment_hash(), payment_hash);
    assert_eq!(paid.state(), PaymentState::Succeeded);

    let settled = pair.bob().lookup_invoice(payment_hash).await?;
    assert_eq!(settled.payment_hash(), payment_hash);
    assert_eq!(settled.state(), InvoiceState::Settled);

    pair.shutdown().await?;
    Ok(())
}
```

The hash ties together the created invoice, Alice's terminal payment, Alice's payment lookup, and
Bob's settled invoice lookup. That is stronger than inspecting the latest record: startup itself
settles two public 1,000-msat readiness invoices, one in each direction, and intentionally leaves
them in both nodes' histories.

## What startup proved before the test body ran

The 180-second default is one deadline for the complete call, not a fresh timeout per phase.
`LndPair::start()` validated all four images and the allocation before creating anything; started
bitcoind, Electrs, Alice, and Bob; initialized independent stateless wallets without retaining their
passwords or seeds; synchronized both LND nodes to Bitcoin regtest; funded Alice; connected the peer;
opened a 2,000,000-sat channel with 1,000,000 sats pushed to Bob; mined exactly six confirmations;
waited for post-channel graph synchronization and spendable local balance on both sides; and settled
the two readiness payments.

If a reverse readiness attempt reaches terminal `no route` or `insufficient balance`, startup
revalidates the channel and creates a fresh invoice before retrying. Authentication, transport,
malformed-response, and uncertain outcomes are terminal. Application payments follow the same safety
rule: on `LndError::OutcomeUnknown`, query `lookup_payment(payment_hash)` before any retry.

## Tune the topology

```rust,no_run
use std::time::Duration;
use nigiri_rs::{Sats, fixtures::LndPair};

# async fn example() -> Result<(), nigiri_rs::fixtures::FixtureError> {
let pair = LndPair::builder()
    .startup_timeout(Duration::from_secs(300))
    .channel_capacity(Sats::new(3_000_000))
    .push_amount(Sats::new(1_500_000))
    .start()
    .await?;
# pair.shutdown().await?;
# Ok(())
# }
```

The push must be nonzero and below capacity, and both nominal sides must retain at least 100,000
sats. The defaults are 2,000,000/1,000,000 sats. Validation also checks signed LND request ranges,
reserve arithmetic, and Bitcoin's monetary maximum before Docker starts.

## Related

- [Fixture API reference](reference-fixtures.md#lndpair)
- [Lightning client API](reference-client.md#lightning-client-api)
- [What "ready" means](explanation-fixture-readiness.md)
- [Errors](reference-errors.md)
