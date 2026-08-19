//! Two stateless-wallet LND nodes sharing one synchronized Bitcoin fixture.

use std::{fmt, future::Future, time::Duration};

use nigiri_rs_core::{Bitcoin, NigiriClient};
use nigiri_rs_lnd::{LndBootstrapConfig, LndClient, LndError, NodeInfo, Sats, initialize_wallet};
use url::Url;
use uuid::Uuid;

use crate::{
    ContainerImage, Fixture, FixtureError,
    chain::{FixtureChain, bitcoin_zmq_args},
    deadline::Deadline,
    diagnostics::{redacted_source, redacted_tail},
    lnd::{LND_GRPC_PORT, TLS_CERT_PATH},
    readiness::RETRY_DELAY,
    runtime::{
        ContainerEngine, RuntimeHandle, Startup, attach_container_log, lnd_spec, runtime_error,
        supervise,
    },
};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_CHANNEL_CAPACITY: Sats = Sats::new(2_000_000);
const DEFAULT_PUSH_AMOUNT: Sats = Sats::new(1_000_000);
const MIN_NOMINAL_SIDE_BALANCE: u64 = 100_000;

/// A synchronized Bitcoin fixture and two initialized, authenticated LND clients.
///
/// Task 8 funds these wallets and adds channel/payment readiness. At this phase the clients are
/// authenticated and synchronized, but no channel is promised yet.
pub struct LndPair {
    handles: LndHandles<RuntimeHandle, Fixture<Bitcoin>>,
    alice: LndClient,
    bob: LndClient,
    #[allow(
        dead_code,
        reason = "Task 8 connects Alice to Bob by this private name"
    )]
    names: LndNames,
    #[allow(
        dead_code,
        reason = "Task 8 lifecycle tests inspect these private identifiers"
    )]
    container_ids: [String; 2],
    #[allow(dead_code, reason = "Task 8 funds and opens the configured channel")]
    channel_capacity: Sats,
    #[allow(dead_code, reason = "Task 8 funds and opens the configured channel")]
    push_amount: Sats,
}

/// Dependency-ordered ownership: Rust drops fields in declaration order.
struct LndHandles<LndRuntime, BitcoinStack> {
    lnd: LndRuntime,
    bitcoin: BitcoinStack,
}

impl fmt::Debug for LndPair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LndPair")
            .field("bitcoin", &self.handles.bitcoin)
            .field("alice", &self.alice)
            .field("bob", &self.bob)
            .finish_non_exhaustive()
    }
}

impl LndPair {
    /// Returns a builder with the four approved image pins and a 180-second shared deadline.
    #[must_use]
    pub fn builder() -> LndPairBuilder {
        LndPairBuilder {
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            bitcoind_image: Bitcoin::node_image_default(),
            bitcoin_electrs_image: Bitcoin::electrs_image_default(),
            alice_image: ContainerImage::lnd_default(),
            bob_image: ContainerImage::lnd_default(),
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            push_amount: DEFAULT_PUSH_AMOUNT,
        }
    }

    /// Starts both authenticated, synchronized LND nodes with the pinned defaults.
    pub async fn start() -> Result<Self, FixtureError> {
        Self::builder().start().await
    }

    /// The client for the funded backing Bitcoin fixture.
    #[must_use]
    pub fn bitcoin(&self) -> &NigiriClient<Bitcoin> {
        self.handles.bitcoin.client()
    }

    /// Alice's authenticated LND client.
    #[must_use]
    pub fn alice(&self) -> &LndClient {
        &self.alice
    }

    /// Bob's authenticated LND client.
    #[must_use]
    pub fn bob(&self) -> &LndClient {
        &self.bob
    }

    /// Removes both LND nodes, then the backing Bitcoin stack, attempting both cleanup phases.
    pub async fn shutdown(self) -> Result<(), FixtureError> {
        let Self {
            handles: LndHandles { lnd, bitcoin },
            alice: _,
            bob: _,
            names: _,
            container_ids: _,
            channel_capacity: _,
            push_amount: _,
        } = self;

        let lnd_result = lnd
            .shutdown()
            .await
            .map_err(|error| runtime_error("LND pair", error));
        let bitcoin_result = bitcoin.shutdown().await;
        lnd_result.and(bitcoin_result)
    }

    #[cfg(test)]
    #[allow(dead_code, reason = "Task 8 lifecycle tests consume these identifiers")]
    pub(crate) fn container_ids(&self) -> [String; 4] {
        let [electrs, bitcoind] = self.handles.bitcoin.container_ids();
        [
            self.container_ids[0].clone(),
            self.container_ids[1].clone(),
            electrs,
            bitcoind,
        ]
    }
}

/// Overrides for the shared deadline, four images, and Task 8's channel allocation.
#[derive(Clone, Debug)]
pub struct LndPairBuilder {
    startup_timeout: Duration,
    bitcoind_image: ContainerImage,
    bitcoin_electrs_image: ContainerImage,
    alice_image: ContainerImage,
    bob_image: ContainerImage,
    channel_capacity: Sats,
    push_amount: Sats,
}

impl LndPairBuilder {
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
    pub fn bitcoin_electrs_image(mut self, image: ContainerImage) -> Self {
        self.bitcoin_electrs_image = image;
        self
    }

    #[must_use]
    pub fn alice_image(mut self, image: ContainerImage) -> Self {
        self.alice_image = image;
        self
    }

    #[must_use]
    pub fn bob_image(mut self, image: ContainerImage) -> Self {
        self.bob_image = image;
        self
    }

    #[must_use]
    pub fn channel_capacity(mut self, capacity: Sats) -> Self {
        self.channel_capacity = capacity;
        self
    }

    #[must_use]
    pub fn push_amount(mut self, amount: Sats) -> Self {
        self.push_amount = amount;
        self
    }

    /// Starts the backing fixture and both LND nodes under one shared startup deadline.
    pub async fn start(self) -> Result<LndPair, FixtureError> {
        self.validate()?;
        let deadline = Deadline::new(self.startup_timeout)?;

        let bitcoin = Fixture::<Bitcoin>::builder()
            .node_image(self.bitcoind_image)
            .electrs_image(self.bitcoin_electrs_image)
            .extra_node_args(bitcoin_zmq_args())
            .start_under(&deadline)
            .await?;

        let nodes = start_lnd_nodes_under(
            bitcoin.engine(),
            bitcoin.network_name().to_owned(),
            bitcoin.node_container_name().to_owned(),
            self.alice_image,
            self.bob_image,
            &deadline,
            RealLndConnector,
            bitcoin.client().clone(),
        )
        .await;

        let (started, lnd_runtime) = match nodes {
            Ok(nodes) => nodes,
            Err(error) => return Err(bitcoin.attach_inner_logs(error).await),
        };

        Ok(LndPair {
            handles: LndHandles {
                lnd: lnd_runtime,
                bitcoin,
            },
            alice: started.alice,
            bob: started.bob,
            names: started.names,
            container_ids: started.container_ids,
            channel_capacity: self.channel_capacity,
            push_amount: self.push_amount,
        })
    }

    fn validate(&self) -> Result<(), FixtureError> {
        if self.startup_timeout.is_zero() {
            return Err(invalid("startup deadline must be greater than zero"));
        }
        for image in [
            &self.bitcoind_image,
            &self.bitcoin_electrs_image,
            &self.alice_image,
            &self.bob_image,
        ] {
            image.validate()?;
        }

        let capacity = self.channel_capacity.as_u64();
        let push = self.push_amount.as_u64();
        if push == 0 {
            return Err(invalid("LND channel push amount must be greater than zero"));
        }
        if push >= capacity {
            return Err(invalid(
                "LND channel push amount must be lower than channel capacity",
            ));
        }
        let alice = capacity
            .checked_sub(push)
            .ok_or_else(|| invalid("LND channel allocation underflowed"))?;
        if alice < MIN_NOMINAL_SIDE_BALANCE {
            return Err(invalid(
                "LND channel must leave Alice at least 100000 satoshis",
            ));
        }
        if push < MIN_NOMINAL_SIDE_BALANCE {
            return Err(invalid(
                "LND channel must give Bob at least 100000 satoshis",
            ));
        }
        Ok(())
    }
}

fn invalid(detail: &'static str) -> FixtureError {
    FixtureError::InvalidConfiguration {
        detail: detail.to_owned(),
    }
}

#[derive(Clone)]
struct LndNames {
    alice: String,
    bob: String,
}

impl LndNames {
    fn scoped() -> Self {
        let scope = Uuid::new_v4().simple().to_string();
        Self {
            alice: format!("nigiri-rs-lnd-alice-{scope}"),
            bob: format!("nigiri-rs-lnd-bob-{scope}"),
        }
    }
}

struct StartedLndNodes<Client> {
    alice: Client,
    bob: Client,
    names: LndNames,
    /// Alice then Bob; diagnostics use the reverse dependency order explicitly.
    container_ids: [String; 2],
}

#[derive(Clone, Copy)]
struct LndSyncStatus {
    block_height: u32,
    network_is_regtest: bool,
    synced_to_chain: bool,
    synced_to_graph: bool,
}

impl From<NodeInfo> for LndSyncStatus {
    fn from(info: NodeInfo) -> Self {
        Self {
            block_height: info.block_height(),
            network_is_regtest: info.network() == "regtest",
            synced_to_chain: info.synced_to_chain(),
            synced_to_graph: info.synced_to_graph(),
        }
    }
}

trait LndNodeConnector: Clone + Send + Sync + 'static {
    type Client: Send + Sync + 'static;

    fn initialize(
        &self,
        config: LndBootstrapConfig,
        password: &[u8],
    ) -> impl Future<Output = Result<Self::Client, LndError>> + Send;

    fn get_info(
        &self,
        client: &Self::Client,
    ) -> impl Future<Output = Result<LndSyncStatus, LndError>> + Send;
}

#[derive(Clone, Copy)]
struct RealLndConnector;

impl LndNodeConnector for RealLndConnector {
    type Client = LndClient;

    async fn initialize(
        &self,
        config: LndBootstrapConfig,
        password: &[u8],
    ) -> Result<Self::Client, LndError> {
        let config = initialize_wallet(config, password).await?;
        LndClient::with_config(config)
    }

    async fn get_info(&self, client: &Self::Client) -> Result<LndSyncStatus, LndError> {
        client.get_info().await.map(Into::into)
    }
}

trait BitcoinTip: Clone + Send + Sync + 'static {
    fn block_height(&self) -> impl Future<Output = Result<u64, FixtureError>> + Send;
}

impl BitcoinTip for NigiriClient<Bitcoin> {
    async fn block_height(&self) -> Result<u64, FixtureError> {
        self.rpc::<u64, _>("getblockcount", ())
            .await
            .map_err(FixtureError::Client)
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_lnd_nodes_under<E, C, B>(
    engine: E,
    network_name: String,
    bitcoind_name: String,
    alice_image: ContainerImage,
    bob_image: ContainerImage,
    deadline: &Deadline,
    connector: C,
    bitcoin_tip: B,
) -> Result<(StartedLndNodes<C::Client>, RuntimeHandle), FixtureError>
where
    E: ContainerEngine,
    C: LndNodeConnector,
    B: BitcoinTip,
{
    let names = LndNames::scoped();
    let endpoint_host = engine.endpoint_host().to_owned();
    let deadline = deadline.clone();

    supervise(engine, move |mut startup| async move {
        let alice_spec = lnd_spec(
            alice_image,
            network_name.clone(),
            names.alice.clone(),
            &bitcoind_name,
            &endpoint_host,
        )?;
        let bob_spec = lnd_spec(
            bob_image,
            network_name,
            names.bob.clone(),
            &bitcoind_name,
            &endpoint_host,
        )?;

        let started = deadline
            .run(
                "lnd-alice",
                "starting Alice and Bob LND containers",
                startup.start_container_pair(alice_spec, bob_spec),
            )
            .await;
        let (alice_container, bob_container) = match started {
            Ok(started) => started,
            Err(error) => {
                return Err(attach_lnd_logs(&mut startup, &names.alice, &names.bob, error).await);
            }
        };
        let (alice_container, bob_container) = match (alice_container, bob_container) {
            (Ok(alice), Ok(bob)) => (alice, bob),
            (alice, bob) => {
                let alice_log = alice
                    .as_ref()
                    .map_or_else(|_| names.alice.clone(), |container| container.id.clone());
                let bob_log = bob
                    .as_ref()
                    .map_or_else(|_| names.bob.clone(), |container| container.id.clone());
                let error = match (alice, bob) {
                    (Err(error), _) => runtime_error("lnd-alice", error),
                    (_, Err(error)) => runtime_error("lnd-bob", error),
                    _ => unreachable!("the successful pair was handled above"),
                };
                return Err(attach_lnd_logs(&mut startup, &alice_log, &bob_log, error).await);
            }
        };

        let alice_certificate = match wait_for_tls_certificate(
            &mut startup,
            "lnd-alice",
            &alice_container.id,
            &deadline,
        )
        .await
        {
            Ok(certificate) => certificate,
            Err(error) => {
                return Err(attach_lnd_logs(
                    &mut startup,
                    &alice_container.id,
                    &bob_container.id,
                    error,
                )
                .await);
            }
        };
        let bob_certificate =
            match wait_for_tls_certificate(&mut startup, "lnd-bob", &bob_container.id, &deadline)
                .await
            {
                Ok(certificate) => certificate,
                Err(error) => {
                    return Err(attach_lnd_logs(
                        &mut startup,
                        &alice_container.id,
                        &bob_container.id,
                        error,
                    )
                    .await);
                }
            };

        let alice_endpoint = match mapped_lnd_endpoint(&alice_container) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                return Err(attach_lnd_logs(
                    &mut startup,
                    &alice_container.id,
                    &bob_container.id,
                    bootstrap_configuration_error(error),
                )
                .await);
            }
        };
        let bob_endpoint = match mapped_lnd_endpoint(&bob_container) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                return Err(attach_lnd_logs(
                    &mut startup,
                    &alice_container.id,
                    &bob_container.id,
                    bootstrap_configuration_error(error),
                )
                .await);
            }
        };

        let alice_config = LndBootstrapConfig {
            endpoint: alice_endpoint,
            tls_certificate: alice_certificate,
            timeout: deadline.budget(),
        };
        let bob_config = LndBootstrapConfig {
            endpoint: bob_endpoint,
            tls_certificate: bob_certificate,
            timeout: deadline.budget(),
        };
        let initialized = startup
            .run_until_cancelled(initialize_lnd_clients(
                &connector,
                alice_config,
                bob_config,
                &deadline,
            ))
            .await;
        let (alice, bob) = match initialized {
            Ok(Ok(clients)) => clients,
            Ok(Err(error)) => {
                return Err(attach_lnd_logs(
                    &mut startup,
                    &alice_container.id,
                    &bob_container.id,
                    error,
                )
                .await);
            }
            Err(error) => {
                let error = runtime_error("LND pair", error);
                return Err(attach_lnd_logs(
                    &mut startup,
                    &alice_container.id,
                    &bob_container.id,
                    error,
                )
                .await);
            }
        };

        let synchronized = startup
            .run_until_cancelled(wait_for_lnd_sync(
                &connector,
                &alice,
                &bob,
                &bitcoin_tip,
                &deadline,
            ))
            .await;
        match synchronized {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(attach_lnd_logs(
                    &mut startup,
                    &alice_container.id,
                    &bob_container.id,
                    error,
                )
                .await);
            }
            Err(error) => {
                let error = runtime_error("LND pair", error);
                return Err(attach_lnd_logs(
                    &mut startup,
                    &alice_container.id,
                    &bob_container.id,
                    error,
                )
                .await);
            }
        }

        Ok(StartedLndNodes {
            alice,
            bob,
            names,
            container_ids: [alice_container.id, bob_container.id],
        })
    })
    .await
}

fn mapped_lnd_endpoint(container: &crate::runtime::RunningContainer) -> Result<Url, FixtureError> {
    let port = container
        .ports
        .get(&LND_GRPC_PORT)
        .copied()
        .ok_or_else(|| invalid("container runtime omitted the mapped LND gRPC port"))?;
    mapped_https_url(&container.host, port)
}

fn mapped_https_url(host: &str, port: u16) -> Result<Url, FixtureError> {
    let mut url = Url::parse("https://localhost/").expect("the static mapped URL is valid");
    url.set_host(Some(host))
        .or_else(|error| match host.parse::<std::net::Ipv6Addr>() {
            Ok(address) => url.set_host(Some(&format!("[{address}]"))),
            Err(_) => Err(error),
        })
        .map_err(|_| invalid("container runtime returned an invalid mapped host"))?;
    url.set_port(Some(port))
        .map_err(|()| invalid("container runtime returned an invalid mapped port"))?;
    Ok(url)
}

async fn wait_for_tls_certificate<E: ContainerEngine>(
    startup: &mut Startup<E>,
    service: &'static str,
    container_id: &str,
    deadline: &Deadline,
) -> Result<Vec<u8>, FixtureError> {
    let mut observation = "waiting for the bounded LND TLS certificate".to_owned();
    loop {
        match deadline
            .run(
                service,
                &observation,
                startup.read_container_file(
                    container_id,
                    TLS_CERT_PATH,
                    nigiri_rs_lnd::MAX_TLS_CERTIFICATE_BYTES,
                ),
            )
            .await
        {
            Ok(Ok(certificate)) if !certificate.is_empty() => return Ok(certificate),
            Ok(Ok(_)) => observation = "LND TLS certificate is still empty".to_owned(),
            Ok(Err(error)) if error.is_cancelled() => {
                return Err(runtime_error(service, error));
            }
            Ok(Err(error)) => {
                observation = redacted_tail(&format!("LND TLS certificate is not ready: {error}"));
            }
            Err(error) => return Err(error),
        }
        let slept = deadline
            .run(
                service,
                &observation,
                startup.run_until_cancelled(tokio::time::sleep(RETRY_DELAY)),
            )
            .await?;
        slept.map_err(|error| runtime_error(service, error))?;
    }
}

async fn wait_for_lnd_sync<C: LndNodeConnector, B: BitcoinTip>(
    connector: &C,
    alice: &C::Client,
    bob: &C::Client,
    bitcoin: &B,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut service = "lnd-alice";
    let mut observation = "waiting for both LND nodes to synchronize to regtest".to_owned();

    loop {
        let (alice_info, bob_info, bitcoin_height) = tokio::join!(
            deadline.run(service, &observation, connector.get_info(alice)),
            deadline.run("lnd-bob", &observation, connector.get_info(bob)),
            deadline.run("bitcoind", &observation, bitcoin.block_height()),
        );

        match (alice_info, bob_info, bitcoin_height) {
            (Ok(Ok(alice)), Ok(Ok(bob)), Ok(Ok(bitcoin_height)))
                if synchronized(alice, bitcoin_height) && synchronized(bob, bitcoin_height) =>
            {
                return Ok(());
            }
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => return Err(error),
            (Ok(Err(error)), _, _) => {
                service = "lnd-alice";
                observation = redacted_tail(&format!("Alice GetInfo is not ready: {error}"));
            }
            (_, Ok(Err(error)), _) => {
                service = "lnd-bob";
                observation = redacted_tail(&format!("Bob GetInfo is not ready: {error}"));
            }
            (_, _, Ok(Err(error))) => {
                service = "bitcoind";
                observation = redacted_tail(&format!("bitcoind tip is not ready: {error}"));
            }
            (Ok(Ok(alice)), Ok(Ok(bob)), Ok(Ok(bitcoin_height))) => {
                service = if !synchronized(alice, bitcoin_height) {
                    "lnd-alice"
                } else {
                    "lnd-bob"
                };
                observation = format!(
                    "Alice height={} regtest={} chain={} graph={}; Bob height={} regtest={} chain={} graph={}; bitcoind height={bitcoin_height}",
                    alice.block_height,
                    alice.network_is_regtest,
                    alice.synced_to_chain,
                    alice.synced_to_graph,
                    bob.block_height,
                    bob.network_is_regtest,
                    bob.synced_to_chain,
                    bob.synced_to_graph,
                );
            }
        }

        deadline
            .run(service, &observation, tokio::time::sleep(RETRY_DELAY))
            .await?;
    }
}

fn synchronized(status: LndSyncStatus, bitcoin_height: u64) -> bool {
    status.network_is_regtest
        && status.synced_to_chain
        && status.synced_to_graph
        && u64::from(status.block_height) == bitcoin_height
}

/// Keeps both random passwords in the narrowest stack frame that needs them. Returning from this
/// helper discards them before synchronization polling begins; no long-lived fixture field owns a
/// password or seed.
async fn initialize_lnd_clients<C: LndNodeConnector>(
    connector: &C,
    alice_config: LndBootstrapConfig,
    bob_config: LndBootstrapConfig,
    deadline: &Deadline,
) -> Result<(C::Client, C::Client), FixtureError> {
    let mut alice_password = [0_u8; 32];
    let mut bob_password = [0_u8; 32];
    fill_password(&mut alice_password)?;
    fill_password(&mut bob_password)?;

    let (alice, bob) = tokio::join!(
        deadline.run(
            "lnd-alice",
            "initializing Alice wallet",
            connector.initialize(alice_config, &alice_password),
        ),
        deadline.run(
            "lnd-bob",
            "initializing Bob wallet",
            connector.initialize(bob_config, &bob_password),
        )
    );
    let alice =
        alice?.map_err(|source| lightning_bootstrap_error("initialize Alice wallet", source))?;
    let bob = bob?.map_err(|source| lightning_bootstrap_error("initialize Bob wallet", source))?;
    Ok((alice, bob))
}

async fn attach_lnd_logs<E: ContainerEngine>(
    startup: &mut Startup<E>,
    alice_id_or_name: &str,
    bob_id_or_name: &str,
    error: FixtureError,
) -> FixtureError {
    let with_bob = attach_container_log(startup, "lnd-bob", bob_id_or_name, error).await;
    attach_container_log(startup, "lnd-alice", alice_id_or_name, with_bob).await
}

fn lightning_bootstrap_error(operation: &'static str, source: LndError) -> FixtureError {
    FixtureError::Bootstrap {
        chain: "Lightning",
        operation,
        diagnostics: redacted_tail(&source.to_string()),
        source: Box::new(FixtureError::Lightning(source)),
    }
}

fn bootstrap_configuration_error(source: FixtureError) -> FixtureError {
    FixtureError::Bootstrap {
        chain: "Lightning",
        operation: "configure mapped gRPC endpoint",
        diagnostics: redacted_tail(&source.to_string()),
        source: Box::new(source),
    }
}

fn fill_password(password: &mut [u8; 32]) -> Result<(), FixtureError> {
    getrandom::fill(password).map_err(|source| FixtureError::Bootstrap {
        chain: "Lightning",
        operation: "generate wallet password",
        diagnostics: "operating system randomness was unavailable".to_owned(),
        source: redacted_source(std::io::Error::other(format!(
            "operating system randomness failed: {source}"
        ))),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use nigiri_rs_lnd::{LndBootstrapConfig, LndError, Sats};
    use tokio::sync::{Barrier, Notify};

    use super::{
        BitcoinTip, LndHandles, LndNodeConnector, LndPair, LndSyncStatus, start_lnd_nodes_under,
    };
    use crate::{
        ContainerImage, FixtureError,
        deadline::Deadline,
        lnd::TLS_CERT_PATH,
        runtime::{ContainerEngine, ContainerSpec, EngineResult},
    };

    #[test]
    fn builder_defaults_pin_one_deadline_four_images_and_balanced_liquidity() {
        let builder = LndPair::builder();

        assert_eq!(builder.startup_timeout, Duration::from_secs(180));
        assert_eq!(builder.bitcoind_image, ContainerImage::bitcoind_default());
        assert_eq!(
            builder.bitcoin_electrs_image,
            ContainerImage::electrs_default()
        );
        assert_eq!(builder.alice_image, ContainerImage::lnd_default());
        assert_eq!(builder.bob_image, ContainerImage::lnd_default());
        assert_eq!(builder.channel_capacity, Sats::new(2_000_000));
        assert_eq!(builder.push_amount, Sats::new(1_000_000));
    }

    #[test]
    fn builder_methods_retain_every_override_for_later_channel_bootstrap() {
        let image = ContainerImage::new("registry.invalid/image", "v1");
        let builder = LndPair::builder()
            .startup_timeout(Duration::from_secs(240))
            .bitcoind_image(image.clone())
            .bitcoin_electrs_image(image.clone())
            .alice_image(image.clone())
            .bob_image(image.clone())
            .channel_capacity(Sats::new(3_000_000))
            .push_amount(Sats::new(1_250_000));

        assert_eq!(builder.startup_timeout, Duration::from_secs(240));
        assert_eq!(builder.bitcoind_image, image);
        assert_eq!(builder.bitcoin_electrs_image, image);
        assert_eq!(builder.alice_image, image);
        assert_eq!(builder.bob_image, image);
        assert_eq!(builder.channel_capacity, Sats::new(3_000_000));
        assert_eq!(builder.push_amount, Sats::new(1_250_000));
    }

    // Catches a regression that discovers an invalid later image or unusable channel allocation
    // only after the backing Bitcoin fixture has already connected to Docker.
    #[tokio::test]
    async fn all_builder_inputs_are_validated_before_connecting_to_docker() {
        let invalid_image = ContainerImage::new("", "v1");
        let rejected = [
            LndPair::builder().startup_timeout(Duration::ZERO),
            LndPair::builder().bitcoind_image(invalid_image.clone()),
            LndPair::builder().bitcoin_electrs_image(invalid_image.clone()),
            LndPair::builder().alice_image(invalid_image.clone()),
            LndPair::builder().bob_image(invalid_image),
            LndPair::builder().push_amount(Sats::new(0)),
            LndPair::builder()
                .channel_capacity(Sats::new(2_000_000))
                .push_amount(Sats::new(2_000_000)),
            LndPair::builder()
                .channel_capacity(Sats::new(2_000_000))
                .push_amount(Sats::new(1_900_001)),
            LndPair::builder()
                .channel_capacity(Sats::new(2_000_000))
                .push_amount(Sats::new(99_999)),
        ];

        for builder in rejected {
            let error = builder
                .start()
                .await
                .expect_err("invalid LND pair input must fail before Docker is contacted");
            assert!(matches!(error, FixtureError::InvalidConfiguration { .. }));
        }
    }

    #[derive(Clone)]
    struct FakeEngine {
        starts_together: Arc<Barrier>,
        block_file_read: bool,
        read_entered: Arc<Notify>,
        removal_delay: Duration,
        specs: Arc<Mutex<Vec<ContainerSpec>>>,
        removed: Arc<Mutex<Vec<String>>>,
    }

    impl FakeEngine {
        fn new() -> Self {
            Self {
                starts_together: Arc::new(Barrier::new(2)),
                block_file_read: false,
                read_entered: Arc::new(Notify::new()),
                removal_delay: Duration::ZERO,
                specs: Arc::new(Mutex::new(Vec::new())),
                removed: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl ContainerEngine for FakeEngine {
        fn endpoint_host(&self) -> &str {
            "127.0.0.1"
        }

        async fn create_network(
            &self,
            _name: &str,
            _labels: HashMap<String, String>,
        ) -> EngineResult<String> {
            panic!("LND nodes must reuse the backing fixture network")
        }

        async fn ensure_image(&self, _spec: &ContainerSpec) -> EngineResult<()> {
            self.starts_together.wait().await;
            Ok(())
        }

        async fn create_container(
            &self,
            spec: &ContainerSpec,
            _labels: HashMap<String, String>,
        ) -> EngineResult<String> {
            self.specs
                .lock()
                .expect("fake specs are never poisoned")
                .push(spec.clone());
            Ok(format!("{}-id", spec.name))
        }

        async fn start_container(&self, _id: &str) -> EngineResult<()> {
            Ok(())
        }

        async fn mapped_port(&self, id: &str, container_port: u16) -> EngineResult<u16> {
            assert_eq!(container_port, 10_009);
            Ok(if id.contains("alice") { 31_009 } else { 32_009 })
        }

        async fn logs(&self, id: &str) -> EngineResult<String> {
            Ok(if id.contains("alice") {
                "wallet_password=raw-password macaroon_hex=deadbeef".to_owned()
            } else {
                "wallet_password_hex=70617373 macaroon=raw-macaroon".to_owned()
            })
        }

        async fn read_container_file(
            &self,
            id: &str,
            path: &str,
            max_bytes: usize,
        ) -> EngineResult<Vec<u8>> {
            assert_eq!(path, TLS_CERT_PATH);
            assert_eq!(max_bytes, 1_048_576);
            self.read_entered.notify_one();
            if self.block_file_read {
                return std::future::pending().await;
            }
            Ok(if id.contains("alice") {
                b"alice certificate".to_vec()
            } else {
                b"bob certificate".to_vec()
            })
        }

        async fn remove_container(&self, id_or_name: &str) -> EngineResult<()> {
            tokio::time::sleep(self.removal_delay).await;
            self.removed
                .lock()
                .expect("fake removals are never poisoned")
                .push(id_or_name.to_owned());
            Ok(())
        }

        async fn remove_network(&self, _id_or_name: &str) -> EngineResult<()> {
            panic!("the LND runtime must not own the backing fixture network")
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeClient {
        endpoint: String,
    }

    struct InitializationRecord {
        endpoint: String,
        certificate: Vec<u8>,
        password: Vec<u8>,
    }

    #[derive(Clone)]
    struct FakeConnector {
        initialized: Arc<Mutex<Vec<InitializationRecord>>>,
        fail_initialization: bool,
    }

    impl FakeConnector {
        fn succeeding() -> Self {
            Self {
                initialized: Arc::new(Mutex::new(Vec::new())),
                fail_initialization: false,
            }
        }
    }

    impl LndNodeConnector for FakeConnector {
        type Client = FakeClient;

        async fn initialize(
            &self,
            config: LndBootstrapConfig,
            password: &[u8],
        ) -> Result<Self::Client, LndError> {
            let endpoint = config.endpoint.to_string();
            self.initialized
                .lock()
                .expect("fake initialization records are never poisoned")
                .push(InitializationRecord {
                    endpoint: endpoint.clone(),
                    certificate: config.tls_certificate,
                    password: password.to_vec(),
                });
            if self.fail_initialization {
                Err(LndError::Status {
                    operation: "initialize wallet".into(),
                    detail: "fake rejected initialization".into(),
                })
            } else {
                Ok(FakeClient { endpoint })
            }
        }

        async fn get_info(&self, _client: &Self::Client) -> Result<LndSyncStatus, LndError> {
            Ok(LndSyncStatus {
                block_height: 101,
                network_is_regtest: true,
                synced_to_chain: true,
                synced_to_graph: true,
            })
        }
    }

    #[derive(Clone)]
    struct FakeBitcoinTip;

    impl BitcoinTip for FakeBitcoinTip {
        async fn block_height(&self) -> Result<u64, FixtureError> {
            Ok(101)
        }
    }

    // Catches sequential startup, a fresh network, filesystem macaroon fallback, password reuse,
    // an unbounded certificate read, or teardown that outlives the backing fixture dependency.
    #[tokio::test]
    async fn fake_runtime_starts_initializes_and_cleans_both_nodes_under_one_supervisor() {
        let engine = FakeEngine::new();
        let connector = FakeConnector::succeeding();
        let deadline = Deadline::new(Duration::from_secs(5)).unwrap();

        let (started, runtime) = start_lnd_nodes_under(
            engine.clone(),
            "shared-network".to_owned(),
            "private-bitcoind".to_owned(),
            ContainerImage::lnd_default(),
            ContainerImage::lnd_default(),
            &deadline,
            connector.clone(),
            FakeBitcoinTip,
        )
        .await
        .expect("the fake nodes expose certificates and synchronized clients");

        assert_ne!(started.alice, started.bob);
        assert_eq!(started.container_ids.len(), 2);
        let mut specs = engine
            .specs
            .lock()
            .expect("fake specs are never poisoned")
            .clone();
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(specs.len(), 2);
        assert!(specs[0].name.starts_with("nigiri-rs-lnd-alice-"));
        assert!(specs[1].name.starts_with("nigiri-rs-lnd-bob-"));
        assert!(specs.iter().all(|spec| spec.network == "shared-network"));
        assert!(specs.iter().all(|spec| {
            spec.command
                .contains(&"--bitcoind.rpchost=private-bitcoind:18443".to_owned())
        }));

        {
            let mut initialized = connector
                .initialized
                .lock()
                .expect("fake initialization records are never poisoned");
            initialized.sort_by(|left, right| left.endpoint.cmp(&right.endpoint));
            assert_eq!(initialized.len(), 2);
            assert_eq!(initialized[0].certificate, b"alice certificate");
            assert_eq!(initialized[1].certificate, b"bob certificate");
            assert_eq!(initialized[0].password.len(), 32);
            assert_eq!(initialized[1].password.len(), 32);
            assert_ne!(initialized[0].password, initialized[1].password);
        }

        runtime.shutdown().await.unwrap();
        let removed = engine
            .removed
            .lock()
            .expect("fake removals are never poisoned")
            .clone();
        assert!(removed[0].contains("bob"), "{removed:?}");
        assert!(removed[1].contains("alice"), "{removed:?}");
    }

    // Catches a failed wallet bootstrap leaking either container or losing its LND source behind
    // diagnostics; the marker spellings exercise both raw and hex-shaped secret redaction.
    #[tokio::test]
    async fn wallet_failure_cleans_both_nodes_and_retains_redacted_dependency_logs() {
        let engine = FakeEngine::new();
        let connector = FakeConnector {
            initialized: Arc::new(Mutex::new(Vec::new())),
            fail_initialization: true,
        };
        let deadline = Deadline::new(Duration::from_secs(5)).unwrap();

        let result = start_lnd_nodes_under(
            engine.clone(),
            "shared-network".to_owned(),
            "private-bitcoind".to_owned(),
            ContainerImage::lnd_default(),
            ContainerImage::lnd_default(),
            &deadline,
            connector,
            FakeBitcoinTip,
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a rejected wallet initialization must fail startup"),
        };

        let FixtureError::Bootstrap {
            chain,
            diagnostics,
            source,
            ..
        } = &error
        else {
            panic!("wallet startup errors need a diagnostic-carrying bootstrap wrapper: {error}");
        };
        assert_eq!(*chain, "Lightning");
        assert!(matches!(
            source.downcast_ref::<FixtureError>(),
            Some(FixtureError::Lightning(_))
        ));
        for secret in ["raw-password", "deadbeef", "70617373", "raw-macaroon"] {
            assert!(!diagnostics.contains(secret), "{diagnostics}");
            assert!(!error.to_string().contains(secret), "{error}");
        }
        let bob_log = diagnostics.find("lnd-bob log").unwrap();
        let alice_log = diagnostics.find("lnd-alice log").unwrap();
        assert!(bob_log < alice_log, "{diagnostics}");
        let removed = engine.removed.lock().unwrap().clone();
        assert_eq!(removed.len(), 2, "{removed:?}");
        assert!(removed[0].contains("bob"), "{removed:?}");
        assert!(removed[1].contains("alice"), "{removed:?}");
    }

    // Catches a caller cancellation leaving either already-started LND container behind while a
    // certificate read is pending.
    #[tokio::test]
    async fn cancellation_during_certificate_poll_cleans_both_lnd_containers() {
        let mut engine = FakeEngine::new();
        engine.block_file_read = true;
        engine.removal_delay = Duration::from_millis(50);
        let task_engine = engine.clone();
        let read_entered = Arc::clone(&engine.read_entered);

        let task = tokio::spawn(async move {
            let deadline = Deadline::new(Duration::from_secs(30)).unwrap();
            start_lnd_nodes_under(
                task_engine,
                "shared-network".to_owned(),
                "private-bitcoind".to_owned(),
                ContainerImage::lnd_default(),
                ContainerImage::lnd_default(),
                &deadline,
                FakeConnector::succeeding(),
                FakeBitcoinTip,
            )
            .await
        });

        read_entered.notified().await;
        task.abort();
        let cancellation = match task.await {
            Err(error) => error,
            Ok(_) => panic!("aborting the startup task must cancel it"),
        };
        assert!(cancellation.is_cancelled());
        let removed = engine.removed.lock().unwrap().clone();
        assert_eq!(
            removed.len(),
            2,
            "caller cancellation returned before cleanup: {removed:?}"
        );
        assert!(removed[0].contains("bob"), "{removed:?}");
        assert!(removed[1].contains("alice"), "{removed:?}");
    }

    struct DropRecorder {
        name: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Drop for DropRecorder {
        fn drop(&mut self) {
            self.order.lock().unwrap().push(self.name);
        }
    }

    // Catches a field-order regression that tears down bitcoind before the LND supervisor that
    // still depends on it.
    #[test]
    fn lnd_runtime_is_owned_before_the_backing_bitcoin_fixture() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let handles = LndHandles {
            lnd: DropRecorder {
                name: "lnd",
                order: Arc::clone(&order),
            },
            bitcoin: DropRecorder {
                name: "bitcoin",
                order: Arc::clone(&order),
            },
        };

        drop(handles);

        assert_eq!(*order.lock().unwrap(), ["lnd", "bitcoin"]);
    }
}
