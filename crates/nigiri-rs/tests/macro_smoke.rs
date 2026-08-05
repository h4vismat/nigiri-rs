//! Proves each accepted signature shape actually produces a working test.
//!
//! These start real containers. They are not `#[ignore]`d: the design rejects auto-ignoring
//! because a silently skipped test reports green having verified nothing. If Docker is absent,
//! `FixtureError::RuntimeUnavailable` fails loudly instead.

#![cfg(feature = "testcontainers")]

use nigiri_rs::{Bitcoin, Liquid, NigiriClient};

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
// cross-chain consumer needs, and it must produce two genuinely independent stacks — measured at
// about 2.7s for the pair, cheaper than two Bitcoin fixtures.
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
