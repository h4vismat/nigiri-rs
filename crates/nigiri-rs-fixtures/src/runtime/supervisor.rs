use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};

use futures_util::FutureExt;
use tokio::sync::{oneshot, watch};

use super::{
    engine::{ContainerEngine, EngineError, EngineResult},
    resources::{OwnedResource, ResourceLedger},
    spec::ContainerSpec,
};

pub(crate) struct RunningContainer {
    pub(crate) id: String,
    pub(crate) host: String,
    pub(crate) ports: BTreeMap<u16, u16>,
}

pub(crate) struct Startup<E: ContainerEngine> {
    engine: E,
    ledger: Arc<Mutex<ResourceLedger>>,
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
        let id = self.run(engine.create_network(&name, labels)).await?;
        self.ledger
            .lock()
            .expect("resource ledger is not poisoned")
            .confirm_network(&name, id);
        Ok(())
    }

    pub(crate) async fn start_container(
        &mut self,
        spec: ContainerSpec,
    ) -> EngineResult<RunningContainer> {
        let engine = self.engine.clone();
        self.run(engine.ensure_image(&spec)).await?;

        self.ledger
            .lock()
            .expect("resource ledger is not poisoned")
            .expect_container(spec.name.clone());
        let engine = self.engine.clone();
        let labels = self.labels.clone();
        let id = self.run(engine.create_container(&spec, labels)).await?;
        self.ledger
            .lock()
            .expect("resource ledger is not poisoned")
            .confirm_container(&spec.name, id.clone());

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

    async fn run<T>(
        &mut self,
        operation: impl Future<Output = EngineResult<T>>,
    ) -> EngineResult<T> {
        if *self.cancelled.borrow() {
            return Err(cancelled_error());
        }

        tokio::select! {
            result = operation => result,
            changed = self.cancelled.changed() => {
                let _ = changed;
                Err(cancelled_error())
            }
        }
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
}

impl CallerCancellation {
    fn disarm(&mut self) {
        self.sender = None;
    }
}

impl Drop for CallerCancellation {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(true);
        }
    }
}

pub(crate) async fn supervise<E, T, F, Fut, X>(engine: E, work: F) -> Result<(T, RuntimeHandle), X>
where
    E: ContainerEngine,
    T: Send + 'static,
    F: FnOnce(Startup<E>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, X>> + Send + 'static,
    X: From<EngineError> + Send + 'static,
{
    let (cancel_sender, cancel_receiver) = watch::channel(false);
    let mut cancellation = CallerCancellation {
        sender: Some(cancel_sender),
    };
    let (result_sender, result_receiver) = oneshot::channel();

    let thread = std::thread::Builder::new()
        .name("nigiri-rs-fixture".to_owned())
        .spawn(move || {
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
                let startup = Startup {
                    engine: engine.clone(),
                    ledger: Arc::clone(&ledger),
                    cancelled: cancel_receiver,
                    labels: HashMap::from([(
                        "nigiri-rs.fixture".to_owned(),
                        uuid::Uuid::new_v4().simple().to_string(),
                    )]),
                };

                match AssertUnwindSafe(work(startup)).catch_unwind().await {
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
                        let _ = completion_sender.send(cleanup(&engine, &ledger).await);
                    }
                    Ok(Err(error)) => {
                        let _ = cleanup(&engine, &ledger).await;
                        let _ = result_sender.send(Err(error));
                    }
                    Err(payload) => {
                        let _ = cleanup(&engine, &ledger).await;
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

    let result = match result_receiver.await {
        Ok(result) => result,
        Err(_) => {
            let _ = thread.join();
            return Err(X::from(EngineError::new(
                "wait for fixture supervisor",
                std::io::Error::other("fixture supervisor stopped before startup completed"),
            )));
        }
    };
    match result {
        Ok((value, mut handle)) => {
            cancellation.disarm();
            handle.thread = Some(thread);
            Ok((value, handle))
        }
        Err(error) => {
            let _ = thread.join();
            Err(error)
        }
    }
}

async fn cleanup<E: ContainerEngine>(
    engine: &E,
    ledger: &Arc<Mutex<ResourceLedger>>,
) -> EngineResult<()> {
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

fn cancelled_error() -> EngineError {
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
        runtime::{
            engine::{ContainerEngine, EngineError, EngineResult},
            spec::node_spec,
        },
    };

    #[derive(Clone, Default)]
    struct FakeEngine {
        block_create: bool,
        create_entered: Arc<Notify>,
        removed: Arc<Mutex<Vec<String>>>,
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
            Ok(format!("{name}-id"))
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
            self.create_entered.notify_waiters();
            if self.block_create {
                std::future::pending().await
            } else {
                Ok(format!("{}-id", spec.name))
            }
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

        async fn remove_container(&self, id_or_name: &str) -> EngineResult<()> {
            self.removed
                .lock()
                .expect("removal log is not poisoned")
                .push(id_or_name.to_owned());
            Ok(())
        }

        async fn remove_network(&self, id_or_name: &str) -> EngineResult<()> {
            self.removed
                .lock()
                .expect("removal log is not poisoned")
                .push(id_or_name.to_owned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancelling_the_caller_removes_a_container_whose_create_never_answered() {
        let engine = FakeEngine {
            block_create: true,
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
        .expect("the pinned Bitcoin specification is valid");

        let caller = tokio::spawn(supervise(engine, move |mut startup| async move {
            startup.start_container(spec).await
        }));
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("container creation must begin");
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
        .expect("the supervisor must clean up after caller cancellation");

        assert_eq!(
            *observed
                .removed
                .lock()
                .expect("removal log is not poisoned"),
            ["bitcoin-node"]
        );
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

        let (_, runtime) = supervise(engine, move |mut startup| async move {
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

        let (_, runtime) = supervise(engine, move |mut startup| async move {
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

        let result = supervise(engine, move |mut startup| async move {
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
