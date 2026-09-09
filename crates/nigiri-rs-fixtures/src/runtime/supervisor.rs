use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
};

use futures_util::FutureExt;
use tokio::sync::{oneshot, watch};

use super::{
    engine::{ContainerEngine, EngineError, EngineResult},
    resources::{OwnedResource, ResourceLedger},
    spec::ContainerSpec,
};
use crate::deadline::Deadline;

pub(crate) struct RunningContainer {
    pub(crate) id: String,
    pub(crate) host: String,
    pub(crate) ports: BTreeMap<u16, u16>,
}

pub(crate) struct Startup<E: ContainerEngine> {
    engine: E,
    ledger: Arc<Mutex<ResourceLedger>>,
    mutations: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    cancelled: watch::Receiver<bool>,
    labels: HashMap<String, String>,
}

impl<E: ContainerEngine> Startup<E> {
    pub(crate) async fn create_network(&mut self, name: String) -> EngineResult<()> {
        self.ledger
            .lock()
            .expect("resource ledger is not poisoned")
            .expect_network(name.clone());
        let engine = self.engine.clone();
        let labels = self.labels.clone();
        let ledger = Arc::clone(&self.ledger);
        self.settle_mutation(async move {
            let id = engine.create_network(&name, labels).await?;
            ledger
                .lock()
                .expect("resource ledger is not poisoned")
                .confirm_network(&name, id);
            Ok(())
        })
        .await?;
        Ok(())
    }

    pub(crate) async fn start_container(
        &mut self,
        spec: ContainerSpec,
    ) -> EngineResult<RunningContainer> {
        self.expect_container(&spec.name);
        self.start_expected_container(spec).await
    }

    /// Starts two already-validated specifications concurrently while reserving their cleanup
    /// order deterministically. Alice is reserved before Bob, so reverse-order teardown always
    /// removes Bob first even if its engine calls happen to finish first.
    #[cfg(feature = "lnd")]
    pub(crate) async fn start_container_pair(
        &mut self,
        first: ContainerSpec,
        second: ContainerSpec,
    ) -> (
        EngineResult<RunningContainer>,
        EngineResult<RunningContainer>,
    ) {
        self.expect_container(&first.name);
        self.expect_container(&second.name);
        let mut first_startup = self.branch();
        let mut second_startup = self.branch();
        tokio::join!(
            first_startup.start_expected_container(first),
            second_startup.start_expected_container(second)
        )
    }

    fn expect_container(&mut self, name: &str) {
        self.ledger
            .lock()
            .expect("resource ledger is not poisoned")
            .expect_container(name.to_owned());
    }

    #[cfg(feature = "lnd")]
    fn branch(&self) -> Self {
        Self {
            engine: self.engine.clone(),
            ledger: Arc::clone(&self.ledger),
            mutations: Arc::clone(&self.mutations),
            cancelled: self.cancelled.clone(),
            labels: self.labels.clone(),
        }
    }

    async fn start_expected_container(
        &mut self,
        spec: ContainerSpec,
    ) -> EngineResult<RunningContainer> {
        let engine = self.engine.clone();
        self.run(engine.ensure_image(&spec)).await?;

        let engine = self.engine.clone();
        let labels = self.labels.clone();
        let ledger = Arc::clone(&self.ledger);
        let create_spec = spec.clone();
        let id = self
            .settle_mutation(async move {
                let id = engine.create_container(&create_spec, labels).await?;
                ledger
                    .lock()
                    .expect("resource ledger is not poisoned")
                    .confirm_container(&create_spec.name, id.clone());
                Ok(id)
            })
            .await?;

        let engine = self.engine.clone();
        self.run(engine.start_container(&id)).await?;

        let mut ports = BTreeMap::new();
        for container_port in &spec.exposed_ports {
            let engine = self.engine.clone();
            let mapped = self.run(engine.mapped_port(&id, *container_port)).await?;
            ports.insert(*container_port, mapped);
        }

        Ok(RunningContainer {
            id,
            host: self.engine.endpoint_host().to_owned(),
            ports,
        })
    }

    pub(crate) async fn logs(&mut self, id_or_name: &str) -> EngineResult<String> {
        let engine = self.engine.clone();
        self.run(engine.logs(id_or_name)).await
    }

    #[cfg(any(feature = "lnd", test))]
    pub(crate) async fn read_container_file(
        &mut self,
        id: &str,
        path: &str,
        max_bytes: usize,
    ) -> EngineResult<Vec<u8>> {
        let engine = self.engine.clone();
        let contents = self
            .run(engine.read_container_file(id, path, max_bytes))
            .await?;
        if contents.len() > max_bytes {
            return Err(EngineError::new(
                "read container file",
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "container file exceeds requested byte limit",
                ),
            ));
        }
        Ok(contents)
    }

    /// Bounds any composite-specific await by the supervisor's caller-cancellation signal.
    ///
    /// Runtime engine operations use the private flattening wrapper below. Protocol RPCs and
    /// readiness sleeps return their own result types, so composites use this generic layer and
    /// preserve those results unchanged.
    pub(crate) async fn run_until_cancelled<T>(
        &mut self,
        operation: impl Future<Output = T>,
    ) -> EngineResult<T> {
        if *self.cancelled.borrow() {
            return Err(cancelled_error());
        }

        tokio::select! {
            result = operation => Ok(result),
            changed = self.cancelled.changed() => {
                let _ = changed;
                Err(cancelled_error())
            }
        }
    }

    /// Docker may commit creation after the waiting caller disappears. Keep the request and its
    /// ledger confirmation alive on the owning runtime; teardown joins it before removing names.
    async fn settle_mutation<T: Send + 'static>(
        &mut self,
        operation: impl Future<Output = EngineResult<T>> + Send + 'static,
    ) -> EngineResult<T> {
        if *self.cancelled.borrow() {
            return Err(cancelled_error());
        }
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = sender.send(operation.await);
        });
        self.mutations
            .lock()
            .expect("mutation registry is not poisoned")
            .push(task);
        self.run_until_cancelled(receiver).await?.map_err(|_| {
            EngineError::new(
                "settle container engine mutation",
                std::io::Error::other("mutation task stopped"),
            )
        })?
    }

    async fn run<T>(
        &mut self,
        operation: impl Future<Output = EngineResult<T>>,
    ) -> EngineResult<T> {
        self.run_until_cancelled(operation).await?
    }
}

pub(crate) struct RuntimeHandle {
    shutdown: Option<oneshot::Sender<()>>,
    completion: Option<oneshot::Receiver<EngineResult<()>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RuntimeHandle {
    pub(crate) async fn shutdown(mut self) -> EngineResult<()> {
        self.signal_shutdown();
        let cleanup = self
            .completion
            .take()
            .expect("runtime completion is awaited once")
            .await
            .map_err(|_| {
                EngineError::new(
                    "wait for fixture cleanup",
                    std::io::Error::other("fixture supervisor stopped without reporting cleanup"),
                )
            })?;
        let joined = self.join_thread();
        cleanup.and(joined)
    }

    fn signal_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    fn join_thread(&mut self) -> EngineResult<()> {
        self.thread
            .take()
            .expect("fixture supervisor thread is joined once")
            .join()
            .map_err(|_| {
                EngineError::new(
                    "join fixture supervisor",
                    std::io::Error::other("fixture supervisor thread panicked"),
                )
            })
    }
}

impl Drop for RuntimeHandle {
    fn drop(&mut self) {
        self.signal_shutdown();
        if self.thread.is_some() {
            let _ = self.join_thread();
        }
    }
}

struct CallerCancellation {
    sender: Option<watch::Sender<bool>>,
    thread: Option<std::thread::JoinHandle<()>>,
    completed: Option<Receiver<()>>,
    wait: CancellationWait,
}

enum CancellationWait {
    Deadline(Deadline),
    Complete,
}

impl CallerCancellation {
    fn disarm(&mut self) -> std::thread::JoinHandle<()> {
        self.sender = None;
        self.thread
            .take()
            .expect("the supervisor thread is transferred once")
    }

    fn cancel_and_join(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(true);
        }
        self.join_within_deadline();
    }

    fn join_completed(&mut self) {
        self.sender = None;
        self.join_within_deadline();
    }

    fn join_within_deadline(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let completed = match &self.wait {
            CancellationWait::Complete => true,
            CancellationWait::Deadline(deadline) => {
                thread.is_finished()
                    || self.completed.as_ref().is_some_and(|completed| {
                        if deadline.remaining().is_zero() {
                            return false;
                        }
                        matches!(
                            completed.recv_timeout(deadline.remaining()),
                            Ok(()) | Err(RecvTimeoutError::Disconnected)
                        )
                    })
            }
        };
        self.completed = None;
        if completed {
            let _ = thread.join();
        }
    }
}

impl Drop for CallerCancellation {
    fn drop(&mut self) {
        self.cancel_and_join();
    }
}

struct ThreadCompletion(Option<SyncSender<()>>);

impl Drop for ThreadCompletion {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

struct PendingHandoff<T> {
    value: Arc<Mutex<Option<T>>>,
    acknowledged: Option<oneshot::Sender<()>>,
}

impl<T> PendingHandoff<T> {
    fn acknowledge(mut self) -> T {
        let value = take_handoff_value(&self.value)
            .expect("successful startup ownership is transferred once");
        if let Some(acknowledged) = self.acknowledged.take() {
            let _ = acknowledged.send(());
        }
        value
    }
}

fn take_handoff_value<T>(value: &Mutex<Option<T>>) -> Option<T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

pub(crate) struct CoordinatorCancellation {
    receiver: watch::Receiver<bool>,
}

impl CoordinatorCancellation {
    pub(crate) async fn cancelled(&mut self) {
        if *self.receiver.borrow() {
            return;
        }
        let _ = self.receiver.changed().await;
    }
}

pub(crate) async fn supervise<E, T, F, Fut, X>(
    engine: E,
    deadline: Deadline,
    work: F,
) -> Result<(T, RuntimeHandle), X>
where
    E: ContainerEngine,
    T: Send + 'static,
    F: FnOnce(Startup<E>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, X>> + Send + 'static,
    X: From<EngineError> + Send + 'static,
{
    supervise_with_wait(engine, CancellationWait::Deadline(deadline), work).await
}

/// A supervisor whose cancellation join is owned by an outer dependency coordinator.
///
/// Only the coordinator's caller guard is deadline-bounded. Once detached, the coordinator waits
/// for this supervisor to finish reverse-order cleanup before it starts dependency cleanup.
pub(crate) async fn supervise_for_coordinator<E, T, F, Fut, X>(
    engine: E,
    work: F,
) -> Result<(T, RuntimeHandle), X>
where
    E: ContainerEngine,
    T: Send + 'static,
    F: FnOnce(Startup<E>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, X>> + Send + 'static,
    X: From<EngineError> + Send + 'static,
{
    supervise_with_wait(engine, CancellationWait::Complete, work).await
}

async fn supervise_with_wait<E, T, F, Fut, X>(
    engine: E,
    wait: CancellationWait,
    work: F,
) -> Result<(T, RuntimeHandle), X>
where
    E: ContainerEngine,
    T: Send + 'static,
    F: FnOnce(Startup<E>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, X>> + Send + 'static,
    X: From<EngineError> + Send + 'static,
{
    let (cancel_sender, cancel_receiver) = watch::channel(false);
    let (result_sender, result_receiver) = oneshot::channel();
    let (completed_sender, completed_receiver) = sync_channel(1);

    let publish_before_cleanup = matches!(&wait, CancellationWait::Deadline(_));
    let thread = std::thread::Builder::new()
        .name("nigiri-rs-fixture".to_owned())
        .spawn(move || {
            let _completion = ThreadCompletion(Some(completed_sender));
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = result_sender.send(Err(EngineError::new(
                        "start fixture supervisor",
                        error,
                    )
                    .into()));
                    return;
                }
            };

            runtime.block_on(async move {
                let ledger = Arc::new(Mutex::new(ResourceLedger::default()));
                let mutations = Arc::new(Mutex::new(Vec::new()));
                let startup = Startup {
                    engine: engine.clone(),
                    ledger: Arc::clone(&ledger),
                    mutations: Arc::clone(&mutations),
                    cancelled: cancel_receiver,
                    labels: HashMap::from([(
                        "nigiri-rs.fixture".to_owned(),
                        uuid::Uuid::new_v4().simple().to_string(),
                    )]),
                };

                let mut work_cancelled = startup.cancelled.clone();
                let outcome = tokio::select! {
                    outcome = AssertUnwindSafe(work(startup)).catch_unwind() => outcome,
                    changed = work_cancelled.changed() => {
                        let _ = changed;
                        Ok(Err(X::from(cancelled_error())))
                    }
                };

                match outcome {
                    Ok(Ok(value)) => {
                        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
                        let (completion_sender, completion_receiver) = oneshot::channel();
                        let handle = RuntimeHandle {
                            shutdown: Some(shutdown_sender),
                            completion: Some(completion_receiver),
                            thread: None,
                        };
                        if result_sender.send(Ok((value, handle))).is_ok() {
                            let _ = shutdown_receiver.await;
                        }
                        let _ = completion_sender.send(cleanup(&engine, &ledger, &mutations).await);
                    }
                    Ok(Err(error)) => {
                        if publish_before_cleanup {
                            let _ = result_sender.send(Err(error));
                            let _ = cleanup(&engine, &ledger, &mutations).await;
                        } else {
                            // A dependency coordinator must not tear down its network before us.
                            let _ = cleanup(&engine, &ledger, &mutations).await;
                            let _ = result_sender.send(Err(error));
                        }
                    }
                    Err(payload) => {
                        let _ = cleanup(&engine, &ledger, &mutations).await;
                        let message = payload
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                            .unwrap_or("fixture startup panicked");
                        let _ = result_sender.send(Err(EngineError::new(
                            "run fixture startup",
                            std::io::Error::other(message.to_owned()),
                        )
                        .into()));
                    }
                }
            });
        })
        .map_err(|error| X::from(EngineError::new("spawn fixture supervisor", error)))?;
    let mut cancellation = CallerCancellation {
        sender: Some(cancel_sender),
        thread: Some(thread),
        completed: Some(completed_receiver),
        wait,
    };

    let result = match result_receiver.await {
        Ok(result) => result,
        Err(_) => {
            cancellation.cancel_and_join();
            return Err(X::from(EngineError::new(
                "wait for fixture supervisor",
                std::io::Error::other("fixture supervisor stopped before startup completed"),
            )));
        }
    };
    match result {
        Ok((value, mut handle)) => {
            handle.thread = Some(cancellation.disarm());
            Ok((value, handle))
        }
        Err(error) => {
            if matches!(&cancellation.wait, CancellationWait::Deadline(_)) {
                // The supervisor retains the ledger and owns all teardown after error delivery.
                drop(cancellation.disarm());
            } else {
                cancellation.join_completed();
            }
            Err(error)
        }
    }
}

/// Runs dependency-aware startup ownership on one dedicated thread.
///
/// Dropping the caller signals cancellation and waits only through the shared deadline. The work
/// itself remains on the coordinator thread after detachment and is responsible for sequencing
/// dependent cleanup before returning.
pub(crate) async fn coordinate_startup<T, F, Fut, X, H>(
    deadline: Deadline,
    work: F,
    before_ack: H,
) -> Result<T, X>
where
    T: Send + 'static,
    F: FnOnce(CoordinatorCancellation) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, X>> + 'static,
    X: From<EngineError> + Send + 'static,
    H: Future<Output = ()>,
{
    let (cancel_sender, cancel_receiver) = watch::channel(false);
    let (result_sender, result_receiver) = oneshot::channel();
    let (completed_sender, completed_receiver) = sync_channel(1);

    let thread = std::thread::Builder::new()
        .name("nigiri-rs-composite".to_owned())
        .spawn(move || {
            let _completion = ThreadCompletion(Some(completed_sender));
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = result_sender.send(Err(EngineError::new(
                        "start composite coordinator",
                        error,
                    )
                    .into()));
                    return;
                }
            };

            let handoff_cancellation = cancel_receiver.clone();
            let outcome = runtime.block_on(
                AssertUnwindSafe(work(CoordinatorCancellation {
                    receiver: cancel_receiver,
                }))
                .catch_unwind(),
            );
            let result = match outcome {
                Ok(Ok(value)) => {
                    let value = Arc::new(Mutex::new(Some(value)));
                    let coordinator_value = Arc::clone(&value);
                    let (acknowledged_sender, acknowledged_receiver) = oneshot::channel();
                    let handoff = PendingHandoff {
                        value,
                        acknowledged: Some(acknowledged_sender),
                    };
                    if result_sender.send(Ok(handoff)).is_err() {
                        drop(take_handoff_value(&coordinator_value));
                        return;
                    }

                    runtime.block_on(async move {
                        let mut cancelled = handoff_cancellation;
                        tokio::select! {
                            biased;
                            acknowledged = acknowledged_receiver => {
                                if acknowledged.is_err() {
                                    drop(take_handoff_value(&coordinator_value));
                                }
                            }
                            changed = cancelled.changed() => {
                                let _ = changed;
                                drop(take_handoff_value(&coordinator_value));
                            }
                        }
                    });
                    return;
                }
                Ok(Err(error)) => Err(error),
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("composite startup panicked");
                    Err(EngineError::new(
                        "run composite startup",
                        std::io::Error::other(message.to_owned()),
                    )
                    .into())
                }
            };
            let _ = result_sender.send(result);
        })
        .map_err(|error| X::from(EngineError::new("spawn composite coordinator", error)))?;
    let mut cancellation = CallerCancellation {
        sender: Some(cancel_sender),
        thread: Some(thread),
        completed: Some(completed_receiver),
        wait: CancellationWait::Deadline(deadline),
    };

    let result = match result_receiver.await {
        Ok(result) => result,
        Err(_) => {
            cancellation.cancel_and_join();
            return Err(X::from(EngineError::new(
                "wait for composite coordinator",
                std::io::Error::other("composite coordinator stopped before startup completed"),
            )));
        }
    };
    let result = match result {
        Ok(handoff) => {
            before_ack.await;
            Ok(handoff.acknowledge())
        }
        Err(error) => Err(error),
    };
    cancellation.join_completed();
    result
}

async fn cleanup<E: ContainerEngine>(
    engine: &E,
    ledger: &Arc<Mutex<ResourceLedger>>,
    mutations: &Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
) -> EngineResult<()> {
    let pending =
        std::mem::take(&mut *mutations.lock().expect("mutation registry is not poisoned"));
    for mutation in pending {
        let _ = mutation.await;
    }

    let resources = ledger
        .lock()
        .expect("resource ledger is not poisoned")
        .take_cleanup_order();
    let mut first_error = None;

    for resource in resources {
        let result = match resource {
            OwnedResource::Container { name, id } => {
                engine
                    .remove_container(id.as_deref().unwrap_or(&name))
                    .await
            }
            OwnedResource::Network { name, id } => {
                engine.remove_network(id.as_deref().unwrap_or(&name)).await
            }
        };
        if first_error.is_none() {
            first_error = result.err();
        }
    }

    first_error.map_or(Ok(()), Err)
}

pub(super) fn cancelled_error() -> EngineError {
    EngineError::new(
        "start fixture",
        std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "fixture startup was cancelled",
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use nigiri_rs_core::Bitcoin;
    use tokio::sync::Notify;

    use super::supervise;
    use crate::{
        ContainerImage,
        deadline::Deadline,
        runtime::{
            engine::{ContainerEngine, EngineError, EngineResult},
            spec::node_spec,
        },
    };

    fn test_deadline() -> Deadline {
        Deadline::new(Duration::from_secs(5)).expect("the test supervisor has a bounded deadline")
    }

    #[derive(Clone, Default)]
    struct FakeEngine {
        block_create: bool,
        block_network: bool,
        create_release: Arc<Notify>,
        committed: Arc<Mutex<Vec<String>>>,
        commit_finished: Arc<Notify>,
        block_remove: bool,
        remove_release: Arc<Notify>,
        block_read: bool,
        create_entered: Arc<Notify>,
        read_entered: Arc<Notify>,
        read_contents: Arc<Mutex<Vec<u8>>>,
        read_requests: Arc<Mutex<Vec<(String, String, usize)>>>,
        removed: Arc<Mutex<Vec<String>>>,
    }

    impl FakeEngine {
        /// The server owns this commit independently of the client's request future. Dropping
        /// that future cannot cancel creation or prevent the server from publishing its resource.
        async fn delayed_commit(&self, id: String) -> EngineResult<String> {
            let release = Arc::clone(&self.create_release);
            let committed = Arc::clone(&self.committed);
            let finished = Arc::clone(&self.commit_finished);
            let (sender, receiver) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                runtime.block_on(release.notified());
                committed.lock().unwrap().push(id.clone());
                finished.notify_one();
                let _ = sender.send(id);
            });
            self.create_entered.notify_one();
            Ok(receiver.await.unwrap())
        }
    }

    impl ContainerEngine for FakeEngine {
        fn endpoint_host(&self) -> &str {
            "127.0.0.1"
        }

        async fn create_network(
            &self,
            name: &str,
            _labels: HashMap<String, String>,
        ) -> EngineResult<String> {
            if self.block_network {
                self.delayed_commit(format!("{name}-id")).await
            } else {
                Ok(format!("{name}-id"))
            }
        }

        async fn ensure_image(
            &self,
            _spec: &crate::runtime::spec::ContainerSpec,
        ) -> EngineResult<()> {
            Ok::<(), EngineError>(())
        }

        async fn create_container(
            &self,
            spec: &crate::runtime::spec::ContainerSpec,
            _labels: HashMap<String, String>,
        ) -> EngineResult<String> {
            if self.block_create {
                return self.delayed_commit(format!("{}-id", spec.name)).await;
            }
            let id = format!("{}-id", spec.name);
            self.committed.lock().unwrap().push(id.clone());
            Ok(id)
        }

        async fn start_container(&self, _id: &str) -> EngineResult<()> {
            Ok(())
        }
        async fn mapped_port(&self, _id: &str, container_port: u16) -> EngineResult<u16> {
            Ok(container_port)
        }
        async fn logs(&self, _id: &str) -> EngineResult<String> {
            Ok(String::new())
        }

        async fn read_container_file(
            &self,
            id: &str,
            path: &str,
            max_bytes: usize,
        ) -> EngineResult<Vec<u8>> {
            self.read_requests
                .lock()
                .expect("read request log is not poisoned")
                .push((id.to_owned(), path.to_owned(), max_bytes));
            self.read_entered.notify_waiters();
            if self.block_read {
                return std::future::pending().await;
            }
            Ok(self
                .read_contents
                .lock()
                .expect("read contents are not poisoned")
                .clone())
        }

        async fn remove_container(&self, id_or_name: &str) -> EngineResult<()> {
            self.committed.lock().unwrap().retain(|id| id != id_or_name);
            self.removed
                .lock()
                .expect("removal log is not poisoned")
                .push(id_or_name.to_owned());
            Ok(())
        }

        async fn remove_network(&self, id_or_name: &str) -> EngineResult<()> {
            if self.block_remove {
                self.remove_release.notified().await;
            }
            self.committed.lock().unwrap().retain(|id| id != id_or_name);
            self.removed
                .lock()
                .expect("removal log is not poisoned")
                .push(id_or_name.to_owned());
            Ok(())
        }
    }

    // Catches a regression that bypasses the runtime engine, changes the requested credential
    // path/bound, or returns bytes from a fake engine that violated the bound contract.
    #[tokio::test]
    async fn startup_file_reads_delegate_exactly_and_enforce_the_byte_bound() {
        const CERT_LIMIT: usize = 1_048_576;
        const CERT_PATH: &str = "/root/.lnd/tls.cert";

        let engine = FakeEngine {
            read_contents: Arc::new(Mutex::new(b"certificate".to_vec())),
            ..FakeEngine::default()
        };
        let observed = engine.clone();
        let (contents, runtime) = supervise(engine, test_deadline(), |mut startup| async move {
            startup
                .read_container_file("alice", CERT_PATH, CERT_LIMIT)
                .await
        })
        .await
        .expect("a bounded fake-engine read succeeds");

        assert_eq!(contents, b"certificate");
        assert_eq!(
            *observed
                .read_requests
                .lock()
                .expect("read request log is not poisoned"),
            [("alice".to_owned(), CERT_PATH.to_owned(), CERT_LIMIT)]
        );
        runtime.shutdown().await.expect("cleanup succeeds");

        let oversized = FakeEngine {
            read_contents: Arc::new(Mutex::new(vec![b'x'; CERT_LIMIT + 1])),
            ..FakeEngine::default()
        };
        let error = match supervise(oversized, test_deadline(), |mut startup| async move {
            startup
                .read_container_file("alice", CERT_PATH, CERT_LIMIT)
                .await
        })
        .await
        {
            Ok(_) => panic!("1,048,577 bytes must be rejected at the supervisor boundary"),
            Err(error) => error,
        };

        assert_eq!(error.operation(), "read container file");
        assert!(!error.to_string().contains(&"x".repeat(32)));
    }

    // Catches a read wrapper that awaits the engine directly instead of using Startup's
    // cancellation gate, which would strand fixture resources when its caller goes away.
    #[tokio::test]
    async fn cancelling_a_container_file_read_preserves_supervisor_cleanup() {
        let engine = FakeEngine {
            block_read: true,
            ..FakeEngine::default()
        };
        let observed = engine.clone();
        let entered = engine.read_entered.clone();

        let caller = tokio::spawn(supervise(
            engine,
            test_deadline(),
            |mut startup| async move {
                startup.create_network("fixture-network".to_owned()).await?;
                startup
                    .read_container_file("alice", "/root/.lnd/tls.cert", 1_048_576)
                    .await
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("the container file read must begin");
        caller.abort();

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !observed
                    .removed
                    .lock()
                    .expect("removal log is not poisoned")
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the supervisor must clean up after file-read cancellation");

        assert_eq!(
            *observed
                .removed
                .lock()
                .expect("removal log is not poisoned"),
            ["fixture-network-id"]
        );
    }

    #[tokio::test]
    async fn cancellation_settles_late_create_commit_before_cleanup() {
        for network in [false, true] {
            let engine = FakeEngine {
                block_create: !network,
                block_network: network,
                ..FakeEngine::default()
            };
            let observed = engine.clone();
            let entered = engine.create_entered.clone();
            let spec = node_spec::<Bitcoin>(
                ContainerImage::bitcoind_default(),
                "fixture-network".to_owned(),
                "bitcoin-node".to_owned(),
                Vec::new(),
            )
            .unwrap();
            let caller = tokio::spawn(supervise(
                engine,
                Deadline::new(Duration::from_millis(50)).unwrap(),
                move |mut startup| async move {
                    if network {
                        startup.create_network("fixture-network".into()).await
                    } else {
                        startup.start_container(spec).await.map(|_| ())
                    }
                },
            ));
            tokio::time::timeout(Duration::from_secs(1), entered.notified())
                .await
                .unwrap();
            caller.abort();
            assert!(
                tokio::time::timeout(Duration::from_secs(1), caller)
                    .await
                    .expect("public cancellation remains bounded")
                    .is_err()
            );
            let premature_cleanup = !observed.removed.lock().unwrap().is_empty();
            observed.create_release.notify_one();
            tokio::time::timeout(Duration::from_secs(1), observed.commit_finished.notified())
                .await
                .expect("the external server commits even after caller cancellation");
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if !observed.removed.lock().unwrap().is_empty()
                        && observed.committed.lock().unwrap().is_empty()
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("every externally committed resource must eventually be removed");
            assert!(
                !premature_cleanup,
                "cleanup cannot race an unsettled create"
            );
            assert_eq!(
                *observed.removed.lock().unwrap(),
                [if network {
                    "fixture-network-id"
                } else {
                    "bitcoin-node-id"
                }]
            );
        }
    }

    #[tokio::test]
    async fn concrete_failure_is_delivered_before_blocked_cleanup() {
        let engine = FakeEngine {
            block_remove: true,
            ..FakeEngine::default()
        };
        let observed = engine.clone();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            supervise(
                engine,
                Deadline::new(Duration::from_secs(2)).unwrap(),
                |mut startup| async move {
                    startup.create_network("fixture-network".into()).await?;
                    Err::<(), _>(EngineError::new(
                        "concrete startup failure",
                        std::io::Error::other("node rejected configuration"),
                    ))
                },
            ),
        )
        .await;
        observed.remove_release.notify_one();
        let error = match result {
            Ok(Err(error)) => error,
            _ => panic!("cleanup hid the original error"),
        };
        assert_eq!(error.operation(), "concrete startup failure");
        tokio::time::timeout(Duration::from_secs(1), async {
            while observed.removed.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background supervisor retains cleanup ownership");
    }

    #[tokio::test]
    async fn explicit_shutdown_waits_for_reverse_order_cleanup() {
        let engine = FakeEngine::default();
        let observed = engine.clone();
        let bitcoin = node_spec::<Bitcoin>(
            ContainerImage::bitcoind_default(),
            "fixture-network".to_owned(),
            "bitcoin-node".to_owned(),
            Vec::new(),
        )
        .expect("the pinned Bitcoin specification is valid");
        let second = node_spec::<Bitcoin>(
            ContainerImage::bitcoind_default(),
            "fixture-network".to_owned(),
            "second-node".to_owned(),
            Vec::new(),
        )
        .expect("the second Bitcoin specification is valid");

        let (_, runtime) = supervise(engine, test_deadline(), move |mut startup| async move {
            startup.create_network("fixture-network".to_owned()).await?;
            startup.start_container(bitcoin).await?;
            startup.start_container(second).await?;
            Ok::<(), EngineError>(())
        })
        .await
        .expect("the fake topology starts");

        runtime.shutdown().await.expect("cleanup succeeds");
        assert_eq!(
            *observed
                .removed
                .lock()
                .expect("removal log is not poisoned"),
            ["second-node-id", "bitcoin-node-id", "fixture-network-id"]
        );
    }

    #[tokio::test]
    async fn dropping_the_runtime_waits_for_cleanup() {
        let engine = FakeEngine::default();
        let observed = engine.clone();

        let (_, runtime) = supervise(engine, test_deadline(), move |mut startup| async move {
            startup.create_network("fixture-network".to_owned()).await?;
            Ok::<(), EngineError>(())
        })
        .await
        .expect("the fake topology starts");

        drop(runtime);

        assert_eq!(
            *observed
                .removed
                .lock()
                .expect("removal log is not poisoned"),
            ["fixture-network-id"]
        );
    }

    #[tokio::test]
    async fn panicking_startup_still_cleans_up_recorded_resources() {
        let engine = FakeEngine::default();
        let observed = engine.clone();

        let result = supervise(engine, test_deadline(), move |mut startup| async move {
            startup.create_network("fixture-network".to_owned()).await?;
            panic!("simulated startup panic");
            #[allow(unreachable_code)]
            Ok::<(), EngineError>(())
        })
        .await;

        assert!(result.is_err(), "a supervisor panic must become an error");
        assert_eq!(
            *observed
                .removed
                .lock()
                .expect("removal log is not poisoned"),
            ["fixture-network-id"]
        );
    }
}
