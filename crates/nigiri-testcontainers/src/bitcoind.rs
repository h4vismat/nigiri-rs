#![cfg_attr(not(test), allow(dead_code))]

use std::{future::Future, io, pin::Pin, sync::Arc, time::Duration};

use nigiri_rs::{Bitcoin, NigiriClient, NigiriConfig, NigiriError};
use testcontainers::{
    ContainerAsync, ContainerRequest, GenericImage, ImageExt,
    bollard::{
        API_DEFAULT_VERSION, Docker, errors::Error as BollardError,
        query_parameters::RemoveContainerOptionsBuilder,
    },
    core::{
        IntoContainerPort,
        error::{ClientError, TestcontainersError, WaitContainerError},
    },
    runners::AsyncRunner,
};
use tokio::io::AsyncReadExt;
use url::Url;
use uuid::Uuid;

use crate::{
    ContainerImage, FixtureError, RPC_PASSWORD, RPC_USER,
    deadline::Deadline,
    diagnostics::{
        MAX_DIAGNOSTIC_BYTES, MAX_REDACTION_CONTEXT_BYTES, MAX_SOURCE_BYTES, join_diagnostics,
        redacted_head, redacted_source, redacted_tail,
    },
};

const RPC_PORT: u16 = 18_443;
const READINESS_RETRY_DELAY: Duration = Duration::from_millis(100);
const UTF8_TAIL_LOOKBEHIND_BYTES: usize = 3;
const ROLLING_LOG_BYTES: usize =
    MAX_DIAGNOSTIC_BYTES + UTF8_TAIL_LOOKBEHIND_BYTES + MAX_REDACTION_CONTEXT_BYTES;
const MAX_CONTAINER_NAME_BYTES: usize = 256;
const MAX_IMAGE_DESCRIPTOR_BYTES: usize = 512;
/// Bounds for error-path cleanup, which runs after startup has already failed. They are deliberately
/// outside the shared startup budget: abandonment happens once that budget is gone, and a removal
/// after a failed start may add up to `PARTIAL_CLEANUP_BOUND` to the failing call.
const ABANDONED_START_JOIN_BOUND: Duration = Duration::from_secs(5);
const PARTIAL_CLEANUP_BOUND: Duration = Duration::from_secs(5);
const DETACHED_DROP_JOIN_BOUND: Duration = Duration::from_secs(5);
const DETACHED_DROP_SHUTDOWN_BOUND: Duration = Duration::from_millis(100);
const DIAGNOSTIC_LOG_BOUND: Duration = Duration::from_secs(5);
const DOCKER_CLEANUP_TIMEOUT: u64 = 5;
const DEFAULT_DOCKER_HOST: &str = "unix:///var/run/docker.sock";
const TESTCONTAINERS_KEEP_COMMAND: &str = "keep";

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
            .run("bitcoind", "mining the initial 101 blocks", future)
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
        image,
        deadline,
        &container_name,
        "starting Bitcoind container",
        container_request.start(),
    )
    .await?;

    let host = deadline
        .run("bitcoind", "resolving Bitcoind host", container.get_host())
        .await?
        .map_err(|error| classify_start_error(image, error))?
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
                error,
                bitcoind_log_tail(&container, deadline).await,
            ));
        }
    };

    let root_url = mapped_http_url(&host, rpc_port)?;
    let root_config = fixture_rpc_config(
        root_url.clone(),
        deadline.remaining_or_expired("bitcoind", "configuring the root Bitcoind RPC client")?,
    );
    let root = fixture_client(root_config)?;
    if let Err(not_ready) = wait_for_root_rpc(&root, deadline).await {
        // A node that never answered is the likeliest startup failure, and its own log is the only
        // thing that explains why, so the timeout carries a bounded tail of it.
        return Err(attach_container_log(not_ready, &container).await);
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

#[cfg_attr(test, allow(dead_code))]
fn mapped_http_url(host: &str, port: u16) -> Result<Url, FixtureError> {
    let mut url = Url::parse("http://localhost/").expect("the static mapped URL is valid");
    url.set_host(Some(host))
        .map_err(|_| FixtureError::InvalidConfiguration {
            detail: "container runtime returned an invalid mapped host".to_owned(),
        })?;
    url.set_port(Some(port))
        .map_err(|()| FixtureError::InvalidConfiguration {
            detail: "container runtime returned an invalid mapped port".to_owned(),
        })?;
    Ok(url)
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

async fn run_owned_start<T, F>(
    image: &ContainerImage,
    deadline: &Deadline,
    container_name: &str,
    last_observation: &str,
    future: F,
) -> Result<T, FixtureError>
where
    T: Send + 'static,
    F: Future<Output = Result<T, TestcontainersError>> + Send + 'static,
{
    run_owned_start_with_cleanup(
        image,
        deadline,
        container_name,
        last_observation,
        future,
        docker_partial_cleanup(),
    )
    .await
}

/// The removal is injected so every startup outcome can be proven without a Docker daemon.
async fn run_owned_start_with_cleanup<T, F>(
    image: &ContainerImage,
    deadline: &Deadline,
    container_name: &str,
    last_observation: &str,
    future: F,
    cleanup: PartialCleanup,
) -> Result<T, FixtureError>
where
    T: Send + 'static,
    F: Future<Output = Result<T, TestcontainersError>> + Send + 'static,
{
    // The task is abandoned rather than detached if this future is itself dropped: dropping a
    // `JoinHandle` detaches the task, which would let Docker create a container after the caller has
    // given up. The guard stays armed across the error paths below, because they await too, and each
    // path stands it down only once nothing is outstanding.
    let mut guard =
        AbandonOnDrop::with_cleanup(tokio::spawn(future), container_name.to_owned(), cleanup);

    let joined = deadline
        .run("bitcoind", last_observation, guard.handle())
        .await;

    match joined {
        Ok(Ok(Ok(value))) => {
            guard.finish();
            Ok(value)
        }
        Ok(Ok(Err(error))) => {
            let orphan_risk = start_failure_can_orphan(&error);
            guard.joined(orphan_risk);
            let classified =
                attach_start_failure_cleanup(classify_start_error(image, error), &mut guard).await;
            Err(classified)
        }
        // A panicking startup task can orphan whatever it had already created.
        Ok(Err(error)) => {
            guard.joined(true);
            let classified =
                attach_start_failure_cleanup(container_start_join_error(image, error), &mut guard)
                    .await;
            Err(classified)
        }
        // `abandon_owned_start` owns the decision to stand the guard down: a join that never
        // resolved leaves the task pending, so its `Drop` must still retry.
        Err(expired) => Err(abandon_owned_start(&mut guard, expired).await),
    }
}

/// Removes the exact partial container, as a future the abandonment paths can hold on to.
type PartialCleanup =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = String> + Send>> + Send + Sync>;

fn docker_partial_cleanup() -> PartialCleanup {
    Arc::new(|container_name| {
        Box::pin(async move { remove_partial_container(&container_name).await })
    })
}

/// What still has to happen to a startup task if its owner disappears.
enum Abandonment<T: Send + 'static> {
    /// The task has not been joined, so it may still be creating a container.
    Pending(tokio::task::JoinHandle<Result<T, TestcontainersError>>),
    /// The task was joined and left a container behind that only explicit removal will reap.
    CleanupOnly,
    /// Nothing is outstanding.
    Finished,
}

/// A startup task that is abandoned rather than detached when its owner goes away.
///
/// A cancelled caller cannot await anything, so the abandonment continues on a dedicated thread that
/// outlives the caller's runtime: it aborts the task, waits for it, and then either lets natural
/// Testcontainers cleanup run on a late handle or removes the exact partial container. The guard is
/// only stood down once nothing is outstanding, so cancellation part-way through the error paths is
/// covered as well.
struct AbandonOnDrop<T: Send + 'static> {
    state: Abandonment<T>,
    container_name: String,
    cleanup: PartialCleanup,
}

impl<T: Send + 'static> AbandonOnDrop<T> {
    /// The cleanup is injected so the abandonment contract can be proven without a Docker daemon.
    fn with_cleanup(
        task: tokio::task::JoinHandle<Result<T, TestcontainersError>>,
        container_name: String,
        cleanup: PartialCleanup,
    ) -> Self {
        Self {
            state: Abandonment::Pending(task),
            container_name,
            cleanup,
        }
    }

    fn handle(&mut self) -> &mut tokio::task::JoinHandle<Result<T, TestcontainersError>> {
        match &mut self.state {
            Abandonment::Pending(task) => task,
            _ => panic!("the startup task is joined exactly once"),
        }
    }

    /// Records that the task was joined: awaiting its handle again would panic, so only explicit
    /// removal can still be outstanding.
    fn joined(&mut self, orphan_risk: bool) {
        self.state = if orphan_risk {
            Abandonment::CleanupOnly
        } else {
            Abandonment::Finished
        };
    }

    fn finish(&mut self) {
        self.state = Abandonment::Finished;
    }

    fn owes_cleanup(&self) -> bool {
        matches!(self.state, Abandonment::CleanupOnly)
    }

    fn cleanup(&self) -> PartialCleanup {
        Arc::clone(&self.cleanup)
    }
}

impl<T: Send + 'static> Drop for AbandonOnDrop<T> {
    fn drop(&mut self) {
        let pending = match std::mem::replace(&mut self.state, Abandonment::Finished) {
            Abandonment::Finished => return,
            Abandonment::CleanupOnly => None,
            Abandonment::Pending(task) => {
                task.abort();
                Some(task)
            }
        };

        let container_name = std::mem::take(&mut self.container_name);
        let cleanup = self.cleanup();
        // A failure to spawn must not unwind out of a drop, so the thread is built fallibly.
        let _ = std::thread::Builder::new().spawn(move || {
            // Nothing can be reported or awaited without a runtime, and this thread is the last
            // owner of the outcome, so an unbuildable runtime is where the abandonment ends.
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };

            runtime.block_on(async move {
                if let Some(task) = pending {
                    match tokio::time::timeout(ABANDONED_START_JOIN_BOUND, task).await {
                        // A late handle owns its container; natural cleanup reaps it, with the same
                        // bound and explicit fallback the reporting path uses.
                        Ok(Ok(Ok(started))) => {
                            let _ =
                                detached_drop_diagnostic(started, cleanup(container_name)).await;
                            return;
                        }
                        // A failure Testcontainers already reaped must not be force-removed.
                        Ok(Ok(Err(error))) if !start_failure_can_orphan(&error) => return,
                        _ => {}
                    }
                }

                // There is nobody left to report the outcome to, so it is not rendered.
                let _ = cleanup(container_name).await;
            });
            runtime.shutdown_timeout(DETACHED_DROP_SHUTDOWN_BOUND);
        });
    }
}

/// Whether a Testcontainers start failure can have left a container nothing owns.
///
/// `AsyncRunner::start` creates the container, copies any sources into it, starts it, and only then
/// builds `ContainerAsync`. That constructor calls the infallible `ContainerAsync::construct` before
/// its first fallible step, so every later failure — inspect, port mapping, pre-ready exec, readiness
/// — is reported with an owner in scope, and Testcontainers' own `Drop` has already reaped the
/// container. This is therefore an allowlist of the failures raised in the unowned window: defaulting
/// to removal would risk force-removing a container that belongs to somebody else, which is worse
/// than reporting a leak. `WaitContainerError::StartupTimeout` is included because the runner's
/// startup timeout also covers `start_container`.
fn start_failure_can_orphan(error: &TestcontainersError) -> bool {
    matches!(
        error,
        TestcontainersError::Client(
            ClientError::StartContainer(_)
                | ClientError::UploadToContainerError(_)
                | ClientError::CopyToContainerError(_),
        ) | TestcontainersError::WaitContainer(WaitContainerError::StartupTimeout)
    )
}

/// Removes the exact container a failed start may have created.
///
/// Testcontainers 0.27.3 creates the container before it builds `ContainerAsync` and returns start
/// failures without removing what it created, so such a failure leaves a container that no owner
/// will ever reap. Runtime setup failures happen before any container exists and are skipped.
/// Whether the outstanding removal recorded on the guard applies to this classification, and if so
/// performs it and stands the guard down.
async fn attach_start_failure_cleanup<T: Send + 'static>(
    error: FixtureError,
    guard: &mut AbandonOnDrop<T>,
) -> FixtureError {
    if !guard.owes_cleanup() {
        guard.finish();
        return error;
    }

    // The removal is performed whatever the classification turned out to be; `attach_diagnostics`
    // drops the diagnostic for a variant that carries none rather than the removal being skipped.
    let removal = (guard.cleanup())(guard.container_name.clone()).await;
    guard.finish();
    attach_diagnostics(error, removal)
}

/// Abandons a startup task whose shared deadline expired.
///
/// The task is aborted and awaited so Docker cannot create a container after the caller has given
/// up: a detached task still belongs to the caller's runtime, and a runtime shutdown would cancel
/// the start between container creation and `ContainerAsync` existing. When the race is lost and a
/// real handle arrives late, natural Testcontainers ownership still performs the removal.
///
/// The abort and the removal are bounded outside the shared startup budget, which has already
/// expired by the time this runs.
async fn abandon_owned_start<T: Send + 'static>(
    guard: &mut AbandonOnDrop<T>,
    deadline_error: FixtureError,
) -> FixtureError {
    let container_name = guard.container_name.clone();
    let cleanup = guard.cleanup();
    let remove = || cleanup(container_name.clone());
    guard.handle().abort();

    let joined = tokio::time::timeout(ABANDONED_START_JOIN_BOUND, guard.handle()).await;
    // Awaiting a resolved handle again would panic, so the guard must stop treating it as joinable.
    // Whether removal is still outstanding depends on the outcome and is corrected per branch below.
    if joined.is_ok() {
        guard.joined(true);
    }

    let diagnostics = match joined {
        Ok(Ok(Ok(started))) => {
            guard.joined(false);
            detached_drop_diagnostic(started, remove()).await
        }
        Ok(Ok(Err(error))) => {
            let observation = format!("the abandoned container startup failed: {error}");
            if start_failure_can_orphan(&error) {
                let removal = remove().await;
                guard.finish();
                join_diagnostics(&observation, &removal)
            } else {
                guard.joined(false);
                redacted_tail(&observation)
            }
        }
        Ok(Err(error)) if error.is_panic() => {
            let removal = remove().await;
            guard.finish();
            join_diagnostics(
                &format!("the abandoned container startup panicked: {error}"),
                &removal,
            )
        }
        Ok(Err(_)) => {
            let removal = remove().await;
            guard.finish();
            removal
        }
        // The task never resolved, so it may still be creating a container. The guard deliberately
        // stays armed: its `Drop` joins the task properly and removes whatever it produced.
        Err(_) => join_diagnostics(
            &format!(
                "abandoning the container startup task exceeded {ABANDONED_START_JOIN_BOUND:?}"
            ),
            &remove().await,
        ),
    };

    attach_diagnostics(deadline_error, diagnostics)
}

/// Hands a late owned handle to natural Testcontainers cleanup, falling back to explicit removal.
///
/// Testcontainers' own drop logs removal failures rather than reporting them, so a completed drop is
/// described as handed over, not as proven removed.
async fn detached_drop_diagnostic<T, C>(started: T, cleanup: C) -> String
where
    T: Send + 'static,
    C: Future<Output = String>,
{
    let completed = detach_owned_drop(started);

    match tokio::time::timeout(DETACHED_DROP_JOIN_BOUND, completed).await {
        Ok(Ok(())) => "the owned container startup completed after the deadline; its handle was \
                       handed to natural Testcontainers cleanup, which does not report removal \
                       failures"
            .to_owned(),
        // The handle could not be dropped through a runtime, so the container is still there.
        Ok(Err(_)) => join_diagnostics(
            "the owned container startup completed after the deadline, but its natural cleanup \
             could not run",
            &cleanup.await,
        ),
        Err(_) => join_diagnostics(
            &format!(
                "the owned container startup completed after the deadline; natural cleanup did not \
                 finish within {DETACHED_DROP_JOIN_BOUND:?}"
            ),
            &cleanup.await,
        ),
    }
}

/// Drops a late owned handle on a dedicated thread.
///
/// `ContainerAsync::drop` blocks its thread until Docker removal finishes, so it must not run on the
/// caller's runtime, and it must outlive that runtime to finish at all. The returned receiver
/// resolves once the drop has completed, and is cancelled if no runtime could be built for it.
fn detach_owned_drop<T: Send + 'static>(value: T) -> tokio::sync::oneshot::Receiver<()> {
    let (completed, completion) = tokio::sync::oneshot::channel();

    std::thread::spawn(move || {
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => {
                runtime.block_on(async move { drop(value) });
                runtime.shutdown_timeout(DETACHED_DROP_SHUTDOWN_BOUND);
                let _ = completed.send(());
            }
            // Testcontainers' drop needs a runtime handle and panics without one. Leaking the
            // handle keeps the failure reportable instead of unwinding a detached thread.
            Err(_) => std::mem::forget(value),
        }
    });

    completion
}

async fn remove_partial_container(container_name: &str) -> String {
    // An operator who asked Testcontainers to keep containers must not have one force-removed here.
    if keeps_containers() {
        return format!(
            "left partial container {} in place for TESTCONTAINERS_COMMAND=keep",
            redacted_head(container_name, MAX_CONTAINER_NAME_BYTES)
        );
    }

    // Testcontainers exposes no public owner for a container it created but does not yet hold, so
    // the exact UUID-scoped container and its anonymous volume are removed through the crate's own
    // re-exported Bollard client rather than a second Docker dependency.
    let host = docker_host();
    let docker = match connect_to_docker_host(&host) {
        Ok(docker) => docker,
        Err(error) => {
            return redacted_tail(&format!(
                "could not reach the container runtime at {host} to remove partial container \
                 {container_name}: {error}"
            ));
        }
    };
    let options = RemoveContainerOptionsBuilder::new()
        .v(true)
        .force(true)
        .build();
    let removal = tokio::time::timeout(
        PARTIAL_CLEANUP_BOUND,
        docker.remove_container(container_name, Some(options)),
    )
    .await
    .ok();

    partial_cleanup_diagnostic(container_name, removal)
}

fn keeps_containers() -> bool {
    // Testcontainers parses this variable into its own `Command` without trimming it, so an untrimmed
    // comparison is what actually predicts whether `ContainerAsync::drop` keeps the container.
    std::env::var("TESTCONTAINERS_COMMAND")
        .is_ok_and(|command| command == TESTCONTAINERS_KEEP_COMMAND)
}

/// Resolves the same Docker host Testcontainers resolved when it created the container.
///
/// `bollard`'s own default consults only `DOCKER_HOST` and then one hardcoded socket path, so a
/// rootless or Docker Desktop socket would send the removal to a daemon that never held the
/// container — and a resulting 404 would read as "nothing leaked". Testcontainers' resolution order
/// lives in a `pub(crate)` module, so it is mirrored here. Its `~/.testcontainers.properties`
/// sources are absent because the `properties-config` feature is not enabled.
fn docker_host() -> String {
    resolve_docker_host(
        std::env::var("DOCKER_HOST").ok(),
        runtime_dir(),
        std::env::var("HOME").ok(),
        |path| std::path::Path::new(path).exists(),
    )
}

/// The runtime directory Testcontainers would consult.
///
/// Testcontainers reads it through `etcetera::choose_base_strategy`, which resolves to the XDG
/// strategy on every platform except Windows — including macOS, where only the *native* strategy is
/// Apple's. That strategy ignores a relative `XDG_RUNTIME_DIR`, so this does too: adding or dropping a
/// socket candidate Testcontainers would not have used sends the removal to a daemon that never held
/// the container, whose 404 would then read as though nothing had leaked.
fn runtime_dir() -> Option<String> {
    if cfg!(windows) {
        return None;
    }

    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|dir| std::path::Path::new(dir).is_absolute())
}

/// The pure resolution order, separated from the environment so the mirroring is provable.
fn resolve_docker_host(
    docker_host: Option<String>,
    runtime_dir: Option<String>,
    home_dir: Option<String>,
    exists: impl Fn(&str) -> bool,
) -> String {
    if let Some(host) = docker_host {
        return host;
    }

    let candidates = [
        Some("/var/run/docker.sock".to_owned()),
        runtime_dir.map(|dir| format!("{dir}/.docker/run/docker.sock")),
        home_dir
            .as_deref()
            .map(|dir| format!("{dir}/.docker/run/docker.sock")),
        home_dir.map(|dir| format!("{dir}/.docker/desktop/docker.sock")),
    ];

    candidates
        .into_iter()
        .flatten()
        .find(|path| exists(path))
        .map(|path| format!("unix://{path}"))
        .unwrap_or_else(|| DEFAULT_DOCKER_HOST.to_owned())
}

/// The transports whose Testcontainers connection this crate can reproduce exactly.
#[derive(Debug, PartialEq, Eq)]
enum CleanupTransport {
    Unix,
    Http,
}

fn connect_to_docker_host(host: &str) -> Result<Docker, BollardError> {
    let tls_verify = std::env::var("DOCKER_TLS_VERIFY").is_ok_and(|verify| verify == "1");

    match cleanup_transport(host, tls_verify)? {
        CleanupTransport::Unix => {
            Docker::connect_with_unix(host, DOCKER_CLEANUP_TIMEOUT, API_DEFAULT_VERSION)
        }
        CleanupTransport::Http => {
            Docker::connect_with_http(host, DOCKER_CLEANUP_TIMEOUT, API_DEFAULT_VERSION)
        }
    }
}

/// Mirrors the scheme dispatch Testcontainers performs for the same host string, except for TLS: its
/// certificate material is Testcontainers' own configuration, and connecting in plain HTTP instead
/// would silently talk to nothing, so a TLS host reports an unsupported cleanup rather than guessing.
fn cleanup_transport(host: &str, tls_verify: bool) -> Result<CleanupTransport, BollardError> {
    match Url::parse(host).as_ref().map(Url::scheme) {
        Ok("unix") => Ok(CleanupTransport::Unix),
        Ok("http" | "tcp") if !tls_verify => Ok(CleanupTransport::Http),
        _ => Err(BollardError::UnsupportedURISchemeError {
            uri: host.to_owned(),
        }),
    }
}

fn partial_cleanup_diagnostic(
    container_name: &str,
    removal: Option<Result<(), BollardError>>,
) -> String {
    let container_name = redacted_head(container_name, MAX_CONTAINER_NAME_BYTES);

    match removal {
        Some(Ok(())) => {
            format!("removed partial container {container_name} and its anonymous volume")
        }
        Some(Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        })) => format!("partial container {container_name} did not exist"),
        Some(Err(error)) => redacted_tail(&format!(
            "could not remove partial container {container_name}: {error}"
        )),
        None => format!(
            "removing partial container {container_name} exceeded {PARTIAL_CLEANUP_BOUND:?}"
        ),
    }
}

/// Attaches a bounded tail of the container's own log to a readiness failure.
///
/// The shared budget is gone by the time this runs — that is what the failure means — so the log read
/// gets its own bound, like the other error-path cleanups. A log that cannot be read is reported as
/// such rather than replacing the failure that prompted it.
async fn attach_container_log(
    error: FixtureError,
    container: &ContainerAsync<GenericImage>,
) -> FixtureError {
    let log_deadline = match Deadline::new(DIAGNOSTIC_LOG_BOUND) {
        Ok(deadline) => deadline,
        Err(_) => return error,
    };
    let diagnostics = match bitcoind_log_tail(container, &log_deadline).await {
        Ok(diagnostics) => diagnostics,
        Err(failure) => redacted_tail(&format!(
            "could not read bitcoind diagnostic log within {DIAGNOSTIC_LOG_BOUND:?}: {failure}"
        )),
    };

    attach_diagnostics(error, diagnostics)
}

/// Adds context to whichever error variant carries diagnostics, leaving the rest unchanged.
fn attach_diagnostics(error: FixtureError, addition: String) -> FixtureError {
    match error {
        FixtureError::ContainerStart {
            service,
            image,
            diagnostics,
            source,
        } => FixtureError::ContainerStart {
            service,
            image,
            diagnostics: join_diagnostics(&diagnostics, &addition),
            source,
        },
        FixtureError::ReadinessTimeout {
            service,
            duration,
            last_observation,
            diagnostics,
        } => FixtureError::ReadinessTimeout {
            service,
            duration,
            last_observation,
            diagnostics: join_diagnostics(&diagnostics, &addition),
        },
        other => other,
    }
}

fn classify_start_error(image: &ContainerImage, error: TestcontainersError) -> FixtureError {
    match error {
        TestcontainersError::Client(ClientError::Init(source)) => {
            FixtureError::RuntimeUnavailable {
                source: redacted_source(source),
            }
        }
        TestcontainersError::Client(ClientError::Configuration(source)) => {
            FixtureError::RuntimeUnavailable {
                source: redacted_source(source),
            }
        }
        TestcontainersError::Client(ClientError::InvalidDockerHost(detail)) => {
            FixtureError::RuntimeUnavailable {
                source: redacted_source(io::Error::other(detail)),
            }
        }
        // The Docker failure itself is the diagnostic a caller needs; `Display` would otherwise
        // report nothing for an ordinary start failure.
        source => {
            let diagnostics = format!("{source}");
            container_start_error(image, diagnostics, source)
        }
    }
}

fn container_start_join_error(
    image: &ContainerImage,
    source: tokio::task::JoinError,
) -> FixtureError {
    container_start_error(
        image,
        redacted_tail(&format!("owned container startup task failed: {source}")),
        source,
    )
}

fn container_start_error(
    image: &ContainerImage,
    diagnostics: String,
    source: impl std::error::Error + Send + Sync + 'static,
) -> FixtureError {
    FixtureError::ContainerStart {
        service: "bitcoind",
        image: redacted_head(
            &format!("{}:{}", image.name(), image.testcontainers_tag()),
            MAX_IMAGE_DESCRIPTOR_BYTES,
        ),
        diagnostics: redacted_tail(&diagnostics),
        source: redacted_source(source),
    }
}

fn bootstrap_error(operation: &'static str, source: NigiriError) -> FixtureError {
    FixtureError::Bootstrap {
        operation,
        diagnostics: redacted_tail(&source.to_string()),
        source: redacted_source(source),
    }
}

fn port_discovery_error(error: TestcontainersError, diagnostics: &str) -> FixtureError {
    FixtureError::PortDiscovery {
        service: "bitcoind",
        container_port: RPC_PORT,
        diagnostics: redacted_tail(diagnostics),
        source: redacted_source(error),
    }
}

fn port_discovery_with_log_result(
    error: TestcontainersError,
    log_result: Result<String, FixtureError>,
) -> FixtureError {
    let diagnostics = match log_result {
        Ok(diagnostics) => diagnostics,
        Err(error) => redacted_tail(&format!("could not read bitcoind diagnostic log: {error}")),
    };

    port_discovery_error(error, &diagnostics)
}

#[cfg_attr(test, allow(dead_code))]
async fn bitcoind_log_tail(
    container: &ContainerAsync<GenericImage>,
    deadline: &Deadline,
) -> Result<String, FixtureError> {
    let output = deadline
        .run("bitcoind", "reading Bitcoind diagnostic log", async {
            let mut log_reader = container.stdout(false);
            let mut tail = Vec::with_capacity(ROLLING_LOG_BYTES);
            let mut chunk = [0_u8; 4 * 1024];

            loop {
                let read = log_reader.read(&mut chunk).await?;
                if read == 0 {
                    break Ok::<Vec<u8>, io::Error>(tail);
                }
                append_log_tail(&mut tail, &chunk[..read]);
            }
        })
        .await?;

    match output {
        Ok(bytes) => Ok(redacted_tail(&format!(
            "bitcoind stdout:\n{}",
            String::from_utf8_lossy(&bytes)
        ))),
        Err(error) => Ok(redacted_tail(&format!(
            "could not read bitcoind diagnostic log: {error}"
        ))),
    }
}

fn append_log_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
    if chunk.len() >= ROLLING_LOG_BYTES {
        tail.clear();
        tail.extend_from_slice(&chunk[chunk.len() - ROLLING_LOG_BYTES..]);
        return;
    }

    let required = tail.len() + chunk.len();
    if required > ROLLING_LOG_BYTES {
        let discarded = required - ROLLING_LOG_BYTES;
        tail.copy_within(discarded.., 0);
        tail.truncate(tail.len() - discarded);
    }
    tail.extend_from_slice(chunk);
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        future::{Future, pending},
        io,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread::{self, ThreadId},
        time::Duration,
    };

    use testcontainers::{
        ContainerRequest, GenericImage, Image,
        bollard::errors::Error as BollardError,
        core::{
            IntoContainerPort,
            error::{ClientError, TestcontainersError, WaitContainerError},
        },
    };
    use tokio::sync::{Barrier, Notify, mpsc, oneshot};
    use url::Url;

    use super::{
        AbandonOnDrop, CleanupTransport, InitialMiningGate, MAX_DIAGNOSTIC_BYTES,
        MAX_IMAGE_DESCRIPTOR_BYTES, MAX_SOURCE_BYTES, PartialCleanup, ROLLING_LOG_BYTES,
        abandon_owned_start, append_log_tail, attach_diagnostics, bootstrap_error,
        classify_start_error, cleanup_transport, container_start_join_error, fixture_client,
        fixture_rpc_config, partial_cleanup_diagnostic, port_discovery_error,
        port_discovery_with_log_result, redacted_tail, request, resolve_docker_host,
        run_owned_start, run_owned_start_with_cleanup, start_failure_can_orphan, wallet_rpc_url,
    };
    use crate::{ContainerImage, FixtureError, deadline::Deadline};
    use nigiri_rs::NigiriError;

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

    /// Reports the thread on which it was dropped, so a detached drop is distinguishable from an
    /// inline one.
    #[derive(Debug)]
    struct DropRecorder(Option<oneshot::Sender<ThreadId>>);

    impl Drop for DropRecorder {
        fn drop(&mut self) {
            if let Some(dropped) = self.0.take() {
                let _ = dropped.send(thread::current().id());
            }
        }
    }

    /// Counts removals instead of performing them, so no test touches a Docker daemon.
    #[derive(Clone)]
    struct CountingCleanup {
        removals: Arc<AtomicUsize>,
        names: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl CountingCleanup {
        fn new() -> Self {
            Self {
                removals: Arc::new(AtomicUsize::new(0)),
                names: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn as_partial_cleanup(&self) -> PartialCleanup {
            let counter = self.clone();
            Arc::new(move |container_name: String| {
                let counter = counter.clone();
                Box::pin(async move {
                    counter.removals.fetch_add(1, Ordering::SeqCst);
                    counter
                        .names
                        .lock()
                        .expect("the recorded names are never poisoned")
                        .push(container_name.clone());
                    format!("removed partial container {container_name} and its anonymous volume")
                })
            })
        }

        fn removals(&self) -> usize {
            self.removals.load(Ordering::SeqCst)
        }

        fn recorded_names(&self) -> Vec<String> {
            self.names
                .lock()
                .expect("the recorded names are never poisoned")
                .clone()
        }
    }

    fn guarded_startup<T, F>(future: F, cleanup: &CountingCleanup) -> AbandonOnDrop<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, TestcontainersError>> + Send + 'static,
    {
        AbandonOnDrop::with_cleanup(
            tokio::spawn(future),
            "nigiri-bitcoind-abc".to_owned(),
            cleanup.as_partial_cleanup(),
        )
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

    // Catches a regression that exposes an unbounded log body or fixture credentials in a port
    // discovery diagnostic.
    #[test]
    fn port_discovery_diagnostic_is_utf8_bounded_and_redacts_credentials() {
        let diagnostics = format!(
            "{} -rpcuser=admin1 -rpcpassword=123 admin1:123",
            "bitcoin-log-".repeat(2_000),
        );
        let error = port_discovery_error(
            TestcontainersError::PortNotExposed {
                id: "bitcoin-id".to_owned(),
                port: 18_443.tcp(),
            },
            &diagnostics,
        );

        let FixtureError::PortDiscovery {
            service,
            container_port,
            diagnostics,
            ..
        } = error
        else {
            panic!("a mapped-port error must remain a port-discovery failure");
        };
        assert_eq!(service, "bitcoind");
        assert_eq!(container_port, 18_443);
        assert!(diagnostics.len() <= 16 * 1024);
        assert!(!diagnostics.contains("admin1"));
        assert!(!diagnostics.contains("=123"));
        assert!(
            diagnostics.ends_with("[REDACTED]"),
            "the terminal end of the diagnostic must be what is retained"
        );
        assert_eq!(diagnostics, redacted_tail(&diagnostics));
    }

    // Catches a regression that keeps the first 16 KiB of Bitcoind stdout rather than its
    // terminal diagnostic tail, including when the retained boundary crosses UTF-8 text.
    #[test]
    fn rolling_log_tail_keeps_a_terminal_utf8_marker_after_early_filler() {
        const MARKER: &str = "terminal-marker-🦀";

        let mut tail = Vec::new();
        append_log_tail(&mut tail, "早".repeat(16 * 1024).as_bytes());
        append_log_tail(&mut tail, format!("\n{MARKER}").as_bytes());

        let diagnostic = redacted_tail(&String::from_utf8_lossy(&tail));
        assert!(diagnostic.len() <= 16 * 1024);
        assert!(diagnostic.ends_with(MARKER));
        assert!(
            diagnostic.starts_with('早'),
            "the retained tail must begin with a whole character"
        );
    }

    // Catches a regression that shrinks the rolling buffer's redaction slack. A credential whose
    // leading bytes are discarded at the buffer's front boundary can no longer be matched by
    // redaction, so the slack must keep the surviving fragment outside the retained tail too.
    #[test]
    fn a_credential_split_by_the_rolling_discard_stays_outside_the_retained_tail() {
        const SECRET: &str = "-rpcpassword=123";
        const MARKER: &str = "terminal-marker-🦀";
        const DISCARDED: usize = 8;

        let mut tail = Vec::new();
        append_log_tail(&mut tail, SECRET.as_bytes());
        append_log_tail(
            &mut tail,
            format!(
                "{}{MARKER}",
                "z".repeat(ROLLING_LOG_BYTES - SECRET.len() + DISCARDED - MARKER.len()),
            )
            .as_bytes(),
        );

        let retained = String::from_utf8_lossy(&tail);
        assert!(
            retained.starts_with("word=123"),
            "the buffer must have discarded the credential's leading bytes"
        );

        let diagnostic = redacted_tail(&retained);
        assert!(diagnostic.ends_with(MARKER));
        assert!(!diagnostic.contains("word"));
        assert!(!diagnostic.contains("123"));
    }

    // Catches a regression that classifies Docker runtime setup by display text rather than the
    // concrete Testcontainers client variants.
    #[test]
    fn runtime_client_error_variants_are_classified_as_runtime_unavailable() {
        for error in [
            TestcontainersError::Client(ClientError::InvalidDockerHost(
                "unix:///not-a-docker-socket".to_owned(),
            )),
            TestcontainersError::Client(ClientError::Configuration(
                testcontainers::core::error::ConfigurationError::InvalidDockerHost(
                    "not-a-host".to_owned(),
                ),
            )),
        ] {
            assert!(matches!(
                classify_start_error(&ContainerImage::bitcoind_default(), error),
                FixtureError::RuntimeUnavailable { .. }
            ));
        }
    }

    // Catches a regression that turns ordinary Testcontainers startup failures into a runtime
    // availability error.
    #[test]
    fn non_runtime_start_errors_are_classified_as_container_start() {
        let error = classify_start_error(
            &ContainerImage::bitcoind_default(),
            TestcontainersError::other(std::io::Error::other("container create failed")),
        );

        assert!(matches!(
            error,
            FixtureError::ContainerStart {
                service: "bitcoind",
                ..
            }
        ));
    }

    fn expired_startup() -> FixtureError {
        FixtureError::ReadinessTimeout {
            service: "bitcoind",
            duration: Duration::from_secs(60),
            last_observation: "starting Bitcoind container".to_owned(),
            diagnostics: String::new(),
        }
    }

    // Catches a regression that leaves a Docker start future running past its expired deadline.
    // Testcontainers 0.27.3 creates the container before `ContainerAsync` exists, so an
    // unabandoned future can create a container that no owner will ever remove.
    #[tokio::test]
    async fn abandoning_an_expired_start_cancels_the_future_and_records_partial_cleanup() {
        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let cleanup = CountingCleanup::new();
        let mut guard = guarded_startup(
            async move {
                let _recorder = DropRecorder(Some(dropped_tx));
                started_tx
                    .send(())
                    .expect("the startup future must report entry");
                pending::<Result<DropRecorder, TestcontainersError>>().await
            },
            &cleanup,
        );
        started_rx
            .await
            .expect("the startup future must be running before it is abandoned");

        let error = abandon_owned_start(&mut guard, expired_startup()).await;
        assert_eq!(cleanup.removals(), 1);

        tokio::time::timeout(Duration::from_secs(2), dropped_rx)
            .await
            .expect("the abandoned startup future must be cancelled, not detached")
            .expect("cancellation must drop everything the startup future owned");

        let FixtureError::ReadinessTimeout {
            service,
            duration,
            last_observation,
            diagnostics,
        } = error
        else {
            panic!("an expired startup must remain a readiness timeout");
        };
        assert_eq!(service, "bitcoind");
        assert_eq!(duration, Duration::from_secs(60));
        assert_eq!(last_observation, "starting Bitcoind container");
        assert_eq!(
            diagnostics,
            "removed partial container nigiri-bitcoind-abc and its anonymous volume"
        );
    }

    // Catches a regression that removes a container Testcontainers already owns instead of letting
    // its natural cleanup run, or that performs that blocking Docker drop on the caller's runtime.
    #[tokio::test]
    async fn abandoning_a_late_started_container_detaches_natural_cleanup_without_removal() {
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let cleanup = CountingCleanup::new();
        let mut guard = guarded_startup(
            async move { Ok::<DropRecorder, TestcontainersError>(DropRecorder(Some(dropped_tx))) },
            &cleanup,
        );
        while !guard.handle().is_finished() {
            tokio::task::yield_now().await;
        }

        let error = abandon_owned_start(&mut guard, expired_startup()).await;

        let dropped_on = tokio::time::timeout(Duration::from_secs(5), dropped_rx)
            .await
            .expect("a late owned handle must still be dropped")
            .expect("the detached drop must release the owned handle");
        assert_ne!(
            dropped_on,
            thread::current().id(),
            "Testcontainers' blocking drop must not run on the caller's thread"
        );
        assert_eq!(
            cleanup.removals(),
            0,
            "a container Testcontainers owns must not be removed behind its back"
        );

        let FixtureError::ReadinessTimeout { diagnostics, .. } = error else {
            panic!("an expired startup must remain a readiness timeout");
        };
        assert!(
            diagnostics.contains("handed to natural Testcontainers cleanup"),
            "{diagnostics}"
        );
        assert!(
            !diagnostics.contains("removed"),
            "natural cleanup does not report removal, so the diagnostic must not claim it"
        );
    }

    // Catches a regression that lets a cancelled caller detach its startup task. Dropping a
    // `JoinHandle` detaches rather than aborts, so Docker would create a container after the caller
    // is gone, with nothing left to own it — and nothing would remove what it created either.
    #[tokio::test]
    async fn a_cancelled_caller_abandons_and_removes_its_partial_container() {
        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let cleanup = CountingCleanup::new();
        let guard = guarded_startup(
            async move {
                let _recorder = DropRecorder(Some(dropped_tx));
                started_tx
                    .send(())
                    .expect("the startup future must report entry");
                pending::<Result<(), TestcontainersError>>().await
            },
            &cleanup,
        );

        started_rx
            .await
            .expect("the startup future must be running before the caller is cancelled");
        // Dropping the guard is exactly what happens when the caller's future is cancelled.
        drop(guard);

        tokio::time::timeout(Duration::from_secs(2), dropped_rx)
            .await
            .expect("cancelling the caller must abort the startup task, not detach it")
            .expect("the aborted task must drop everything it owned");
        for _ in 0..100 {
            if cleanup.removals() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            cleanup.removals(),
            1,
            "a cancelled caller must still have its partial container removed"
        );
        assert_eq!(
            cleanup.recorded_names(),
            ["nigiri-bitcoind-abc"],
            "only the fixture's own container may be removed"
        );
    }

    // Catches a regression that force-removes a container Testcontainers already reaped when the
    // caller is cancelled, which would destroy whatever else bears the name.
    #[tokio::test]
    async fn a_cancelled_caller_does_not_remove_a_reaped_start_failure() {
        let cleanup = CountingCleanup::new();
        let mut guard = guarded_startup(
            async {
                Err::<(), TestcontainersError>(TestcontainersError::WaitContainer(
                    WaitContainerError::Unhealthy,
                ))
            },
            &cleanup,
        );
        while !guard.handle().is_finished() {
            tokio::task::yield_now().await;
        }

        drop(guard);
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(
            cleanup.removals(),
            0,
            "a failure Testcontainers already reaped must not be force-removed"
        );
    }

    // Catches a regression that stands the abandonment guard down before its own cleanup has run, so
    // a caller cancelled during error handling would leave the partial container behind.
    #[tokio::test]
    async fn a_caller_cancelled_during_cleanup_still_removes_its_partial_container() {
        let cleanup = CountingCleanup::new();
        let mut guard = guarded_startup(
            async {
                Err::<(), TestcontainersError>(TestcontainersError::Client(
                    ClientError::StartContainer(BollardError::DockerResponseServerError {
                        status_code: 500,
                        message: "start failed".to_owned(),
                    }),
                ))
            },
            &cleanup,
        );
        while !guard.handle().is_finished() {
            tokio::task::yield_now().await;
        }

        // The outcome was observed, but its removal had not run yet when the caller went away.
        guard.joined(true);
        drop(guard);
        for _ in 0..100 {
            if cleanup.removals() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(cleanup.removals(), 1);
    }

    // Catches a regression that force-removes a container this fixture never created. Only the
    // failures raised between `create_container` and `ContainerAsync::construct` can leave a container
    // nobody owns; a create conflict means the name belongs to somebody else, and everything after
    // `construct` was already reaped by Testcontainers' own `Drop`.
    #[test]
    fn only_start_failures_that_can_orphan_are_treated_as_removable() {
        let conflicting_create = TestcontainersError::Client(ClientError::CreateContainer(
            BollardError::DockerResponseServerError {
                status_code: 409,
                message: "Conflict. The container name is already in use".to_owned(),
            },
        ));
        assert!(!start_failure_can_orphan(&conflicting_create));
        assert!(!start_failure_can_orphan(&TestcontainersError::Client(
            ClientError::PullImage {
                descriptor: "registry.example/bitcoin:v1".to_owned(),
                err: BollardError::DockerResponseServerError {
                    status_code: 404,
                    message: "manifest unknown".to_owned(),
                },
            }
        )));
        assert!(!start_failure_can_orphan(
            &TestcontainersError::WaitContainer(WaitContainerError::Unhealthy)
        ));
        assert!(!start_failure_can_orphan(
            &TestcontainersError::WaitContainer(WaitContainerError::UnexpectedExitCode {
                expected: 0,
                actual: Some(1),
            })
        ));
        assert!(!start_failure_can_orphan(&TestcontainersError::Exec(
            testcontainers::core::error::ExecError::ExitCodeMismatch {
                expected: 0,
                actual: 1,
            }
        )));

        assert!(start_failure_can_orphan(&TestcontainersError::Client(
            ClientError::StartContainer(BollardError::DockerResponseServerError {
                status_code: 500,
                message: "start failed".to_owned(),
            })
        )));
        assert!(start_failure_can_orphan(&TestcontainersError::Client(
            ClientError::UploadToContainerError(BollardError::DockerResponseServerError {
                status_code: 500,
                message: "upload failed".to_owned(),
            })
        )));
        assert!(start_failure_can_orphan(
            &TestcontainersError::WaitContainer(WaitContainerError::StartupTimeout)
        ));

        // Inspection and port mapping only run once `ContainerAsync::construct` has produced an
        // owner, and an unrecognised failure is not assumed to have created anything.
        assert!(!start_failure_can_orphan(&TestcontainersError::Client(
            ClientError::InspectContainer(BollardError::DockerResponseServerError {
                status_code: 500,
                message: "inspect failed".to_owned(),
            })
        )));
        assert!(!start_failure_can_orphan(&TestcontainersError::other(
            io::Error::other("an unrecognised failure")
        )));
    }

    // Catches a regression that abandons a startup failure by force-removing a container
    // Testcontainers had already owned and reaped.
    #[tokio::test]
    async fn abandoning_a_reaped_start_failure_does_not_remove_anything() {
        let cleanup = CountingCleanup::new();
        let mut guard = guarded_startup(
            async {
                Err::<(), TestcontainersError>(TestcontainersError::WaitContainer(
                    WaitContainerError::Unhealthy,
                ))
            },
            &cleanup,
        );
        while !guard.handle().is_finished() {
            tokio::task::yield_now().await;
        }

        let error = abandon_owned_start(&mut guard, expired_startup()).await;

        assert_eq!(
            cleanup.removals(),
            0,
            "a container Testcontainers already reaped must not be force-removed"
        );
        let FixtureError::ReadinessTimeout { diagnostics, .. } = error else {
            panic!("an expired startup must remain a readiness timeout");
        };
        assert!(
            diagnostics.contains("the abandoned container startup failed"),
            "{diagnostics}"
        );
    }

    // Catches a regression that resolves the cleanup daemon through bollard's own default, which
    // consults only DOCKER_HOST and one hardcoded socket. Removal would then reach a daemon that
    // never held the container, and its 404 would read as "nothing leaked".
    #[test]
    fn the_cleanup_docker_host_mirrors_testcontainers_resolution_order() {
        let all_present = |_: &str| true;

        assert_eq!(
            resolve_docker_host(
                Some("tcp://docker.example:2375".to_owned()),
                Some("/run/user/1000".to_owned()),
                Some("/home/fixture".to_owned()),
                all_present,
            ),
            "tcp://docker.example:2375"
        );
        assert_eq!(
            resolve_docker_host(
                None,
                Some("/run/user/1000".to_owned()),
                Some("/home/fixture".to_owned()),
                all_present,
            ),
            "unix:///var/run/docker.sock"
        );
        assert_eq!(
            resolve_docker_host(
                None,
                Some("/run/user/1000".to_owned()),
                Some("/home/fixture".to_owned()),
                |path| path != "/var/run/docker.sock",
            ),
            "unix:///run/user/1000/.docker/run/docker.sock"
        );
        assert_eq!(
            resolve_docker_host(None, None, Some("/home/fixture".to_owned()), |path| path
                != "/var/run/docker.sock"),
            "unix:///home/fixture/.docker/run/docker.sock"
        );
        assert_eq!(
            resolve_docker_host(None, None, Some("/home/fixture".to_owned()), |path| path
                .ends_with("/.docker/desktop/docker.sock")),
            "unix:///home/fixture/.docker/desktop/docker.sock"
        );
        assert_eq!(
            resolve_docker_host(None, None, None, |_| false),
            "unix:///var/run/docker.sock"
        );
    }

    // Catches a regression that reproduces a Testcontainers connection this crate cannot actually
    // reproduce — above all guessing plain HTTP for a TLS-verified Docker host, which would remove
    // nothing while reporting a connection failure.
    #[test]
    fn only_reproducible_docker_transports_are_accepted_for_cleanup() {
        assert_eq!(
            cleanup_transport("unix:///var/run/docker.sock", false).ok(),
            Some(CleanupTransport::Unix)
        );
        assert_eq!(
            cleanup_transport("tcp://docker.example:2375", false).ok(),
            Some(CleanupTransport::Http)
        );
        assert_eq!(
            cleanup_transport("http://docker.example:2375", false).ok(),
            Some(CleanupTransport::Http)
        );

        for (host, tls_verify) in [
            ("tcp://docker.example:2376", true),
            ("http://docker.example:2375", true),
            ("https://docker.example:2376", false),
            ("npipe:////./pipe/docker_engine", false),
            ("/var/run/docker.sock", false),
        ] {
            assert!(
                matches!(
                    cleanup_transport(host, tls_verify),
                    Err(BollardError::UnsupportedURISchemeError { .. })
                ),
                "{host} (tls_verify={tls_verify}) must not be reproduced"
            );
        }
    }

    // Catches a regression that reports the failing image descriptor unbounded, or that renders an
    // ordinary Docker start failure with no diagnostic at all.
    #[test]
    fn container_start_reports_a_bounded_descriptor_and_the_docker_failure() {
        let error = classify_start_error(
            &ContainerImage::new(format!("registry.example/{}", "b".repeat(4 * 1024)), "v1"),
            TestcontainersError::other(io::Error::other("container create failed")),
        );

        let FixtureError::ContainerStart {
            image, diagnostics, ..
        } = error
        else {
            panic!("an ordinary Docker failure must remain a container-start failure");
        };
        assert!(image.len() <= MAX_IMAGE_DESCRIPTOR_BYTES);
        assert!(image.starts_with("registry.example/b"));
        assert!(
            diagnostics.contains("container create failed"),
            "{diagnostics}"
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

    // Catches a regression that overwrites existing diagnostics with partial-cleanup context or
    // rewrites a fixture error that carries no diagnostics at all.
    #[test]
    fn partial_cleanup_context_is_appended_only_to_diagnostic_bearing_errors() {
        let start = attach_diagnostics(
            FixtureError::ContainerStart {
                service: "bitcoind",
                image: "registry.example/bitcoin:v1".to_owned(),
                diagnostics: "docker rejected the create".to_owned(),
                source: Box::new(io::Error::other("create failed")),
            },
            "removed partial container nigiri-bitcoind-abc".to_owned(),
        );
        let FixtureError::ContainerStart { diagnostics, .. } = start else {
            panic!("attaching cleanup context must preserve the container-start classification");
        };
        assert_eq!(
            diagnostics,
            "docker rejected the create; removed partial container nigiri-bitcoind-abc"
        );

        let unrelated = attach_diagnostics(
            FixtureError::InvalidConfiguration {
                detail: "invalid image".to_owned(),
            },
            "removed partial container nigiri-bitcoind-abc".to_owned(),
        );
        let FixtureError::InvalidConfiguration { detail } = unrelated else {
            panic!("an error without diagnostics must be returned unchanged");
        };
        assert_eq!(detail, "invalid image");
    }

    // Catches a regression that classifies an already-absent partial container by Docker's message
    // text rather than its status code, or that leaks an unbounded removal failure.
    #[test]
    fn partial_cleanup_diagnostics_are_classified_by_status_code_and_bounded() {
        assert_eq!(
            partial_cleanup_diagnostic("nigiri-bitcoind-abc", Some(Ok(()))),
            "removed partial container nigiri-bitcoind-abc and its anonymous volume"
        );
        assert_eq!(
            partial_cleanup_diagnostic(
                "nigiri-bitcoind-abc",
                Some(Err(BollardError::DockerResponseServerError {
                    status_code: 404,
                    message: "No such container: nigiri-bitcoind-abc".to_owned(),
                })),
            ),
            "partial container nigiri-bitcoind-abc did not exist"
        );

        let noisy = partial_cleanup_diagnostic(
            "nigiri-bitcoind-abc",
            Some(Err(BollardError::DockerResponseServerError {
                status_code: 500,
                message: format!("{} admin1:123", "docker-error-".repeat(2_000)),
            })),
        );
        assert!(noisy.len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(!noisy.contains("admin1:123"));

        assert!(
            partial_cleanup_diagnostic("nigiri-bitcoind-abc", None)
                .contains("removing partial container nigiri-bitcoind-abc exceeded")
        );
    }

    // Catches a regression on the ordinary start-failure path: the container Docker created before
    // the failure must be removed by exact name, and the removal must be reported in the error.
    #[tokio::test]
    async fn a_failed_start_removes_exactly_its_own_partial_container() {
        let deadline =
            Deadline::new(Duration::from_secs(60)).expect("a positive deadline is valid");
        let cleanup = CountingCleanup::new();

        let error = run_owned_start_with_cleanup::<(), _>(
            &ContainerImage::bitcoind_default(),
            &deadline,
            "nigiri-bitcoind-abc",
            "starting Bitcoind container",
            async {
                Err(TestcontainersError::Client(ClientError::StartContainer(
                    BollardError::DockerResponseServerError {
                        status_code: 500,
                        message: "start failed".to_owned(),
                    },
                )))
            },
            cleanup.as_partial_cleanup(),
        )
        .await
        .expect_err("a failed container start must not succeed");

        assert_eq!(cleanup.recorded_names(), ["nigiri-bitcoind-abc"]);
        let FixtureError::ContainerStart { diagnostics, .. } = error else {
            panic!("a failed start must remain a container-start failure");
        };
        assert!(diagnostics.contains("start failed"), "{diagnostics}");
        assert!(
            diagnostics.contains("removed partial container nigiri-bitcoind-abc"),
            "{diagnostics}"
        );
    }

    // Catches a regression that force-removes on a start failure Testcontainers already reaped, on
    // the ordinary reporting path rather than the abandonment path.
    #[tokio::test]
    async fn a_reaped_start_failure_removes_nothing_and_still_reports() {
        let deadline =
            Deadline::new(Duration::from_secs(60)).expect("a positive deadline is valid");
        let cleanup = CountingCleanup::new();

        let error = run_owned_start_with_cleanup::<(), _>(
            &ContainerImage::bitcoind_default(),
            &deadline,
            "nigiri-bitcoind-abc",
            "starting Bitcoind container",
            async {
                Err(TestcontainersError::WaitContainer(
                    WaitContainerError::Unhealthy,
                ))
            },
            cleanup.as_partial_cleanup(),
        )
        .await
        .expect_err("an unhealthy container must not succeed");

        assert_eq!(cleanup.removals(), 0);
        assert!(matches!(error, FixtureError::ContainerStart { .. }));
    }

    // Catches a regression that fails to hand a ready startup result back to the caller.
    #[tokio::test]
    async fn owned_start_returns_a_ready_result() {
        let deadline =
            Deadline::new(Duration::from_secs(60)).expect("a positive deadline is valid");

        let value = run_owned_start(
            &ContainerImage::bitcoind_default(),
            &deadline,
            "nigiri-bitcoind-abc",
            "starting Bitcoind container",
            async { Ok::<u8, TestcontainersError>(7) },
        )
        .await
        .expect("a ready owned start must succeed");

        assert_eq!(value, 7);
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
                &image,
                TestcontainersError::other(io::Error::other(secret_body.clone())),
            ),
            classify_start_error(
                &image,
                TestcontainersError::Client(ClientError::InvalidDockerHost(secret_body.clone())),
            ),
            port_discovery_error(
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

    // Catches a regression that turns a failed owned startup task into an unclassified join
    // error or emits an unbounded task diagnostic.
    #[tokio::test]
    async fn owned_start_join_errors_become_bounded_container_start_errors() {
        let task = tokio::spawn(async { pending::<()>().await });
        task.abort();
        let join_error = task
            .await
            .expect_err("an aborted task must report a join error");

        let error = container_start_join_error(&ContainerImage::bitcoind_default(), join_error);
        let FixtureError::ContainerStart {
            service,
            diagnostics,
            ..
        } = error
        else {
            panic!("a failed owned startup task must remain a container-start failure");
        };
        assert_eq!(service, "bitcoind");
        assert!(diagnostics.len() <= MAX_DIAGNOSTIC_BYTES);
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

    // Catches a regression that discards a known mapped-port failure when bounded log collection
    // reaches the shared startup deadline.
    #[test]
    fn port_discovery_survives_diagnostic_collection_timeout() {
        let log_timeout = FixtureError::ReadinessTimeout {
            service: "bitcoind",
            duration: Duration::from_secs(1),
            last_observation: "-rpcpassword=123".to_owned(),
            diagnostics: "admin1:123".to_owned(),
        };
        let error = port_discovery_with_log_result(
            TestcontainersError::PortNotExposed {
                id: "bitcoin-id".to_owned(),
                port: 18_443.tcp(),
            },
            Err(log_timeout),
        );
        assert!(
            Error::source(&error)
                .map(ToString::to_string)
                .is_some_and(|source| source.contains("does not expose port"))
        );

        let FixtureError::PortDiscovery {
            service,
            container_port,
            diagnostics,
            ..
        } = error
        else {
            panic!("a known mapped-port error must not be replaced by a log timeout");
        };
        assert_eq!(service, "bitcoind");
        assert_eq!(container_port, 18_443);
        assert!(diagnostics.len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(!diagnostics.contains("-rpcpassword=123"));
        assert!(!diagnostics.contains("admin1:123"));
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
