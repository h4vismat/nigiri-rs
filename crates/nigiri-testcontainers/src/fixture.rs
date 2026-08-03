//! The public fixture: one funded, synchronized Bitcoin regtest stack per instance.

use std::time::Duration;

use nigiri_rs::{Bitcoin, NigiriClient};
use testcontainers::{ContainerAsync, GenericImage};
use uuid::Uuid;

use crate::{
    ContainerImage, ElectrumEndpoint, FixtureError, bitcoind, electrs,
    owned_start::attach_container_log, readiness,
};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// A running Bitcoin regtest stack with a funded wallet, ready to be queried.
///
/// Dropping the fixture removes everything it created. The field order is deliberate: Electrs is
/// dropped before Bitcoind, so the indexer is gone before the node it indexes disappears underneath
/// it.
#[derive(Debug)]
pub struct BitcoinFixture {
    // Held for their `Drop`, which is what removes the containers, in this order.
    #[expect(
        dead_code,
        reason = "the handle's only job is to reap its container when the fixture is dropped"
    )]
    electrs: ContainerAsync<GenericImage>,
    #[expect(
        dead_code,
        reason = "the handle's only job is to reap its container when the fixture is dropped"
    )]
    bitcoin: ContainerAsync<GenericImage>,
    client: NigiriClient<Bitcoin>,
    electrum_endpoint: ElectrumEndpoint,
}

impl BitcoinFixture {
    /// A builder carrying the pinned images and the 60-second startup budget.
    pub fn builder() -> BitcoinFixtureBuilder {
        BitcoinFixtureBuilder {
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            bitcoind_image: ContainerImage::bitcoind_default(),
            electrs_image: ContainerImage::electrs_default(),
        }
    }

    /// Starts a fixture with the pinned defaults.
    pub async fn start() -> Result<Self, FixtureError> {
        Self::builder().start().await
    }

    /// A client whose wallet already holds the proceeds of 101 mined blocks.
    pub fn client(&self) -> &NigiriClient<Bitcoin> {
        &self.client
    }

    /// The mapped Electrum endpoint, for callers that speak the protocol directly.
    pub fn electrum_endpoint(&self) -> &ElectrumEndpoint {
        &self.electrum_endpoint
    }
}

#[derive(Clone, Debug)]
pub struct BitcoinFixtureBuilder {
    startup_timeout: Duration,
    bitcoind_image: ContainerImage,
    electrs_image: ContainerImage,
}

/// The UUID-scoped names of one fixture's Docker resources.
#[derive(Debug)]
struct TopologyNames {
    network: String,
    bitcoind: String,
    electrs: String,
}

impl BitcoinFixtureBuilder {
    /// Overrides the budget for the whole startup, not for any single step within it.
    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    pub fn bitcoind_image(mut self, image: ContainerImage) -> Self {
        self.bitcoind_image = image;
        self
    }

    pub fn electrs_image(mut self, image: ContainerImage) -> Self {
        self.electrs_image = image;
        self
    }

    /// Starts Bitcoind, funds a wallet, starts Electrs, and returns only once all three services
    /// agree on the tip.
    ///
    /// One `Deadline` covers everything after validation, so a slow phase spends budget the later
    /// phases no longer have, rather than each phase getting a fresh clock.
    pub async fn start(self) -> Result<BitcoinFixture, FixtureError> {
        self.bitcoind_image.validate()?;
        self.electrs_image.validate()?;

        let names = Self::topology_names();
        let deadline = crate::deadline::Deadline::new(self.startup_timeout)?;

        let bitcoin = bitcoind::start_bitcoind(
            &self.bitcoind_image,
            names.network.clone(),
            names.bitcoind.clone(),
            &deadline,
        )
        .await?;

        let electrs = match electrs::start_electrs(
            &self.electrs_image,
            &names.network,
            &names.electrs,
            &names.bitcoind,
            &deadline,
        )
        .await
        {
            Ok(electrs) => electrs,
            // Bitcoind is running and holds the only account of what Electrs was pointed at.
            Err(error) => {
                return Err(attach_container_log("bitcoind", error, &bitcoin.container).await);
            }
        };

        // The node client cannot be reconfigured in place, so the Esplora base URL Electrs just
        // published is applied to a copy of the wallet-scoped configuration.
        let mut client_config = bitcoin.client_config.clone();
        client_config.esplora_url = electrs.esplora_url.clone();
        let client = NigiriClient::<Bitcoin>::with_config(client_config).map_err(|source| {
            FixtureError::InvalidConfiguration {
                detail: crate::diagnostics::redacted_head(
                    &format!("fixture client configuration was rejected: {source}"),
                    crate::diagnostics::MAX_SOURCE_BYTES,
                ),
            }
        })?;

        if let Err(not_ready) =
            readiness::wait_for_sync(&client, &electrs.electrum_endpoint, &deadline).await
        {
            // Whichever service fell behind, its own log is what explains why.
            let with_electrs = attach_container_log("electrs", not_ready, &electrs.container).await;
            return Err(attach_container_log("bitcoind", with_electrs, &bitcoin.container).await);
        }

        Ok(BitcoinFixture {
            electrs: electrs.container,
            bitcoin: bitcoin.container,
            client,
            electrum_endpoint: electrs.electrum_endpoint,
        })
    }

    /// Scopes every Docker resource of one fixture to a single UUID, so concurrent fixtures cannot
    /// collide and a leaked resource is traceable to the fixture that made it.
    fn topology_names() -> TopologyNames {
        let scope = Uuid::new_v4().simple().to_string();

        TopologyNames {
            network: format!("nigiri-rs-fixture-{scope}"),
            bitcoind: format!("nigiri-rs-bitcoind-{scope}"),
            electrs: format!("nigiri-rs-electrs-{scope}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{BitcoinFixture, BitcoinFixtureBuilder};
    use crate::{ContainerImage, FixtureError};

    // Catches a regression that changes what a caller gets without asking for anything: the pinned
    // images and the 60-second budget the whole design is bounded by.
    #[test]
    fn builder_defaults_are_pinned_and_sixty_seconds() {
        let builder = BitcoinFixture::builder();

        assert_eq!(builder.startup_timeout, Duration::from_secs(60));
        assert_eq!(builder.bitcoind_image, ContainerImage::bitcoind_default());
        assert_eq!(builder.electrs_image, ContainerImage::electrs_default());
    }

    // Catches a regression that drops a builder override, which would silently start the pinned image
    // a caller had explicitly replaced.
    #[test]
    fn builder_methods_return_updated_values() {
        let image = ContainerImage::new("registry.invalid/bitcoin", "30");
        let electrs = ContainerImage::new("registry.invalid/electrs", "latest");

        let builder = BitcoinFixture::builder()
            .startup_timeout(Duration::from_secs(90))
            .bitcoind_image(image.clone())
            .electrs_image(electrs.clone());

        assert_eq!(builder.startup_timeout, Duration::from_secs(90));
        assert_eq!(builder.bitcoind_image, image);
        assert_eq!(builder.electrs_image, electrs);
    }

    // Catches a regression that defers image validation until Docker has already been asked to start
    // something, or that starts a fixture whose budget cannot bound anything.
    #[tokio::test]
    async fn invalid_inputs_are_rejected_before_any_container_is_started() {
        let rejected = [
            BitcoinFixture::builder().bitcoind_image(ContainerImage::new("", "v30.0")),
            BitcoinFixture::builder().electrs_image(ContainerImage::new("registry.invalid/e", "")),
            BitcoinFixture::builder().startup_timeout(Duration::ZERO),
        ];

        for builder in rejected {
            let error = builder
                .start()
                .await
                .expect_err("invalid fixture input must be rejected");
            assert!(
                matches!(error, FixtureError::InvalidConfiguration { .. }),
                "{error}"
            );
        }
    }

    // The one test that proves the whole assembly against a real daemon: a fixture that reports
    // itself ready must already be funded and reachable through its mapped Electrum port.
    #[tokio::test]
    #[ignore = "requires Docker and pulls pinned Bitcoin images"]
    async fn fixture_starts_ready() {
        let fixture = BitcoinFixture::start()
            .await
            .expect("a pinned fixture must start against a real daemon");

        let height = fixture
            .client()
            .block_height()
            .await
            .expect("a ready fixture must serve its Esplora tip");
        assert_eq!(height, 101, "a ready fixture must already be funded");

        let port = fixture.electrum_endpoint().port();
        assert_ne!(port, 0);
        assert_ne!(
            port, 50_000,
            "the Electrum endpoint must be the mapped port, not the fixed container port"
        );
        println!(
            "fixture ready: height={height} electrum={}:{port}",
            fixture.electrum_endpoint().host()
        );

        drop(fixture);
    }

    // Catches a regression that reuses a fixed network or container name, which would make two
    // concurrent fixtures collide on the host instead of running independently.
    #[test]
    fn every_fixture_scopes_its_own_topology_names() {
        let first = BitcoinFixtureBuilder::topology_names();
        let second = BitcoinFixtureBuilder::topology_names();

        assert_ne!(first.network, second.network);
        assert_ne!(first.bitcoind, second.bitcoind);
        assert_ne!(first.electrs, second.electrs);
        assert!(first.network.starts_with("nigiri-rs-fixture-"), "{first:?}");
        assert!(
            first.bitcoind.starts_with("nigiri-rs-bitcoind-"),
            "{first:?}"
        );
        assert!(first.electrs.starts_with("nigiri-rs-electrs-"), "{first:?}");
        // One UUID scopes the whole topology, so a leaked resource is traceable to one fixture.
        let suffix = first
            .network
            .strip_prefix("nigiri-rs-fixture-")
            .expect("the network name carries the topology suffix");
        assert!(first.bitcoind.ends_with(suffix));
        assert!(first.electrs.ends_with(suffix));
    }
}
