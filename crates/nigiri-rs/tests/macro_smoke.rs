//! Proves each accepted signature shape actually produces a working test.
//!
//! These start real containers. They are not `#[ignore]`d: the design rejects auto-ignoring
//! because a silently skipped test reports green having verified nothing. If Docker is absent,
//! `FixtureError::RuntimeUnavailable` fails loudly instead.

#![cfg(feature = "testcontainers")]

use nigiri_rs::{Bitcoin, NigiriClient};

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
