//! The public fixture: one funded, synchronized regtest stack per instance, generic over chain.

use std::{fmt, marker::PhantomData, time::Duration};

use nigiri_rs_core::NigiriClient;
use testcontainers::{ContainerAsync, GenericImage};
use uuid::Uuid;

use crate::{
    ContainerImage, ElectrumEndpoint, FixtureError, chain::FixtureChain, deadline::Deadline,
    electrs, node, owned_start::attach_container_log, readiness,
};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// A running regtest stack with a funded wallet, ready to be queried.
///
/// Dropping the fixture removes everything it created. The field order is deliberate: Electrs is
/// dropped before the node, so the indexer is gone before the node it indexes disappears underneath
/// it.
pub struct Fixture<C: FixtureChain> {
    handles: ContainerHandles<ContainerAsync<GenericImage>, ContainerAsync<GenericImage>>,
    client: NigiriClient<C>,
    /// The UUID-scoped names of everything this fixture created.
    ///
    /// Retained for two callers: the teardown test names what must no longer exist once this is
    /// dropped, and a composite reads the network and node names to attach its own containers and
    /// point them at the node.
    names: TopologyNames,
}

/// The fixture's container handles, held only for their `Drop`.
///
/// Declaring them in one place makes the order a property of a type a test can drop, rather than of
/// two adjacent fields nothing checks. Rust drops fields in declaration order, so the indexer goes
/// first and is never left pointed at a node that has already disappeared.
struct ContainerHandles<Indexer, Node> {
    electrs: Indexer,
    node: Node,
}

// Written by hand rather than derived: the held client's configuration carries the RPC password,
// and the rest of the crate works to keep that out of any caller-visible text.
impl<C: FixtureChain> fmt::Debug for Fixture<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Fixture")
            .field("chain", &C::CHAIN_NAME)
            .field("electrum_endpoint", &self.electrum_endpoint())
            .finish_non_exhaustive()
    }
}

impl<C: FixtureChain> Fixture<C> {
    /// A builder carrying the pinned images and the 60-second startup budget.
    #[must_use]
    pub fn builder() -> FixtureBuilder<C> {
        FixtureBuilder {
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            node_image: C::node_image_default(),
            electrs_image: C::electrs_image_default(),
            extra_node_args: Vec::new(),
            network: None,
            chain: PhantomData,
        }
    }

    /// Starts a fixture with the pinned defaults.
    pub async fn start() -> Result<Self, FixtureError> {
        Self::builder().start().await
    }

    /// A client whose wallet already holds the proceeds of the chain's initial funding.
    #[must_use]
    pub fn client(&self) -> &NigiriClient<C> {
        &self.client
    }

    /// The mapped Electrum endpoint, for callers that speak the protocol directly.
    #[must_use]
    pub fn electrum_endpoint(&self) -> &ElectrumEndpoint {
        self.client.electrum_endpoint()
    }

    /// The Docker network every container of this fixture is attached to.
    ///
    /// Crate-private: a composite attaches its own containers to it. The name is an implementation
    /// detail of this crate's topology and is deliberately not public.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the composites that call this — PegPair, LightningStack — land after it"
        )
    )]
    pub(crate) fn network_name(&self) -> &str {
        &self.names.network
    }

    /// The node's container name, which sibling containers dial it by on the fixture network.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the composites that call this — PegPair, LightningStack — land after it"
        )
    )]
    pub(crate) fn node_container_name(&self) -> &str {
        &self.names.node
    }

    /// Adds the inner stack's container logs to a composite's failure.
    ///
    /// A composite's own daemon can only be explained together with the node it followed and the
    /// indexer beside it, and those handles are private to this type. The order matches `start`'s
    /// own failure path: the indexer first, then the node, so the node's log — the service most
    /// failures come back to — ends up nearest the error text.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the composites that call this — PegPair, LightningStack — land after it"
        )
    )]
    pub(crate) async fn attach_inner_logs(&self, error: FixtureError) -> FixtureError {
        let with_electrs =
            attach_container_log(electrs::SERVICE, error, &self.handles.electrs).await;
        attach_container_log(C::NODE_SERVICE, with_electrs, &self.handles.node).await
    }

    /// The Docker ids of this fixture's containers, indexer first.
    ///
    /// Test-only, and only for a composite's teardown test: proving a pair removes everything it
    /// created means naming all four containers, and these handles are private to this type.
    #[cfg(test)]
    #[expect(
        dead_code,
        reason = "the composite teardown test that calls this lands in the next task"
    )]
    pub(crate) fn container_ids(&self) -> [String; 2] {
        [
            self.handles.electrs.id().to_owned(),
            self.handles.node.id().to_owned(),
        ]
    }
}

#[derive(Clone, Debug)]
pub struct FixtureBuilder<C: FixtureChain> {
    startup_timeout: Duration,
    node_image: ContainerImage,
    electrs_image: ContainerImage,
    /// Appended to `C::node_cmd()` by a composite that needs arguments the standalone chain does
    /// not set. Crate-private: a composite in this crate supplies them, and a caller who wants a
    /// differently-configured node replaces the image instead.
    extra_node_args: Vec<String>,
    /// The network to join instead of creating one, supplied by a composite.
    network: Option<String>,
    chain: PhantomData<C>,
}

/// The UUID-scoped names of one fixture's Docker resources.
#[derive(Debug)]
struct TopologyNames {
    network: String,
    node: String,
    electrs: String,
}

/// Scopes every Docker resource of one fixture to a single UUID, so concurrent fixtures cannot
/// collide and a leaked resource is traceable to the fixture that made it.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the thin wrapper survives for the naming unit test while start_under calls topology_names_on"
    )
)]
fn topology_names<C: FixtureChain>() -> TopologyNames {
    topology_names_on::<C>(None)
}

/// The same scoping, with a composite's shared network in place of a generated one.
///
/// Only the network is replaceable. The container names keep their own scope even when the network
/// is shared, which is what lets two fixtures live on one network.
fn topology_names_on<C: FixtureChain>(shared_network: Option<String>) -> TopologyNames {
    let scope = Uuid::new_v4().simple().to_string();

    TopologyNames {
        network: shared_network.unwrap_or_else(|| format!("nigiri-rs-fixture-{scope}")),
        node: format!("{}-{scope}", C::NODE_NAME_PREFIX),
        electrs: format!("nigiri-rs-electrs-{scope}"),
    }
}

impl<C: FixtureChain> FixtureBuilder<C> {
    /// Overrides the budget for the whole startup, not for any single step within it.
    #[must_use]
    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    #[must_use]
    pub fn node_image(mut self, image: ContainerImage) -> Self {
        self.node_image = image;
        self
    }

    #[must_use]
    pub fn electrs_image(mut self, image: ContainerImage) -> Self {
        self.electrs_image = image;
        self
    }

    /// Arguments appended to the chain's own node command.
    ///
    /// Crate-private, and deliberately additive: a composite extends the chain's flag vector and
    /// must not have to restate the flags the chain owns.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the composites that call this — PegPair, LightningStack — land after it"
        )
    )]
    pub(crate) fn extra_node_args(mut self, args: Vec<String>) -> Self {
        self.extra_node_args = args;
        self
    }

    /// Attaches this fixture to a network that already exists rather than creating its own.
    ///
    /// Crate-private: a composite starts its first fixture, reads [`Fixture::network_name`], and
    /// hands it here, so all four containers share one network and resolve each other by container
    /// name. Only the network is shared — the node and indexer keep their own UUID scope, so two
    /// fixtures on one network still cannot collide.
    ///
    /// The network outlives whichever fixture drops first without anything here counting
    /// references: `testcontainers` keys created networks by name and removes one only once the
    /// last container holding it is gone.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the composites that call this — PegPair, LightningStack — land after it"
        )
    )]
    pub(crate) fn network(mut self, name: String) -> Self {
        self.network = Some(name);
        self
    }

    /// Starts the node, funds a wallet, starts Electrs, and returns only once all three services
    /// agree on the tip.
    ///
    /// One `Deadline` covers everything after validation, so a slow phase spends budget the later
    /// phases no longer have, rather than each phase getting a fresh clock.
    pub async fn start(self) -> Result<Fixture<C>, FixtureError> {
        let deadline = Deadline::new(self.startup_timeout)?;
        self.start_under(&deadline).await
    }

    /// Starts under a clock the caller already owns.
    ///
    /// A composite bounds its whole stack with one `Deadline` and passes it here, so the inner
    /// fixture spends the same budget rather than running a second one beside it. `startup_timeout`
    /// is ignored on this path: the caller's clock is the only one.
    pub(crate) async fn start_under(self, deadline: &Deadline) -> Result<Fixture<C>, FixtureError> {
        self.node_image.validate()?;
        self.electrs_image.validate()?;

        let names = topology_names_on::<C>(self.network.clone());

        let node = node::start_node::<C>(
            &self.node_image,
            &names.network,
            &names.node,
            &self.extra_node_args,
            deadline,
        )
        .await?;

        let electrs = match electrs::start_electrs::<C>(
            &self.electrs_image,
            &names.network,
            &names.electrs,
            &names.node,
            deadline,
        )
        .await
        {
            Ok(electrs) => electrs,
            // The node is running and holds the only account of what Electrs was pointed at.
            Err(error) => {
                return Err(attach_container_log(C::NODE_SERVICE, error, &node.container).await);
            }
        };

        // The node client cannot be reconfigured in place, so both endpoints Electrs just
        // published are applied to a copy of the wallet-scoped configuration.
        let mut client_config = node.client_config.clone();
        client_config.esplora_url = electrs.esplora_url.clone();
        client_config.electrum = electrs.electrum_endpoint.clone();
        let client = node::fixture_client::<C>(client_config)?;

        if let Err(not_ready) = readiness::wait_for_sync::<C>(&client, deadline).await {
            // Whichever service fell behind, its own log is what explains why.
            let with_electrs =
                attach_container_log(electrs::SERVICE, not_ready, &electrs.container).await;
            return Err(attach_container_log(C::NODE_SERVICE, with_electrs, &node.container).await);
        }

        Ok(Fixture {
            handles: ContainerHandles {
                electrs: electrs.container,
                node: node.container,
            },
            client,
            names,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nigiri_rs_core::{Bitcoin, Liquid};

    use super::{ContainerHandles, Fixture};
    use crate::{ContainerImage, FixtureChain, FixtureError};

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
            node: DropOrderRecorder {
                name: "node",
                order: std::sync::Arc::clone(&order),
            },
        });

        assert_eq!(
            *order.lock().expect("the recorded order is never poisoned"),
            ["electrs", "node"]
        );
    }

    // Catches a regression that changes what a caller gets without asking for anything: the pinned
    // images and the 60-second budget the whole design is bounded by.
    #[test]
    fn builder_defaults_are_pinned_and_sixty_seconds() {
        let builder = Fixture::<Bitcoin>::builder();

        assert_eq!(builder.startup_timeout, Duration::from_secs(60));
        assert_eq!(builder.node_image, ContainerImage::bitcoind_default());
        assert_eq!(builder.electrs_image, ContainerImage::electrs_default());
    }

    // Catches a regression that drops a builder override, which would silently start the pinned image
    // a caller had explicitly replaced.
    #[test]
    fn builder_methods_return_updated_values() {
        let image = ContainerImage::new("registry.invalid/bitcoin", "30");
        let electrs = ContainerImage::new("registry.invalid/electrs", "latest");

        let builder = Fixture::<Bitcoin>::builder()
            .startup_timeout(Duration::from_secs(90))
            .node_image(image.clone())
            .electrs_image(electrs.clone());

        assert_eq!(builder.startup_timeout, Duration::from_secs(90));
        assert_eq!(builder.node_image, image);
        assert_eq!(builder.electrs_image, electrs);
    }

    // Catches a regression that drops a composite's extra node arguments between the builder and the
    // container request, which would start a node silently missing the flags the composite needs.
    #[test]
    fn extra_node_args_reach_the_builder_and_default_to_none() {
        let default = Fixture::<Bitcoin>::builder();
        assert!(default.extra_node_args.is_empty());

        let extended = Fixture::<Bitcoin>::builder()
            .extra_node_args(vec!["-zmqpubrawblock=tcp://0.0.0.0:28332".to_owned()]);
        assert_eq!(
            extended.extra_node_args,
            vec!["-zmqpubrawblock=tcp://0.0.0.0:28332".to_owned()]
        );
    }

    // Catches a regression that defers image validation until Docker has already been asked to start
    // something, or that starts a fixture whose budget cannot bound anything.
    #[tokio::test]
    async fn invalid_inputs_are_rejected_before_any_container_is_started() {
        let rejected = [
            Fixture::<Bitcoin>::builder().node_image(ContainerImage::new("", "v30.0")),
            Fixture::<Bitcoin>::builder()
                .electrs_image(ContainerImage::new("registry.invalid/e", "")),
            Fixture::<Bitcoin>::builder().startup_timeout(Duration::ZERO),
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
    async fn assert_dropping_a_fixture_removes_every_resource_it_created<C: FixtureChain>() {
        use testcontainers::bollard::{Docker, models::MountPointTypeEnum};

        let fixture = Fixture::<C>::start()
            .await
            .expect("a pinned fixture must start against a real daemon");
        let node = fixture.handles.node.id().to_owned();
        let electrs = fixture.handles.electrs.id().to_owned();
        let network = fixture.names.network.clone();

        let docker = Docker::connect_with_local_defaults()
            .expect("the daemon that just served the fixture is reachable");

        // Every mount must be a volume Docker created for this container alone. A bind, a named
        // volume, or a host path would outlive the fixture or be shared with something else.
        let mut volumes = Vec::new();
        for container in [&node, &electrs] {
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
        // Guards against this test passing vacuously, without asserting an image fact that goes
        // stale the moment a pin changes: how many anonymous volumes a fixture owns is decided by
        // how many `VOLUME`s its images declare, so the expectation is read from the images
        // themselves. Nigiri's Elements image declared one and Blockstream's declares none; either
        // is fine, and mounting one Docker did not create for this container alone is not.
        let mut declared = 0_usize;
        for image in [C::node_image_default(), C::electrs_image_default()] {
            let reference = format!("{}:{}", image.name(), image.testcontainers_tag());
            let inspected = docker
                .inspect_image(&reference)
                .await
                .expect("a pinned image a fixture just ran can be inspected");
            declared += inspected
                .config
                .and_then(|config| config.volumes)
                .map_or(0, |volumes| volumes.len());
        }
        assert_eq!(
            volumes.len(),
            declared,
            "a {} fixture must own exactly one anonymous volume per volume its images declare",
            C::CHAIN_NAME
        );
        println!(
            "created {}={node} electrs={electrs} network={network} volumes={volumes:?}",
            C::CHAIN_NAME
        );

        drop(fixture);

        // Removal is asynchronous, so this polls rather than asserting once.
        let mut outstanding = Vec::new();
        for _ in 0..100 {
            outstanding.clear();
            for container in [&node, &electrs] {
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

    #[tokio::test]
    async fn dropping_a_bitcoin_fixture_removes_every_resource_it_created() {
        assert_dropping_a_fixture_removes_every_resource_it_created::<Bitcoin>().await;
    }

    #[tokio::test]
    async fn dropping_a_liquid_fixture_removes_every_resource_it_created() {
        assert_dropping_a_fixture_removes_every_resource_it_created::<Liquid>().await;
    }

    // The one test that proves the whole assembly against a real daemon: a fixture that reports
    // itself ready must already be funded and reachable through its mapped Electrum port.
    #[tokio::test]
    async fn fixture_starts_ready() {
        let fixture = Fixture::<Bitcoin>::start()
            .await
            .expect("a pinned fixture must start against a real daemon");

        let height = fixture
            .client()
            .block_height()
            .await
            .expect("a ready fixture must serve its Esplora tip");
        assert_eq!(height, 101, "a ready fixture must already be funded");

        let port = fixture.electrum_endpoint().port();
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
        let first = super::topology_names::<Bitcoin>();
        let second = super::topology_names::<Bitcoin>();

        assert_ne!(first.network, second.network);
        assert_ne!(first.node, second.node);
        assert_ne!(first.electrs, second.electrs);
        assert!(first.network.starts_with("nigiri-rs-fixture-"), "{first:?}");
        assert!(first.node.starts_with("nigiri-rs-bitcoind-"), "{first:?}");
        assert!(first.electrs.starts_with("nigiri-rs-electrs-"), "{first:?}");
        // One UUID scopes the whole topology, so a leaked resource is traceable to one fixture.
        let suffix = first
            .network
            .strip_prefix("nigiri-rs-fixture-")
            .expect("the network name carries the topology suffix");
        assert!(first.node.ends_with(suffix));
        assert!(first.electrs.ends_with(suffix));
    }

    // Catches a regression that discards the node's container name, which a composite needs to point
    // its own daemons at the node over the fixture network. The mapped host port is not a substitute:
    // sibling containers dial the node by name on the user-defined network, not through the host.
    #[tokio::test]
    async fn a_started_fixture_reports_the_topology_names_a_composite_must_dial() {
        let fixture = Fixture::<Bitcoin>::start()
            .await
            .expect("a pinned fixture must start against a real daemon");

        assert!(
            fixture.network_name().starts_with("nigiri-rs-fixture-"),
            "{}",
            fixture.network_name()
        );
        assert!(
            fixture
                .node_container_name()
                .starts_with("nigiri-rs-bitcoind-"),
            "{}",
            fixture.node_container_name()
        );
        // One UUID scopes the whole topology, so a composite can trace every resource to one fixture.
        let suffix = fixture
            .network_name()
            .strip_prefix("nigiri-rs-fixture-")
            .expect("the network name carries the topology suffix");
        assert!(fixture.node_container_name().ends_with(suffix));

        drop(fixture);
    }

    // Catches a regression that gives the inner fixture a fresh clock instead of the caller's. A
    // composite bounds its whole stack with one Deadline; an inner fixture that ignores it would let
    // a slow node start spend budget the composite's later phases still believe they have.
    #[tokio::test(start_paused = true)]
    async fn start_under_reports_the_callers_exhausted_budget_not_its_own() {
        let deadline = crate::deadline::Deadline::new(Duration::from_secs(30))
            .expect("a positive deadline is valid");
        tokio::time::advance(Duration::from_secs(30)).await;

        let error = Fixture::<Bitcoin>::builder()
            .start_under(&deadline)
            .await
            .expect_err("an exhausted caller budget must not start a container");

        let FixtureError::ReadinessTimeout { duration, .. } = error else {
            panic!("an exhausted caller budget must surface as a readiness timeout: {error}");
        };
        assert_eq!(
            duration,
            Duration::from_secs(30),
            "the reported budget must be the caller's 30s, not the builder's own 60s default"
        );
    }

    // Catches a regression that leaves a composite's failure without the inner stack's evidence. A
    // daemon that never syncs is explained by the node it was following, whose log lives behind a
    // handle only the fixture owns.
    #[tokio::test]
    async fn inner_logs_are_attached_to_a_composites_error() {
        let fixture = Fixture::<Bitcoin>::start()
            .await
            .expect("a pinned fixture must start against a real daemon");

        let bare = FixtureError::ReadinessTimeout {
            service: "lnd",
            duration: Duration::from_secs(180),
            last_observation: "waiting for synced_to_chain".to_owned(),
            diagnostics: String::new(),
        };
        let enriched = fixture.attach_inner_logs(bare).await;

        let FixtureError::ReadinessTimeout {
            service,
            diagnostics,
            ..
        } = enriched
        else {
            panic!("attaching logs must not change which error it is");
        };
        assert_eq!(service, "lnd", "the failing service must stay named");
        assert!(
            diagnostics.contains("bitcoind"),
            "the node's log must be attached: {diagnostics:.256}"
        );
        assert!(
            !diagnostics.contains("admin1:123"),
            "attached logs must stay redacted"
        );

        drop(fixture);
    }

    // Catches a regression that lets a composite's shared network also share the container names.
    // Two fixtures on one network still need distinct containers, or the second collides with the
    // first on the host.
    #[test]
    fn a_shared_network_replaces_only_the_network_name() {
        let shared = "nigiri-rs-fixture-shared".to_owned();

        let first = super::topology_names_on::<Bitcoin>(Some(shared.clone()));
        let second = super::topology_names_on::<Liquid>(Some(shared.clone()));

        assert_eq!(first.network, shared);
        assert_eq!(second.network, shared);
        assert_ne!(first.node, second.node);
        assert_ne!(first.electrs, second.electrs);
        assert!(first.node.starts_with("nigiri-rs-bitcoind-"), "{first:?}");
        assert!(second.node.starts_with("nigiri-rs-elements-"), "{second:?}");
    }

    // Catches a regression that makes a shared network the default, which would put every
    // standalone fixture on one network and let unrelated tests reach each other's nodes.
    #[test]
    fn without_a_shared_network_a_fixture_scopes_its_own() {
        assert!(
            super::topology_names_on::<Bitcoin>(None)
                .network
                .starts_with("nigiri-rs-fixture-")
        );
        assert!(Fixture::<Bitcoin>::builder().network.is_none());
        assert_eq!(
            Fixture::<Bitcoin>::builder()
                .network("nigiri-rs-fixture-shared".to_owned())
                .network
                .as_deref(),
            Some("nigiri-rs-fixture-shared")
        );
    }

    // Catches a regression that starts a fixture on a network other than the one it was given,
    // which would leave a composite's containers unable to resolve each other by name.
    #[tokio::test]
    async fn a_fixture_started_on_a_shared_network_reports_that_network() {
        let first = Fixture::<Bitcoin>::start()
            .await
            .expect("a pinned fixture must start against a real daemon");
        let shared = first.network_name().to_owned();

        let deadline = crate::deadline::Deadline::new(Duration::from_secs(120))
            .expect("a positive deadline is valid");
        let second = Fixture::<Bitcoin>::builder()
            .network(shared.clone())
            .start_under(&deadline)
            .await
            .expect("a second fixture must join an existing fixture network");

        assert_eq!(second.network_name(), shared);
        assert_ne!(second.node_container_name(), first.node_container_name());

        drop(second);
        drop(first);
    }
}
