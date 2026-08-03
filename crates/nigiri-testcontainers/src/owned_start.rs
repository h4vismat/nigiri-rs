//! Starting a container this crate can always account for.
//!
//! Testcontainers 0.27.3 creates a container before `ContainerAsync` exists and returns start
//! failures without removing what it created, so a cancelled or timed-out start can leave a container
//! nobody owns. Everything here exists to make that impossible: a startup task is abandoned rather
//! than detached, natural Testcontainers ownership is preserved wherever a handle exists, and only a
//! container created inside that unowned window is removed explicitly, by exact name.
//!
//! Every entry point takes the service it is starting, so Bitcoind and Electrs share one
//! implementation and report failures under their own name.

#![cfg_attr(not(test), allow(dead_code))]

use std::{future::Future, io, pin::Pin, sync::Arc, time::Duration};

use testcontainers::{
    ContainerAsync, GenericImage,
    bollard::{
        API_DEFAULT_VERSION, Docker, errors::Error as BollardError,
        query_parameters::RemoveContainerOptionsBuilder,
    },
    core::error::{ClientError, TestcontainersError, WaitContainerError},
};
use tokio::io::AsyncReadExt;
use url::Url;

use crate::{
    ContainerImage, FixtureError,
    deadline::Deadline,
    diagnostics::{
        MAX_DIAGNOSTIC_BYTES, MAX_REDACTION_CONTEXT_BYTES, join_diagnostics, redacted_head,
        redacted_source, redacted_tail,
    },
};

const UTF8_TAIL_LOOKBEHIND_BYTES: usize = 3;
pub(crate) const ROLLING_LOG_BYTES: usize =
    MAX_DIAGNOSTIC_BYTES + UTF8_TAIL_LOOKBEHIND_BYTES + MAX_REDACTION_CONTEXT_BYTES;
const MAX_CONTAINER_NAME_BYTES: usize = 256;
pub(crate) const MAX_IMAGE_DESCRIPTOR_BYTES: usize = 512;
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

pub(crate) async fn run_owned_start<T, F>(
    service: &'static str,
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
        service,
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
    service: &'static str,
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
        .run(service, last_observation, guard.handle())
        .await;

    match joined {
        Ok(Ok(Ok(value))) => {
            guard.finish();
            Ok(value)
        }
        Ok(Ok(Err(error))) => {
            let orphan_risk = start_failure_can_orphan(&error);
            guard.joined(orphan_risk);
            let classified = attach_start_failure_cleanup(
                classify_start_error(service, image, error),
                &mut guard,
            )
            .await;
            Err(classified)
        }
        // A panicking startup task can orphan whatever it had already created.
        Ok(Err(error)) => {
            guard.joined(true);
            let classified = attach_start_failure_cleanup(
                container_start_join_error(service, image, error),
                &mut guard,
            )
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
pub(crate) async fn attach_container_log(
    service: &'static str,
    error: FixtureError,
    container: &ContainerAsync<GenericImage>,
) -> FixtureError {
    let log_deadline = match Deadline::new(DIAGNOSTIC_LOG_BOUND) {
        Ok(deadline) => deadline,
        Err(_) => return error,
    };
    let diagnostics = match container_log_tail(service, container, &log_deadline).await {
        Ok(diagnostics) => diagnostics,
        Err(failure) => redacted_tail(&format!(
            "could not read the {service} diagnostic log within {DIAGNOSTIC_LOG_BOUND:?}: {failure}"
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

pub(crate) fn classify_start_error(
    service: &'static str,
    image: &ContainerImage,
    error: TestcontainersError,
) -> FixtureError {
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
            container_start_error(service, image, diagnostics, source)
        }
    }
}

fn container_start_join_error(
    service: &'static str,
    image: &ContainerImage,
    source: tokio::task::JoinError,
) -> FixtureError {
    container_start_error(
        service,
        image,
        redacted_tail(&format!("owned container startup task failed: {source}")),
        source,
    )
}

fn container_start_error(
    service: &'static str,
    image: &ContainerImage,
    diagnostics: String,
    source: impl std::error::Error + Send + Sync + 'static,
) -> FixtureError {
    FixtureError::ContainerStart {
        service,
        image: redacted_head(
            &format!("{}:{}", image.name(), image.testcontainers_tag()),
            MAX_IMAGE_DESCRIPTOR_BYTES,
        ),
        diagnostics: redacted_tail(&diagnostics),
        source: redacted_source(source),
    }
}

pub(crate) fn port_discovery_error(
    service: &'static str,
    container_port: u16,
    error: TestcontainersError,
    diagnostics: &str,
) -> FixtureError {
    FixtureError::PortDiscovery {
        service,
        container_port,
        diagnostics: redacted_tail(diagnostics),
        source: redacted_source(error),
    }
}

pub(crate) fn port_discovery_with_log_result(
    service: &'static str,
    container_port: u16,
    error: TestcontainersError,
    log_result: Result<String, FixtureError>,
) -> FixtureError {
    let diagnostics = match log_result {
        Ok(diagnostics) => diagnostics,
        Err(error) => redacted_tail(&format!(
            "could not read the {service} diagnostic log: {error}"
        )),
    };

    port_discovery_error(service, container_port, error, &diagnostics)
}

#[cfg_attr(test, allow(dead_code))]
/// A bounded, redacted tail of both of a container's output streams, labelled by service.
///
/// Both streams are read because Bitcoin Core and Electrs do not agree on which one carries a fatal
/// error, and a startup failure is usually explained by the last thing the container managed to say.
pub(crate) async fn container_log_tail(
    service: &'static str,
    container: &ContainerAsync<GenericImage>,
    deadline: &Deadline,
) -> Result<String, FixtureError> {
    let stdout = stream_tail(service, "stdout", container.stdout(false), deadline).await?;
    let stderr = stream_tail(service, "stderr", container.stderr(false), deadline).await?;

    Ok(redacted_tail(&format!("{stdout}\n{stderr}")))
}

/// Retains only the terminal [`ROLLING_LOG_BYTES`] of one stream, so a container that logged
/// gigabytes cannot be buffered in order to report the little that matters.
async fn stream_tail(
    service: &'static str,
    stream: &'static str,
    mut reader: impl tokio::io::AsyncRead + Unpin,
    deadline: &Deadline,
) -> Result<String, FixtureError> {
    let output = deadline
        .run(service, "reading the container diagnostic log", async {
            let mut tail = Vec::with_capacity(ROLLING_LOG_BYTES);
            let mut chunk = [0_u8; 4 * 1024];

            loop {
                let read = reader.read(&mut chunk).await?;
                if read == 0 {
                    break Ok::<Vec<u8>, io::Error>(tail);
                }
                append_log_tail(&mut tail, &chunk[..read]);
            }
        })
        .await?;

    Ok(match output {
        Ok(bytes) => format!("{service} {stream}:\n{}", String::from_utf8_lossy(&bytes)),
        Err(error) => format!("could not read {service} {stream}: {error}"),
    })
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
        bollard::errors::Error as BollardError,
        core::{
            IntoContainerPort,
            error::{ClientError, TestcontainersError, WaitContainerError},
        },
    };
    use tokio::sync::oneshot;

    use super::{
        AbandonOnDrop, CleanupTransport, MAX_IMAGE_DESCRIPTOR_BYTES, PartialCleanup,
        ROLLING_LOG_BYTES, abandon_owned_start, append_log_tail, attach_diagnostics,
        classify_start_error, cleanup_transport, container_start_join_error,
        partial_cleanup_diagnostic, port_discovery_error, port_discovery_with_log_result,
        resolve_docker_host, run_owned_start, run_owned_start_with_cleanup,
        start_failure_can_orphan,
    };
    use crate::{
        ContainerImage, FixtureError,
        deadline::Deadline,
        diagnostics::{MAX_DIAGNOSTIC_BYTES, redacted_tail},
    };

    /// The machinery is service-neutral, so the tests drive it under one stand-in name.
    const SERVICE: &str = "bitcoind";
    const PORT: u16 = 18_443;
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
    // Catches a regression that exposes an unbounded log body or fixture credentials in a port
    // discovery diagnostic.
    #[test]
    fn port_discovery_diagnostic_is_utf8_bounded_and_redacts_credentials() {
        let diagnostics = format!(
            "{} -rpcuser=admin1 -rpcpassword=123 admin1:123",
            "bitcoin-log-".repeat(2_000),
        );
        let error = port_discovery_error(
            SERVICE,
            PORT,
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
                classify_start_error(SERVICE, &ContainerImage::bitcoind_default(), error),
                FixtureError::RuntimeUnavailable { .. }
            ));
        }
    }
    // Catches a regression that turns ordinary Testcontainers startup failures into a runtime
    // availability error.
    #[test]
    fn non_runtime_start_errors_are_classified_as_container_start() {
        let error = classify_start_error(
            SERVICE,
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
            SERVICE,
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
            SERVICE,
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
            SERVICE,
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
            SERVICE,
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
    // Catches a regression that turns a failed owned startup task into an unclassified join
    // error or emits an unbounded task diagnostic.
    #[tokio::test]
    async fn owned_start_join_errors_become_bounded_container_start_errors() {
        let task = tokio::spawn(async { pending::<()>().await });
        task.abort();
        let join_error = task
            .await
            .expect_err("an aborted task must report a join error");

        let error =
            container_start_join_error(SERVICE, &ContainerImage::bitcoind_default(), join_error);
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
            SERVICE,
            PORT,
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
}
