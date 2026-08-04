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
//! Enable the `testcontainers` feature to reach [`testcontainers`], which provisions a throwaway
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
