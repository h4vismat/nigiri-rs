//! Ephemeral Bitcoin regtest fixtures backed by Testcontainers.
//!
//! Each fixture is one throwaway regtest stack: a Bitcoin Core node with a funded wallet, an Electrs
//! indexer following it, and a [`nigiri_rs::NigiriClient`] pointed at both. Nothing is shared, so
//! tests can run in parallel and mine or reorg freely without coordinating.
//!
//! ```no_run
//! use nigiri_testcontainers::BitcoinFixture;
//!
//! # async fn example() -> Result<(), nigiri_testcontainers::FixtureError> {
//! let fixture = BitcoinFixture::start().await?;
//! let client = fixture.client();
//! let electrum_host = fixture.electrum_endpoint().host();
//! let electrum_port = fixture.electrum_endpoint().port();
//! # let _ = (client, electrum_host, electrum_port);
//! # Ok(())
//! # }
//! ```
//!
//! # What a fixture requires and guarantees
//!
//! Docker must be running; no Nigiri installation is needed. Ports are chosen by the runtime, so read
//! them from the fixture rather than assuming Nigiri's fixed ones. Containers, their anonymous
//! volumes, and the network are removed when the fixture is dropped, and nothing survives the test.
//! The first start on a machine pulls two pinned images, which is slow; later starts reuse them and a
//! fixture is ready in a few seconds.
//!
//! When [`BitcoinFixture::start`] returns, the node, Esplora, and Electrum all report the same tip,
//! so the wallet's funds are queryable through any of them. That agreement is established once, at
//! startup: blocks mined afterwards reach the indexer on its own schedule.
//!
//! Starting several fixtures at once costs more than starting one, because every node mines its own
//! 101 blocks while every indexer follows it. Raise
//! [`BitcoinFixtureBuilder::startup_timeout`] when doing that.
//!
//! The images are pinned by tag and digest. [`ContainerImage`] can replace them, but an image this
//! crate has not been tested against may not honour the same arguments.

mod bitcoind;
mod deadline;
mod diagnostics;
mod electrs;
mod electrum;
mod endpoint;
mod error;
mod fixture;
mod image;
mod owned_start;
mod readiness;

pub use endpoint::ElectrumEndpoint;
pub use error::FixtureError;
pub use fixture::{BitcoinFixture, BitcoinFixtureBuilder};
pub use image::ContainerImage;

/// The fixture's regtest RPC credentials.
///
/// Declared once for the whole crate: the service requests, the client configuration, and the
/// redaction patterns all derive from these, so they cannot drift apart.
pub(crate) const RPC_USER: &str = "admin1";
pub(crate) const RPC_PASSWORD: &str = "123";

/// The Nigiri release whose Bitcoin topology the pinned images reproduce.
///
/// Recorded so a future divergence can be traced to a version rather than guessed at. It is
/// documentation, not a runtime check: nothing here talks to Nigiri.
pub const REPRODUCED_NIGIRI_VERSION: &str = "nigiri-v0.5.17";
