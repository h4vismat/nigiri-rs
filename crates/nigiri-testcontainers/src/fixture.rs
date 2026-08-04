//! The public fixture: one funded, synchronized Bitcoin regtest stack per instance.

use std::{fmt, time::Duration};

use nigiri_rs::{Bitcoin, NigiriClient};
use testcontainers::{ContainerAsync, GenericImage};
use uuid::Uuid;

use crate::{
    ContainerImage, ElectrumEndpoint, FixtureError, chain::FixtureChain, electrs, node,
    owned_start::attach_container_log, readiness,
};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// A running Bitcoin regtest stack with a funded wallet, ready to be queried.
///
/// Dropping the fixture removes everything it created. The field order is deliberate: Electrs is
/// dropped before Bitcoind, so the indexer is gone before the node it indexes disappears underneath
/// it.
pub struct BitcoinFixture {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the handles exist only to reap their containers when the fixture is dropped"
        )
    )]
    handles: ContainerHandles<ContainerAsync<GenericImage>, ContainerAsync<GenericImage>>,
    client: NigiriClient<Bitcoin>,
    electrum_endpoint: ElectrumEndpoint,
    /// Retained so the teardown test can name what must no longer exist once this is dropped.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the network is reaped by Testcontainers; its name is only read when proving that"
        )
    )]
    network: String,
}

/// The fixture's container handles, held only for their `Drop`.
///
/// Declaring them in one place makes the order a property of a type a test can drop, rather than of
/// two adjacent fields nothing checks. Rust drops fields in declaration order, so the indexer goes
/// first and is never left pointed at a node that has already disappeared.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "these handles exist only to reap their containers when the fixture is dropped"
    )
)]
struct ContainerHandles<Indexer, Node> {
    electrs: Indexer,
    bitcoin: Node,
}

// Written by hand rather than derived: the held client's configuration carries the RPC password,
// and the rest of the crate works to keep that out of any caller-visible text.
impl fmt::Debug for BitcoinFixture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BitcoinFixture")
            .field("electrum_endpoint", &self.electrum_endpoint)
            .finish_non_exhaustive()
    }
}

impl BitcoinFixture {
    /// A builder carrying the pinned images and the 60-second startup budget.
    #[must_use]
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
    #[must_use]
    pub fn client(&self) -> &NigiriClient<Bitcoin> {
        &self.client
    }

    /// The mapped Electrum endpoint, for callers that speak the protocol directly.
    #[must_use]
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
    #[must_use]
    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    #[must_use]
    pub fn bitcoind_image(mut self, image: ContainerImage) -> Self {
        self.bitcoind_image = image;
        self
    }

    #[must_use]
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

        let bitcoin = node::start_node::<Bitcoin>(
            &self.bitcoind_image,
            &names.network,
            &names.bitcoind,
            &deadline,
        )
        .await?;

        let electrs = match electrs::start_electrs::<Bitcoin>(
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
                return Err(attach_container_log(
                    <Bitcoin as FixtureChain>::NODE_SERVICE,
                    error,
                    &bitcoin.container,
                )
                .await);
            }
        };

        // The node client cannot be reconfigured in place, so the Esplora base URL Electrs just
        // published is applied to a copy of the wallet-scoped configuration.
        let mut client_config = bitcoin.client_config.clone();
        client_config.esplora_url = electrs.esplora_url.clone();
        let client = node::fixture_client::<Bitcoin>(client_config)?;

        if let Err(not_ready) =
            readiness::wait_for_sync(&client, &electrs.electrum_endpoint, &deadline).await
        {
            // Whichever service fell behind, its own log is what explains why.
            let with_electrs =
                attach_container_log(electrs::SERVICE, not_ready, &electrs.container).await;
            return Err(attach_container_log(
                <Bitcoin as FixtureChain>::NODE_SERVICE,
                with_electrs,
                &bitcoin.container,
            )
            .await);
        }

        Ok(BitcoinFixture {
            handles: ContainerHandles {
                electrs: electrs.container,
                bitcoin: bitcoin.container,
            },
            client,
            electrum_endpoint: electrs.electrum_endpoint,
            network: names.network,
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

    use super::{BitcoinFixture, BitcoinFixtureBuilder, ContainerHandles};
    use crate::{ContainerImage, FixtureError};

    /// Reports the order in which the fixture released its handles.
    struct DropOrderRecorder {
        name: &'static str,
        order: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl Drop for DropOrderRecorder {
        fn drop(&mut self) {
            self.order
                .lock()
                .expect("the recorded order is never poisoned")
                .push(self.name);
        }
    }

    // Catches a regression that reorders the fixture's handles. Electrs must be released before the
    // node it indexes, or the indexer is briefly pointed at a container that no longer exists.
    #[test]
    fn the_indexer_handle_is_released_before_the_node_it_indexes() {
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        drop(ContainerHandles {
            electrs: DropOrderRecorder {
                name: "electrs",
                order: std::sync::Arc::clone(&order),
            },
            bitcoin: DropOrderRecorder {
                name: "bitcoind",
                order: std::sync::Arc::clone(&order),
            },
        });

        assert_eq!(
            *order.lock().expect("the recorded order is never poisoned"),
            ["electrs", "bitcoind"]
        );
    }

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

    /// The label Docker puts on a volume it created implicitly for a container.
    const DOCKER_ANONYMOUS_VOLUME_LABEL: &str = "com.docker.volume.anonymous";

    // Catches a regression that leaves a container, volume, or network behind, and one that gives a
    // fixture storage outliving it. "Ephemeral" is the crate's whole promise, and it can only be
    // checked from inside: the identifiers of what a fixture created are deliberately not public.
    //
    // Storage is asserted before the drop and absence after it, in one fixture, because starting a
    // second one to check the other half would double the slowest test in the suite.
    #[tokio::test]
    #[ignore = "requires Docker and pulls pinned Bitcoin images"]
    async fn dropping_a_fixture_removes_every_resource_it_created() {
        use testcontainers::bollard::{Docker, models::MountPointTypeEnum};

        let fixture = BitcoinFixture::start()
            .await
            .expect("a pinned fixture must start against a real daemon");
        let bitcoind = fixture.handles.bitcoin.id().to_owned();
        let electrs = fixture.handles.electrs.id().to_owned();
        let network = fixture.network.clone();

        let docker = Docker::connect_with_local_defaults()
            .expect("the daemon that just served the fixture is reachable");

        // Every mount must be a volume Docker created for this container alone. A bind, a named
        // volume, or a host path would outlive the fixture or be shared with something else.
        let mut volumes = Vec::new();
        for container in [&bitcoind, &electrs] {
            let inspected = docker
                .inspect_container(container, None)
                .await
                .expect("a running fixture container can be inspected");

            for mount in inspected.mounts.unwrap_or_default() {
                assert_eq!(
                    mount.typ,
                    Some(MountPointTypeEnum::VOLUME),
                    "a fixture may only mount Docker volumes: {mount:?}"
                );
                let name = mount.name.clone().expect("a volume mount names its volume");
                assert_eq!(
                    name.len(),
                    64,
                    "a fixture volume must be anonymous, so Docker names it by digest: {name}"
                );
                assert!(
                    name.chars().all(|character| character.is_ascii_hexdigit()),
                    "an anonymous volume name is hexadecimal: {name}"
                );

                let volume = docker
                    .inspect_volume(&name)
                    .await
                    .expect("a mounted volume can be inspected");
                assert_eq!(volume.driver, "local", "{name} must use local storage");
                assert_eq!(
                    volume.scope.map(|scope| scope.to_string()),
                    Some("local".to_owned())
                );
                assert_eq!(
                    volume.labels.keys().collect::<Vec<_>>(),
                    vec![DOCKER_ANONYMOUS_VOLUME_LABEL],
                    "{name} carries labels beyond Docker's anonymous marker, so it is not a volume \
                     this fixture created"
                );
                assert!(
                    volume.options.is_empty(),
                    "{name} configures storage options, so it is not ephemeral: {:?}",
                    volume.options
                );

                volumes.push(name);
            }
        }
        assert!(
            !volumes.is_empty(),
            "the pinned Bitcoind image declares an anonymous volume, so at least one is expected"
        );
        println!(
            "created bitcoind={bitcoind} electrs={electrs} network={network} volumes={volumes:?}"
        );

        drop(fixture);

        // Removal is asynchronous, so this polls rather than asserting once.
        let mut outstanding = Vec::new();
        for _ in 0..100 {
            outstanding.clear();
            for container in [&bitcoind, &electrs] {
                if docker.inspect_container(container, None).await.is_ok() {
                    outstanding.push(container.clone());
                }
            }
            if docker.inspect_network(&network, None).await.is_ok() {
                outstanding.push(network.clone());
            }
            for volume in &volumes {
                if docker.inspect_volume(volume).await.is_ok() {
                    outstanding.push(volume.clone());
                }
            }
            if outstanding.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        assert!(
            outstanding.is_empty(),
            "dropping the fixture left these behind: {outstanding:?}"
        );
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
