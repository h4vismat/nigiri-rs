#![cfg_attr(not(test), allow(dead_code))]

use std::{future::Future, time::Duration};

use nigiri_rs::{Bitcoin, NigiriClient, NigiriConfig, NigiriError};
use testcontainers::{
    ContainerAsync, ContainerRequest, GenericImage, ImageExt, core::IntoContainerPort,
    runners::AsyncRunner,
};
use url::Url;
use uuid::Uuid;

use crate::{
    ContainerImage, FixtureError, RPC_PASSWORD, RPC_USER,
    deadline::Deadline,
    diagnostics::{MAX_SOURCE_BYTES, redacted_head, redacted_source, redacted_tail},
    endpoint::mapped_http_url,
    owned_start::{
        attach_container_log, classify_start_error, container_log_tail,
        port_discovery_with_log_result, run_owned_start,
    },
};

const SERVICE: &str = "bitcoind";
const RPC_PORT: u16 = 18_443;
const READINESS_RETRY_DELAY: Duration = Duration::from_millis(100);

#[cfg_attr(test, allow(dead_code))]
pub(crate) struct StartedBitcoind {
    pub(crate) container: ContainerAsync<GenericImage>,
    pub(crate) client: NigiriClient<Bitcoin>,
    pub(crate) client_config: NigiriConfig,
    pub(crate) network_name: String,
    pub(crate) container_name: String,
}

struct InitialMiningGate {
    permit: tokio::sync::Mutex<()>,
}

impl InitialMiningGate {
    const fn new() -> Self {
        Self {
            permit: tokio::sync::Mutex::const_new(()),
        }
    }

    async fn run<T, F>(&self, deadline: &Deadline, future: F) -> Result<T, FixtureError>
    where
        F: Future<Output = T>,
    {
        // Acquiring and holding the permit are reported separately so a timeout distinguishes
        // waiting for another fixture from mining that is genuinely too slow. Both are bounded by
        // the same shared budget, and dropping the guard on cancellation releases the permit.
        let _permit = deadline
            .run(
                "bitcoind",
                "waiting for the initial mining permit",
                self.permit.lock(),
            )
            .await?;

        deadline
            .run(SERVICE, "mining the initial 101 blocks", future)
            .await
    }
}

#[cfg_attr(test, allow(dead_code))]
static INITIAL_MINING_GATE: InitialMiningGate = InitialMiningGate::new();

pub(crate) fn request(
    image: &ContainerImage,
    network_name: &str,
    container_name: &str,
) -> Result<ContainerRequest<GenericImage>, FixtureError> {
    image.validate()?;

    Ok(
        GenericImage::new(image.name().to_owned(), image.testcontainers_tag())
            .with_exposed_port(RPC_PORT.tcp())
            .with_network(network_name)
            .with_container_name(container_name)
            .with_cmd([
                "-regtest=1".to_owned(),
                "-server=1".to_owned(),
                "-txindex=1".to_owned(),
                "-rpcbind=0.0.0.0:18443".to_owned(),
                "-rpcallowip=0.0.0.0/0".to_owned(),
                format!("-rpcuser={RPC_USER}"),
                format!("-rpcpassword={RPC_PASSWORD}"),
                "-fallbackfee=0.00001".to_owned(),
                "-printtoconsole=1".to_owned(),
            ]),
    )
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) async fn start_bitcoind(
    image: &ContainerImage,
    network_name: String,
    container_name: String,
    deadline: &Deadline,
) -> Result<StartedBitcoind, FixtureError> {
    let container_request = request(image, &network_name, &container_name)?;
    let container = run_owned_start(
        SERVICE,
        image,
        deadline,
        &container_name,
        "starting Bitcoind container",
        container_request.start(),
    )
    .await?;

    let host = deadline
        .run(SERVICE, "resolving Bitcoind host", container.get_host())
        .await?
        .map_err(|error| classify_start_error(SERVICE, image, error))?
        .to_string();
    let rpc_port = match deadline
        .run(
            "bitcoind",
            "resolving Bitcoind RPC mapped port",
            container.get_host_port_ipv4(RPC_PORT.tcp()),
        )
        .await?
    {
        Ok(port) => port,
        Err(error) => {
            return Err(port_discovery_with_log_result(
                SERVICE,
                RPC_PORT,
                error,
                container_log_tail(SERVICE, &container, deadline).await,
            ));
        }
    };

    let root_url = mapped_http_url(&host, rpc_port)?;
    let root_config = fixture_rpc_config(
        root_url.clone(),
        deadline.remaining_or_expired(SERVICE, "configuring the root Bitcoind RPC client")?,
    );
    let root = fixture_client(root_config)?;
    if let Err(not_ready) = wait_for_root_rpc(&root, deadline).await {
        // A node that never answered is the likeliest startup failure, and its own log is the only
        // thing that explains why, so the timeout carries a bounded tail of it.
        return Err(attach_container_log(SERVICE, not_ready, &container).await);
    }

    let wallet_name = format!("nigiri-rs-{}", Uuid::new_v4().simple());
    let wallet_creation = deadline
        .run(
            "bitcoind",
            "creating Bitcoin Core wallet",
            root.rpc("createwallet", (&wallet_name,)),
        )
        .await?;
    let _: serde_json::Value =
        wallet_creation.map_err(|source| bootstrap_error("createwallet", source))?;

    let wallet_url = wallet_rpc_url(&root_url, &wallet_name)?;
    // This client outlives startup, so it gets the whole startup budget as its request timeout rather
    // than whatever is left of it: every startup RPC below is bounded by the shared deadline anyway,
    // and a caller must not inherit a timeout that depends on how slow startup happened to be.
    let client_config = fixture_rpc_config(wallet_url, deadline.budget());
    let client = fixture_client(client_config.clone())?;
    let mining_address = deadline
        .run(
            "bitcoind",
            "creating initial mining address",
            client.new_address(),
        )
        .await?
        .map_err(|source| bootstrap_error("getnewaddress", source))?
        .to_string();
    INITIAL_MINING_GATE
        .run(
            deadline,
            client.generate_to_address(101, mining_address.as_str()),
        )
        .await?
        .map_err(|source| bootstrap_error("generatetoaddress", source))?;

    Ok(StartedBitcoind {
        container,
        client,
        client_config,
        network_name,
        container_name,
    })
}

/// Builds a fixture RPC client, keeping the rejected configuration out of the error chain.
///
/// `FixtureError::Client` forwards a `NigiriError` and its whole raw cause chain, and a rejected
/// fixture configuration carries the fixture credentials, so the rejection is reported as bounded,
/// redacted configuration detail instead.
#[cfg_attr(test, allow(dead_code))]
fn fixture_client(config: NigiriConfig) -> Result<NigiriClient<Bitcoin>, FixtureError> {
    NigiriClient::<Bitcoin>::with_config(config).map_err(|source| {
        FixtureError::InvalidConfiguration {
            detail: redacted_head(
                &format!("fixture RPC client configuration was rejected: {source}"),
                MAX_SOURCE_BYTES,
            ),
        }
    })
}

/// The node RPC half of a fixture client's configuration.
///
/// `esplora_url` is a placeholder pointing at the same node: this task starts no indexer, so the
/// returned client is node-RPC-only until Electrs is wired in and supplies a real Esplora base URL.
#[cfg_attr(test, allow(dead_code))]
fn fixture_rpc_config(node_rpc_url: Url, timeout: Duration) -> NigiriConfig {
    NigiriConfig {
        esplora_url: node_rpc_url.clone(),
        node_rpc_url,
        node_rpc_user: RPC_USER.to_owned(),
        node_rpc_password: RPC_PASSWORD.to_owned(),
        timeout,
        ..Default::default()
    }
}

#[cfg_attr(test, allow(dead_code))]
async fn wait_for_root_rpc(
    root: &NigiriClient<Bitcoin>,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut last_observation = "waiting for root getblockchaininfo RPC".to_owned();

    loop {
        match deadline
            .run(
                "bitcoind",
                &last_observation,
                root.rpc::<serde_json::Value, _>("getblockchaininfo", ()),
            )
            .await
        {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(error)) => {
                last_observation = redacted_tail(&format!("root RPC: {error}"));
                deadline
                    .run(
                        "bitcoind",
                        &last_observation,
                        tokio::time::sleep(READINESS_RETRY_DELAY),
                    )
                    .await?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn wallet_rpc_url(root_url: &Url, wallet_name: &str) -> Result<Url, FixtureError> {
    let mut wallet_url = root_url.clone();
    wallet_url
        .path_segments_mut()
        .map_err(|_| FixtureError::InvalidConfiguration {
            detail: "node RPC URL cannot hold a wallet path".to_owned(),
        })?
        .extend(["wallet", wallet_name]);
    Ok(wallet_url)
}

fn bootstrap_error(operation: &'static str, source: NigiriError) -> FixtureError {
    FixtureError::Bootstrap {
        operation,
        diagnostics: redacted_tail(&source.to_string()),
        source: redacted_source(source),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        io,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use nigiri_rs::NigiriError;
    use testcontainers::{
        ContainerRequest, GenericImage, Image,
        core::{
            IntoContainerPort,
            error::{ClientError, TestcontainersError},
        },
    };
    use tokio::sync::{Barrier, Notify, mpsc, oneshot};
    use url::Url;

    use super::{
        InitialMiningGate, RPC_PORT, SERVICE, bootstrap_error, fixture_client, fixture_rpc_config,
        request, wallet_rpc_url,
    };
    use crate::{
        ContainerImage, FixtureError,
        deadline::Deadline,
        diagnostics::{MAX_DIAGNOSTIC_BYTES, MAX_SOURCE_BYTES},
        owned_start::{classify_start_error, port_discovery_error},
    };

    struct ActiveMining {
        active: Arc<AtomicUsize>,
    }

    impl ActiveMining {
        fn enter(active: Arc<AtomicUsize>, maximum: &AtomicUsize) -> Self {
            let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(now_active, Ordering::SeqCst);
            Self { active }
        }
    }

    impl Drop for ActiveMining {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn command(request: &ContainerRequest<GenericImage>) -> Vec<String> {
        request
            .cmd()
            .map(|argument| argument.into_owned())
            .collect()
    }

    // Catches a request regression that changes a Bitcoin Core's pinned image, topology,
    // exposed RPC port, or its bootstrap RPC arguments.
    #[test]
    fn request_preserves_the_exact_regtest_rpc_contract() {
        let request = request(
            &ContainerImage::bitcoind_default(),
            "nigiri-test-fixture",
            "nigiri-bitcoind-fixture",
        )
        .expect("the pinned Bitcoind image is valid");

        assert_eq!(request.image().name(), "ghcr.io/getumbrel/docker-bitcoind");
        assert_eq!(
            request.image().tag(),
            "v30.0@sha256:f5826a32aed9287cc5ffdec0996f5272634c4b346529cb8627224986ff555101"
        );
        assert_eq!(request.entrypoint(), None);
        assert_eq!(request.expose_ports(), &[18_443.tcp()]);
        assert_eq!(request.network().as_deref(), Some("nigiri-test-fixture"));
        assert_eq!(
            request.container_name().as_deref(),
            Some("nigiri-bitcoind-fixture")
        );
        assert_eq!(
            command(&request),
            [
                "-regtest=1",
                "-server=1",
                "-txindex=1",
                "-rpcbind=0.0.0.0:18443",
                "-rpcallowip=0.0.0.0/0",
                "-rpcuser=admin1",
                "-rpcpassword=123",
                "-fallbackfee=0.00001",
                "-printtoconsole=1",
            ]
        );
    }

    // Catches a regression that defers invalid image validation until Docker request startup.
    #[test]
    fn request_rejects_invalid_images_before_constructing_a_request() {
        let error = match request(
            &ContainerImage::new("", "v1"),
            "nigiri-test-fixture",
            "nigiri-bitcoind-fixture",
        ) {
            Err(error) => error,
            Ok(_) => panic!("an image without a name must be rejected"),
        };

        assert!(matches!(error, FixtureError::InvalidConfiguration { .. }));
    }

    // Catches a regression that lets the wallet endpoint acquire a trailing slash or loses the
    // exact wallet name path segment needed by Bitcoin Core.
    #[test]
    fn wallet_rpc_url_is_exactly_wallet_name_without_a_trailing_slash() {
        let root = Url::parse("http://127.0.0.1:18443/").expect("a static root URL is valid");

        let wallet = wallet_rpc_url(&root, "nigiri-rs-123")
            .expect("a hierarchical node RPC URL can contain a wallet path");

        assert_eq!(
            wallet.as_str(),
            "http://127.0.0.1:18443/wallet/nigiri-rs-123"
        );
    }

    // Catches a regression that reports a rejected fixture client configuration through
    // `FixtureError::Client`, whose transparent source chain would carry the raw configuration.
    #[test]
    fn a_rejected_client_configuration_is_reported_without_a_raw_source() {
        let url = Url::parse("http://127.0.0.1:18443/").expect("a static root URL is valid");

        let error = fixture_client(fixture_rpc_config(url, Duration::ZERO))
            .expect_err("a zero request timeout must be rejected");

        let FixtureError::InvalidConfiguration { detail } = error else {
            panic!("a rejected fixture configuration must not become a transparent client error");
        };
        assert!(detail.len() <= MAX_SOURCE_BYTES);
        assert!(!detail.contains("admin1"));
        assert!(
            detail.starts_with("fixture RPC client configuration was rejected:"),
            "{detail}"
        );
    }

    // Catches a regression that boxes a raw error as a fixture source, letting error-chain
    // formatters expose fixture credentials or an unbounded body that bounded diagnostics hide.
    #[test]
    fn fixture_error_sources_are_bounded_and_redacted_across_the_whole_chain() {
        let secret_body = format!(
            "{} -rpcuser=admin1 -rpcpassword=123 admin1:123",
            "node-error-".repeat(4_000),
        );
        let image = ContainerImage::bitcoind_default();
        let errors = [
            bootstrap_error(
                "createwallet",
                NigiriError::InvalidResponse {
                    operation: "bootstrap RPC".into(),
                    detail: secret_body.clone(),
                },
            ),
            classify_start_error(
                SERVICE,
                &image,
                TestcontainersError::other(io::Error::other(secret_body.clone())),
            ),
            classify_start_error(
                SERVICE,
                &image,
                TestcontainersError::Client(ClientError::InvalidDockerHost(secret_body.clone())),
            ),
            port_discovery_error(
                SERVICE,
                RPC_PORT,
                TestcontainersError::other(io::Error::other(secret_body.clone())),
                "mapped port unavailable",
            ),
        ];

        for error in errors {
            let mut cause = Error::source(&error);
            let mut depth = 0_usize;

            while let Some(source) = cause {
                depth += 1;
                for rendered in [source.to_string(), format!("{source:?}")] {
                    assert!(rendered.len() <= MAX_SOURCE_BYTES, "{rendered:.64}");
                    assert!(!rendered.contains("admin1:123"));
                    assert!(!rendered.contains("-rpcuser=admin1"));
                    assert!(!rendered.contains("-rpcpassword=123"));
                }
                cause = source.source();
            }

            assert_eq!(
                depth, 1,
                "a fixture source must not expose a raw cause chain"
            );
            assert!(!format!("{error:?}").contains("admin1:123"));
        }
    }

    // Catches a regression that exposes wallet bootstrap failures as generic client errors or
    // leaks a large credential-bearing RPC error into the fixture display.
    #[test]
    fn bootstrap_errors_keep_the_operation_source_and_redacted_bounded_diagnostics() {
        for operation in ["createwallet", "getnewaddress", "generatetoaddress"] {
            let source = NigiriError::InvalidResponse {
                operation: "bootstrap RPC".into(),
                detail: format!("{} admin1:123", "node-error-".repeat(2_000)),
            };
            let error = bootstrap_error(operation, source);

            assert!(error.to_string().starts_with(&format!(
                "Bitcoin wallet bootstrap failed during {operation}:"
            )));
            assert!(
                Error::source(&error).map(ToString::to_string).is_some_and(
                    |source| source.starts_with("invalid response during bootstrap RPC")
                )
            );
            let FixtureError::Bootstrap {
                operation: actual_operation,
                diagnostics,
                ..
            } = error
            else {
                panic!("a wallet RPC error must be classified as bootstrap failure");
            };
            assert_eq!(actual_operation, operation);
            assert!(diagnostics.len() <= MAX_DIAGNOSTIC_BYTES);
            assert!(!diagnostics.contains("admin1:123"));
        }
    }

    // Catches a regression that permits concurrent initial 101-block mining or leaks a permit
    // when its owning startup task is cancelled. The watchdog only detects a deadlock; ordering
    // is proven through channels and atomics.
    #[tokio::test]
    async fn initial_mining_gate_serializes_and_releases_after_cancellation() {
        let gate = Arc::new(InitialMiningGate::new());
        let deadline = Arc::new(
            crate::deadline::Deadline::new(Duration::from_secs(10))
                .expect("a positive shared deadline is valid"),
        );
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (first_entered_tx, first_entered_rx) = oneshot::channel();
        let (first_release_tx, first_release_rx) = oneshot::channel::<()>();

        let first = {
            let gate = Arc::clone(&gate);
            let deadline = Arc::clone(&deadline);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            tokio::spawn(async move {
                gate.run(&deadline, async move {
                    let _active = ActiveMining::enter(active, &maximum);
                    first_entered_tx
                        .send(())
                        .expect("the test must await first entry");
                    let _ = first_release_rx.await;
                })
                .await
            })
        };

        first_entered_rx
            .await
            .expect("the first mining operation must acquire the permit");

        let (second_entered_tx, mut second_entered_rx) = oneshot::channel();
        let second = {
            let gate = Arc::clone(&gate);
            let deadline = Arc::clone(&deadline);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            tokio::spawn(async move {
                gate.run(&deadline, async move {
                    let _active = ActiveMining::enter(active, &maximum);
                    second_entered_tx
                        .send(())
                        .expect("the test must await second entry");
                })
                .await
            })
        };

        tokio::task::yield_now().await;
        assert!(
            matches!(
                second_entered_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "work waiting for the mining permit must not enter concurrently"
        );

        first.abort();
        let _ = first.await;
        drop(first_release_tx);

        tokio::time::timeout(Duration::from_secs(2), &mut second_entered_rx)
            .await
            .expect("cancelling the first operation must release the mining permit")
            .expect("the second mining operation must enter after cancellation");
        second
            .await
            .expect("the second task must complete")
            .expect("the gate must preserve successful operation output");
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    // Catches a regression that broadens the initial-mining permit to serialize unrelated work
    // before or after the one Bitcoin Core mining RPC.
    #[tokio::test]
    async fn initial_mining_gate_only_serializes_the_mining_future() {
        let gate = Arc::new(InitialMiningGate::new());
        let deadline =
            Arc::new(Deadline::new(Duration::from_secs(10)).expect("a positive deadline is valid"));
        let pre_barrier = Arc::new(Barrier::new(3));
        let post_barrier = Arc::new(Barrier::new(3));
        let release_first_mining = Arc::new(Notify::new());
        let mining_order = Arc::new(AtomicUsize::new(0));
        let pre_active = Arc::new(AtomicUsize::new(0));
        let pre_maximum = Arc::new(AtomicUsize::new(0));
        let mining_active = Arc::new(AtomicUsize::new(0));
        let mining_maximum = Arc::new(AtomicUsize::new(0));
        let post_active = Arc::new(AtomicUsize::new(0));
        let post_maximum = Arc::new(AtomicUsize::new(0));
        let (mining_started_tx, mut mining_started_rx) = mpsc::unbounded_channel();

        let start_worker = |mining_started_tx: mpsc::UnboundedSender<usize>| {
            let gate = Arc::clone(&gate);
            let deadline = Arc::clone(&deadline);
            let pre_barrier = Arc::clone(&pre_barrier);
            let post_barrier = Arc::clone(&post_barrier);
            let release_first_mining = Arc::clone(&release_first_mining);
            let mining_order = Arc::clone(&mining_order);
            let pre_active = Arc::clone(&pre_active);
            let pre_maximum = Arc::clone(&pre_maximum);
            let mining_active = Arc::clone(&mining_active);
            let mining_maximum = Arc::clone(&mining_maximum);
            let post_active = Arc::clone(&post_active);
            let post_maximum = Arc::clone(&post_maximum);

            tokio::spawn(async move {
                let pre_work = ActiveMining::enter(pre_active, &pre_maximum);
                pre_barrier.wait().await;
                drop(pre_work);

                gate.run(&deadline, async move {
                    let _mining_work = ActiveMining::enter(mining_active, &mining_maximum);
                    let order = mining_order.fetch_add(1, Ordering::SeqCst);
                    mining_started_tx
                        .send(order)
                        .expect("the test must observe mining entry");
                    if order == 0 {
                        release_first_mining.notified().await;
                    }
                })
                .await
                .expect("the mining gate must preserve successful work");

                let _post_work = ActiveMining::enter(post_active, &post_maximum);
                post_barrier.wait().await;
            })
        };

        let first = start_worker(mining_started_tx.clone());
        let second = start_worker(mining_started_tx);

        tokio::time::timeout(Duration::from_secs(2), pre_barrier.wait())
            .await
            .expect("both workers must reach pre-gate work concurrently");
        assert_eq!(pre_maximum.load(Ordering::SeqCst), 2);

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), mining_started_rx.recv())
                .await
                .expect("one worker must enter the mining permit")
                .expect("the mining-entry channel must remain open"),
            0
        );
        assert_eq!(mining_maximum.load(Ordering::SeqCst), 1);
        release_first_mining.notify_one();

        tokio::time::timeout(Duration::from_secs(2), post_barrier.wait())
            .await
            .expect("both workers must reach post-gate work concurrently");
        first.await.expect("the first worker must complete");
        second.await.expect("the second worker must complete");

        assert_eq!(mining_maximum.load(Ordering::SeqCst), 1);
        assert_eq!(post_maximum.load(Ordering::SeqCst), 2);
    }
}
