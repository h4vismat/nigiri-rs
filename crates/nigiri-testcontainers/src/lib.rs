//! Ephemeral Bitcoin and Liquid regtest fixtures backed by Testcontainers.
//!
//! Each fixture is one throwaway regtest stack: a node with a funded wallet, an Electrs indexer
//! following it, and a [`nigiri_rs::NigiriClient`] pointed at both. Nothing is shared, so tests can
//! run in parallel and mine or reorg freely without coordinating.
//!
//! ```no_run
//! use nigiri_testcontainers::{Bitcoin, Fixture};
//!
//! # async fn example() -> Result<(), nigiri_testcontainers::FixtureError> {
//! let fixture = Fixture::<Bitcoin>::start().await?;
//! let client = fixture.client();
//! let electrum_host = fixture.electrum_endpoint().host();
//! let electrum_port = fixture.electrum_endpoint().port();
//! # let _ = (client, electrum_host, electrum_port);
//! # Ok(())
//! # }
//! ```
//!
//! `Fixture::<Liquid>::start` starts the same way; swap the type parameter.
//!
//! # What a fixture requires and guarantees
//!
//! Docker must be running; no Nigiri installation is needed. Ports are chosen by the runtime, so read
//! them from the fixture rather than assuming Nigiri's fixed ones. Containers, their anonymous
//! volumes, and the network are removed when the fixture is dropped, and nothing survives the test.
//! The first start on a machine pulls two pinned images per chain, which is slow; later starts reuse
//! them and a fixture is ready in a few seconds.
//!
//! When [`Fixture::start`] returns, the node, Esplora, and Electrum all report the same tip,
//! so the wallet's funds are queryable through any of them. That agreement is established once, at
//! startup: blocks mined afterwards reach the indexer on its own schedule.
//!
//! Funding a wallet differs by chain: Bitcoin has a block subsidy, so a Bitcoin fixture mines 101
//! blocks until its coinbase matures. Liquid has none, so a Liquid fixture instead connects its
//! genesis outputs; the single block it then mines is for neither funds nor the indexer, and exists
//! only because callers reasonably expect a nonzero tip.
//!
//! Fixtures are cheap to run in parallel. Measured on an idle machine with the images already
//! pulled: one Bitcoin fixture is ready in about 3 seconds, one Liquid fixture in about 1.5, two
//! Bitcoin fixtures at once in about 4.5, and two of each at once in about 5. The default
//! 60-second budget covers all of those with room to spare; raise
//! [`FixtureBuilder::startup_timeout`] for the first run on a machine that still has to pull the
//! images, which is the slow part.
//!
//! The images are pinned by tag and digest. [`ContainerImage`] can replace them, but an image this
//! crate has not been tested against may not honour the same arguments.
//!
//! # Liquid fixtures differ from a running Nigiri Liquid node
//!
//! A Liquid fixture connects its genesis outputs to the UTXO set, so its wallet holds the full
//! 21,000,000 L-BTC of free coins. Nigiri does not, and its wallet reports a zero L-BTC balance.
//! The chain itself is identical — same genesis, same dynamic-federation parameters — but a
//! fixture is funded and Nigiri's node is not.

mod chain;
mod deadline;
mod diagnostics;
mod electrs;
mod electrum;
mod endpoint;
mod error;
mod fixture;
mod image;
mod node;
mod owned_start;
mod readiness;

pub use chain::FixtureChain;
pub use endpoint::ElectrumEndpoint;
pub use error::FixtureError;
pub use fixture::{Fixture, FixtureBuilder};
pub use image::ContainerImage;
pub use nigiri_rs::{Bitcoin, Liquid};

/// The fixture's regtest RPC credentials.
///
/// Declared once for the whole crate: the service requests, the client configuration, and the
/// redaction patterns all derive from these, so they cannot drift apart.
pub(crate) const RPC_USER: &str = "admin1";
pub(crate) const RPC_PASSWORD: &str = "123";

/// The Nigiri release whose topology the pinned images reproduce.
///
/// Recorded so a future divergence can be traced to a version rather than guessed at. It is
/// documentation, not a runtime check: nothing here talks to Nigiri.
pub const REPRODUCED_NIGIRI_VERSION: &str = "nigiri-v0.5.17";
