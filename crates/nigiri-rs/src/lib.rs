//! Typed Bitcoin and Liquid regtest clients, with optional ephemeral Docker fixtures.
//!
//! This crate is a facade. The client lives in `nigiri-rs-core` and is re-exported here in full,
//! so every path published at 0.2.0 still resolves:
//!
//! ```
//! use nigiri_rs::{Bitcoin, NigiriClient};
//!
//! let client = NigiriClient::<Bitcoin>::new();
//! # let _ = client;
//! ```
//!
//! # Fixtures
//!
//! Enable the `testcontainers` feature to reach `testcontainers`, which provisions a throwaway
//! regtest stack per test. It is off by default because it pulls Docker client dependencies that a
//! consumer talking to a host-owned Nigiri does not need.
//!
//! ```toml
//! nigiri-rs = { version = "0.5", features = ["testcontainers"] }
//! ```
//!
//! # Testing a wallet against a throwaway chain
//!
//! The same feature provides [`macro@test`], which lets a wallet test skip the preamble above
//! entirely. See its documentation for the shape; the example lives there because it only
//! compiles with the feature enabled.

pub use nigiri_rs_core::*;

/// Ephemeral Docker-backed regtest fixtures.
///
/// Requires the `testcontainers` feature.
#[cfg(feature = "testcontainers")]
pub use nigiri_rs_testcontainers as testcontainers;

/// Provisions a regtest stack for a test and injects a ready [`NigiriClient`].
///
/// Requires the `testcontainers` feature, and Docker.
///
/// ```no_run
/// use nigiri_rs::{Bitcoin, NigiriClient};
///
/// #[nigiri_rs::test]
/// async fn my_wallet_sees_its_funding(client: NigiriClient<Bitcoin>) -> Result<(), Box<dyn std::error::Error>> {
///     // `client` is already pointed at a funded, synchronized stack.
///     let address = client.new_address().await?;
///     client.faucet(&address.to_string(), None).await?;
///
///     // Point a wallet library at either endpoint; both report runtime-mapped ports.
///     let _esplora = client.esplora_url();
///     let _electrum = client.electrum_endpoint();
///     Ok(())
/// }
/// ```
///
/// One fixture is started per parameter, so a cross-chain test takes two. The chain comes from the
/// parameter type; there is no attribute argument for it, and therefore no way for the two to
/// disagree. Tests are not `#[ignore]`d — if Docker is unavailable they fail loudly rather than
/// reporting green having run nothing.
///
/// Two arguments are accepted: `startup_timeout = <seconds>` and `flavor = "multi_thread"`.
#[cfg(feature = "testcontainers")]
pub use nigiri_rs_macros::test;

/// Implementation detail of `#[nigiri_rs::test]`. Not public API.
///
/// Generated code reaches every item it needs through this module, so a consumer depends only on
/// `nigiri-rs` and never has to add `tokio` or the fixtures crate to make an expansion compile.
/// Nothing here is covered by semver; do not reference it directly.
#[cfg(feature = "testcontainers")]
#[doc(hidden)]
pub mod __private {
    pub use nigiri_rs_testcontainers as testcontainers;
    pub use tokio;
}
