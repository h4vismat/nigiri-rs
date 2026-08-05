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
//! nigiri-rs = { version = "0.4", features = ["testcontainers"] }
//! ```

pub use nigiri_rs_core::*;

/// Ephemeral Docker-backed regtest fixtures.
///
/// Requires the `testcontainers` feature.
#[cfg(feature = "testcontainers")]
pub use nigiri_rs_testcontainers as testcontainers;

/// Provisions a regtest stack for a test and injects a ready [`NigiriClient`].
///
/// Requires the `testcontainers` feature.
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
