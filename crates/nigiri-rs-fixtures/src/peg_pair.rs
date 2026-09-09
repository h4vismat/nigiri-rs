//! A Bitcoin and Liquid pair wired for Liquid's peg: four containers, one network.

use std::{fmt, future::Future, time::Duration};

use nigiri_rs_core::{Bitcoin, Liquid, NigiriClient, Peg};

use crate::{
    ContainerImage, Fixture, FixtureError, RPC_PASSWORD, RPC_USER,
    chain::FixtureChain,
    deadline::Deadline,
    fixture::FixtureStartupOwner,
    runtime::{CoordinatorCancellation, cancelled_startup_error, coordinate_startup},
};

/// Four containers rather than two, so twice the standalone fixture's budget.
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

/// Names the pairing step in a readiness timeout.
const PEG_SERVICE: &str = "peg";

/// A Bitcoin and Liquid stack wired for Liquid's peg, with a verified [`Peg`] across them.
///
/// Four containers on one Docker network: `bitcoind` with its Electrs, and `elementsd` with its
/// Electrs. The Elements node runs `-validatepegin=1` and reaches `bitcoind` over `-mainchainrpc*`
/// by container name, which is what lets a real `claimpegin` validate against a real deposit.
///
/// ```no_run
/// use bitcoin::Amount;
/// use nigiri_rs_fixtures::PegPair;
///
/// # async fn example() -> Result<(), nigiri_rs_fixtures::FixtureError> {
/// let pair = PegPair::start().await?;
/// let pegged = pair.peg().complete_peg_in(Amount::from_sat(100_000)).await?;
/// println!("minted by {}", pegged.claim_txid);
/// # Ok(())
/// # }
/// ```
///
/// # Peg-in is real, peg-out is half real
///
/// [`Peg::release_peg_out`] pays the destination from the Bitcoin node's own wallet, not from a
/// locked reserve, because regtest has no functionaries. Total BTC on the mainchain side grows with
/// every release and no 1:1 invariant holds across the pair. The Liquid half stays honest —
/// `sendtomainchain` genuinely burns. See the [`Peg`] documentation before asserting on supply.
///
/// # Lifetime
///
/// Dropping the pair requests best-effort cleanup of all four containers, their anonymous volumes,
/// and the shared network. Use [`PegPair::shutdown`] when cleanup errors matter. The Liquid stack is
/// released first: `elementsd` holds an RPC connection to `bitcoind` and must not outlive it. A hard
/// process kill can still leave resources for manual removal.
pub struct PegPair {
    handles: PegHandles<Fixture<Liquid>, Fixture<Bitcoin>>,
    peg: Peg,
}

/// The pair's two inner stacks, held for their `Drop`.
///
/// Declaration order is teardown order, and that is why this is its own type rather than two
/// adjacent fields nothing checks: Rust drops fields in declaration order, so the whole Liquid
/// stack goes before the Bitcoin node it validates against. Generic over both so a test can drop
/// this shape with recorders in place of containers.
struct PegHandles<LiquidStack, BitcoinStack> {
    liquid: LiquidStack,
    bitcoin: BitcoinStack,
}

// Written by hand rather than derived, exactly as for `Fixture`, but the field that forces it is
// different: the two held stacks print safely because `Fixture`'s own `Debug` is hand-written and
// redacting. `peg` is the one that must stay out — `nigiri_rs_core::Peg` derives `Debug` over a
// config whose `node_rpc_password` is public.
impl fmt::Debug for PegPair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PegPair")
            .field("bitcoin", &self.handles.bitcoin)
            .field("liquid", &self.handles.liquid)
            .finish_non_exhaustive()
    }
}

impl PegPair {
    /// A builder carrying the four pinned images and the 120-second startup budget.
    #[must_use]
    pub fn builder() -> PegPairBuilder {
        PegPairBuilder {
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            bitcoind_image: Bitcoin::node_image_default(),
            bitcoin_electrs_image: Bitcoin::electrs_image_default(),
            elements_image: Liquid::node_image_default(),
            liquid_electrs_image: Liquid::electrs_image_default(),
        }
    }

    /// Starts the wired pair with the pinned defaults.
    pub async fn start() -> Result<Self, FixtureError> {
        Self::builder().start().await
    }

    /// Removes both stacks in dependency order and waits for cleanup to finish.
    pub async fn shutdown(self) -> Result<(), FixtureError> {
        let Self {
            handles: PegHandles { liquid, bitcoin },
            peg: _,
        } = self;

        let liquid_result = liquid.shutdown().await;
        let bitcoin_result = bitcoin.shutdown().await;
        liquid_result.and(bitcoin_result)
    }

    /// The Bitcoin side's client, pointed at `bitcoind` and its Electrs.
    #[must_use]
    pub fn bitcoin(&self) -> &NigiriClient<Bitcoin> {
        self.handles.bitcoin.client()
    }

    /// The Liquid side's client, pointed at `elementsd` and its Electrs.
    #[must_use]
    pub fn liquid(&self) -> &NigiriClient<Liquid> {
        self.handles.liquid.client()
    }

    /// The peg between them, already verified by [`Peg::connect`].
    #[must_use]
    pub fn peg(&self) -> &Peg {
        &self.peg
    }
}

/// The Elements arguments that turn a standalone Liquid node into the sidechain half of a pair.
///
/// `-validatepegin=1` replaces the `0` the standalone chain sets; see `node::merge_node_args` for
/// why replacing rather than appending matters. The port and credentials are read from the crate's
/// own constants, so changing either cannot leave the pair pointed at a door that moved.
fn peg_node_args(bitcoin_container: &str) -> Vec<String> {
    vec![
        "-validatepegin=1".to_owned(),
        format!("-mainchainrpchost={bitcoin_container}"),
        format!("-mainchainrpcport={}", Bitcoin::NODE_RPC_PORT),
        format!("-mainchainrpcuser={RPC_USER}"),
        format!("-mainchainrpcpassword={RPC_PASSWORD}"),
    ]
}

/// Overrides for a [`PegPair`]'s four images and its startup budget.
#[derive(Clone, Debug)]
pub struct PegPairBuilder {
    startup_timeout: Duration,
    bitcoind_image: ContainerImage,
    bitcoin_electrs_image: ContainerImage,
    elements_image: ContainerImage,
    liquid_electrs_image: ContainerImage,
}

impl PegPairBuilder {
    /// Overrides the budget for the whole four-container startup, not for any step within it.
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
    pub fn elements_image(mut self, image: ContainerImage) -> Self {
        self.elements_image = image;
        self
    }

    #[must_use]
    pub fn liquid_electrs_image(mut self, image: ContainerImage) -> Self {
        self.liquid_electrs_image = image;
        self
    }

    /// Starts `bitcoind`, then `elementsd` wired to it, then verifies the pair.
    ///
    /// One `Deadline` covers all four containers and the pairing check, so a slow phase spends
    /// budget the later phases no longer have.
    ///
    /// The Bitcoin half comes up completely first, and not for tidiness: `elementsd` reads
    /// `-mainchainrpc*` while starting, so the node it points at has to be answering RPC by then.
    pub async fn start(self) -> Result<PegPair, FixtureError> {
        // Every image is validated before the first container starts. The inner builders validate
        // their own, but the Bitcoin half runs to completion first, so an unusable Elements image
        // would otherwise be rejected only after two containers were already up.
        for image in [
            &self.bitcoind_image,
            &self.bitcoin_electrs_image,
            &self.elements_image,
            &self.liquid_electrs_image,
        ] {
            image.validate()?;
        }

        let deadline = Deadline::new(self.startup_timeout)?;

        let started = self
            .start_with_environment(RealPegEnvironment, deadline)
            .await?;
        Ok(PegPair {
            handles: started.handles,
            peg: started.peg,
        })
    }

    async fn start_with_environment<E: PegEnvironment>(
        self,
        environment: E,
        deadline: Deadline,
    ) -> Result<StartedPegPair<E>, FixtureError> {
        let work_deadline = deadline.clone();
        let coordinated = coordinate_startup(
            deadline.clone(),
            move |mut cancellation| async move {
                start_peg_stacks(self, environment, work_deadline, &mut cancellation).await
            },
            std::future::ready(()),
        );
        deadline
            .run(
                PEG_SERVICE,
                "coordinating complete peg startup",
                coordinated,
            )
            .await?
    }
}

struct StartedPegPair<E: PegEnvironment> {
    handles: PegHandles<E::LiquidStack, E::BitcoinStack>,
    peg: E::ConnectedPeg,
}

/// Lifecycle boundary used to exercise real coordinator ownership without running Docker.
trait PegEnvironment: Send + 'static {
    type BitcoinStack: Send + 'static;
    type LiquidStack: Send + 'static;
    type ConnectedPeg: Send + 'static;
    fn start_bitcoin(
        &self,
        builder: &PegPairBuilder,
        deadline: &Deadline,
        owner: FixtureStartupOwner,
    ) -> impl Future<Output = Result<Self::BitcoinStack, FixtureError>>;
    fn start_liquid(
        &self,
        builder: &PegPairBuilder,
        bitcoin: &Self::BitcoinStack,
        deadline: &Deadline,
        owner: FixtureStartupOwner,
    ) -> impl Future<Output = Result<Self::LiquidStack, FixtureError>>;
    fn connect(
        &self,
        bitcoin: &Self::BitcoinStack,
        liquid: &Self::LiquidStack,
    ) -> impl Future<Output = Result<Self::ConnectedPeg, FixtureError>>;
    fn attach_bitcoin_logs(
        &self,
        bitcoin: &Self::BitcoinStack,
        deadline: &Deadline,
        error: FixtureError,
    ) -> impl Future<Output = FixtureError>;
    fn shutdown_bitcoin(&self, bitcoin: Self::BitcoinStack) -> impl Future<Output = ()>;
}

struct RealPegEnvironment;
impl PegEnvironment for RealPegEnvironment {
    type BitcoinStack = Fixture<Bitcoin>;
    type LiquidStack = Fixture<Liquid>;
    type ConnectedPeg = Peg;
    async fn start_bitcoin(
        &self,
        builder: &PegPairBuilder,
        deadline: &Deadline,
        owner: FixtureStartupOwner,
    ) -> Result<Self::BitcoinStack, FixtureError> {
        Fixture::<Bitcoin>::builder()
            .node_image(builder.bitcoind_image.clone())
            .electrs_image(builder.bitcoin_electrs_image.clone())
            .start_under_with_owner(deadline, owner)
            .await
    }
    async fn start_liquid(
        &self,
        builder: &PegPairBuilder,
        bitcoin: &Self::BitcoinStack,
        deadline: &Deadline,
        owner: FixtureStartupOwner,
    ) -> Result<Self::LiquidStack, FixtureError> {
        Fixture::<Liquid>::builder()
            .node_image(builder.elements_image.clone())
            .electrs_image(builder.liquid_electrs_image.clone())
            .network(bitcoin.network_name().to_owned())
            .extra_node_args(peg_node_args(bitcoin.node_container_name()))
            .start_under_with_owner(deadline, owner)
            .await
    }
    async fn connect(
        &self,
        bitcoin: &Self::BitcoinStack,
        liquid: &Self::LiquidStack,
    ) -> Result<Peg, FixtureError> {
        Peg::connect(bitcoin.client().clone(), liquid.client().clone())
            .await
            .map_err(FixtureError::Client)
    }
    async fn attach_bitcoin_logs(
        &self,
        bitcoin: &Self::BitcoinStack,
        deadline: &Deadline,
        error: FixtureError,
    ) -> FixtureError {
        bitcoin.attach_inner_logs(deadline, error).await
    }
    async fn shutdown_bitcoin(&self, bitcoin: Self::BitcoinStack) {
        let _ = bitcoin.shutdown().await;
    }
}

async fn start_peg_stacks<E: PegEnvironment>(
    builder: PegPairBuilder,
    environment: E,
    deadline: Deadline,
    cancellation: &mut CoordinatorCancellation,
) -> Result<StartedPegPair<E>, FixtureError> {
    let bitcoin = tokio::select! {
        biased;
        bitcoin = environment.start_bitcoin(&builder, &deadline, FixtureStartupOwner::CompositeCoordinator) => bitcoin?,
        () = cancellation.cancelled() => return Err(cancelled_startup_error("peg pair")),
    };
    // Dropping either in-flight startup waits for its owned supervisor to settle and clean up.
    // Only this coordinator thread may wait without a deadline; the public guard can detach it.
    let liquid = tokio::select! {
        biased;
        liquid = environment.start_liquid(&builder, &bitcoin, &deadline, FixtureStartupOwner::CompositeCoordinator) => liquid,
        () = cancellation.cancelled() => Err(cancelled_startup_error("peg pair")),
    };
    let liquid = match liquid {
        Ok(liquid) => liquid,
        Err(error) => {
            let error = environment
                .attach_bitcoin_logs(&bitcoin, &deadline, error)
                .await;
            environment.shutdown_bitcoin(bitcoin).await;
            return Err(error);
        }
    };
    let handles = PegHandles { liquid, bitcoin };
    // Holding both stacks in declaration order also protects connect failure and cancellation.
    let peg = tokio::select! {
        biased;
        peg = deadline.run(PEG_SERVICE, "verifying both chains report the same parent", environment.connect(&handles.bitcoin, &handles.liquid)) => peg??,
        () = cancellation.cancelled() => return Err(cancelled_startup_error("peg pair")),
    };
    Ok(StartedPegPair { handles, peg })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::{PegHandles, PegPair, peg_node_args};
    use crate::{ContainerImage, node::merge_node_args};

    #[derive(Clone, Default)]
    struct SlowLiquidEngine {
        removing_liquid: Arc<tokio::sync::Notify>,
        release_liquid: Arc<tokio::sync::Notify>,
        removed: Arc<Mutex<Vec<&'static str>>>,
    }

    impl crate::runtime::ContainerEngine for SlowLiquidEngine {
        fn endpoint_host(&self) -> &str {
            "127.0.0.1"
        }
        async fn create_network(
            &self,
            name: &str,
            _: std::collections::HashMap<String, String>,
        ) -> crate::runtime::EngineResult<String> {
            Ok(name.to_owned())
        }
        async fn ensure_image(
            &self,
            _: &crate::runtime::ContainerSpec,
        ) -> crate::runtime::EngineResult<()> {
            Ok(())
        }
        async fn create_container(
            &self,
            spec: &crate::runtime::ContainerSpec,
            _: std::collections::HashMap<String, String>,
        ) -> crate::runtime::EngineResult<String> {
            Ok(spec.name.clone())
        }
        async fn start_container(&self, id: &str) -> crate::runtime::EngineResult<()> {
            if id == "bitcoin" {
                return Ok(());
            }
            Err(crate::runtime::EngineError::new(
                "start Liquid",
                std::io::Error::other("Liquid configuration failed"),
            ))
        }
        async fn mapped_port(&self, _: &str, port: u16) -> crate::runtime::EngineResult<u16> {
            Ok(port)
        }
        async fn logs(&self, _: &str) -> crate::runtime::EngineResult<String> {
            Ok(String::new())
        }
        async fn read_container_file(
            &self,
            _: &str,
            _: &str,
            _: usize,
        ) -> crate::runtime::EngineResult<Vec<u8>> {
            unreachable!()
        }
        async fn remove_container(&self, id: &str) -> crate::runtime::EngineResult<()> {
            if id == "bitcoin" {
                self.removed.lock().unwrap().push("bitcoin");
                return Ok(());
            }
            self.removing_liquid.notify_one();
            self.release_liquid.notified().await;
            self.removed.lock().unwrap().push("liquid");
            Ok(())
        }
        async fn remove_network(&self, _: &str) -> crate::runtime::EngineResult<()> {
            self.removed.lock().unwrap().push("bitcoin-network");
            Ok(())
        }
    }

    struct FakePegEnvironment(SlowLiquidEngine);
    impl super::PegEnvironment for FakePegEnvironment {
        type BitcoinStack = crate::runtime::RuntimeHandle;
        type LiquidStack = crate::runtime::RuntimeHandle;
        type ConnectedPeg = ();
        async fn start_bitcoin(
            &self,
            _: &super::PegPairBuilder,
            deadline: &crate::deadline::Deadline,
            owner: crate::fixture::FixtureStartupOwner,
        ) -> Result<Self::BitcoinStack, crate::FixtureError> {
            let work = |mut startup: crate::runtime::Startup<SlowLiquidEngine>| async move {
                startup.create_network("bitcoin-network".into()).await?;
                let spec = crate::runtime::node_spec::<nigiri_rs_core::Bitcoin>(
                    ContainerImage::bitcoind_default(),
                    "bitcoin-network".into(),
                    "bitcoin".into(),
                    Vec::new(),
                )
                .unwrap();
                startup.start_container(spec).await.map(|_| ())
            };
            let (_, runtime) = match owner {
                crate::fixture::FixtureStartupOwner::CallerDeadline => {
                    crate::runtime::supervise(self.0.clone(), deadline.clone(), work).await?
                }
                crate::fixture::FixtureStartupOwner::CompositeCoordinator => {
                    crate::runtime::supervise_for_coordinator(self.0.clone(), work).await?
                }
            };
            Ok(runtime)
        }
        async fn start_liquid(
            &self,
            _: &super::PegPairBuilder,
            _: &Self::BitcoinStack,
            deadline: &crate::deadline::Deadline,
            owner: crate::fixture::FixtureStartupOwner,
        ) -> Result<Self::LiquidStack, crate::FixtureError> {
            let spec = crate::runtime::node_spec::<nigiri_rs_core::Liquid>(
                ContainerImage::elements_default(),
                "bitcoin-network".into(),
                "liquid".into(),
                Vec::new(),
            )?;
            let work = |mut startup: crate::runtime::Startup<SlowLiquidEngine>| async move {
                startup.start_container(spec).await
            };
            let (_, runtime) = match owner {
                crate::fixture::FixtureStartupOwner::CallerDeadline => {
                    crate::runtime::supervise(self.0.clone(), deadline.clone(), work).await?
                }
                crate::fixture::FixtureStartupOwner::CompositeCoordinator => {
                    crate::runtime::supervise_for_coordinator(self.0.clone(), work).await?
                }
            };
            Ok(runtime)
        }
        async fn connect(
            &self,
            _: &Self::BitcoinStack,
            _: &Self::LiquidStack,
        ) -> Result<(), crate::FixtureError> {
            unreachable!()
        }
        async fn attach_bitcoin_logs(
            &self,
            _: &Self::BitcoinStack,
            _: &crate::deadline::Deadline,
            error: crate::FixtureError,
        ) -> crate::FixtureError {
            error
        }
        async fn shutdown_bitcoin(&self, bitcoin: Self::BitcoinStack) {
            bitcoin.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn failed_liquid_cleanup_keeps_bitcoin_and_network_alive_after_public_deadline() {
        assert_failed_liquid_cleanup_is_ordered(false).await;
    }

    #[tokio::test]
    async fn cancelling_peg_startup_keeps_bitcoin_alive_until_liquid_cleanup_finishes() {
        assert_failed_liquid_cleanup_is_ordered(true).await;
    }

    async fn assert_failed_liquid_cleanup_is_ordered(abort_caller: bool) {
        let engine = SlowLiquidEngine::default();
        let deadline = crate::deadline::Deadline::new(Duration::from_millis(100)).unwrap();
        let caller = tokio::spawn(
            PegPair::builder().start_with_environment(FakePegEnvironment(engine.clone()), deadline),
        );
        tokio::time::timeout(Duration::from_secs(1), engine.removing_liquid.notified())
            .await
            .expect("Liquid failure begins cleanup");
        if abort_caller {
            caller.abort();
        }
        let result = tokio::time::timeout(Duration::from_secs(1), caller)
            .await
            .expect("public startup and cancellation stay bounded");
        let failed = match result {
            Err(error) => error.is_cancelled(),
            Ok(result) => result.is_err(),
        };
        let before_release = engine.removed.lock().unwrap().clone();
        engine.release_liquid.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            while engine.removed.lock().unwrap().len() < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background coordinator finishes both cleanup phases");
        assert!(failed, "startup must fail or be cancelled");
        assert!(
            before_release.is_empty(),
            "Bitcoin/network teardown raced Liquid: {before_release:?}"
        );
        assert_eq!(
            *engine.removed.lock().unwrap(),
            ["liquid", "bitcoin", "bitcoin-network"]
        );
    }

    /// Reports the order in which the pair released its inner stacks.
    struct DropOrderRecorder {
        name: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Drop for DropOrderRecorder {
        fn drop(&mut self) {
            self.order
                .lock()
                .expect("the recorded order is never poisoned")
                .push(self.name);
        }
    }

    // Catches a regression that reorders the pair's inner stacks. `elementsd` holds an RPC
    // connection to `bitcoind` through `-mainchainrpc*`, so the whole Liquid stack must be released
    // before the Bitcoin node it validates against disappears underneath it.
    #[test]
    fn the_liquid_stack_is_released_before_the_bitcoin_node_it_validates_against() {
        let order = Arc::new(Mutex::new(Vec::new()));

        drop(PegHandles {
            liquid: DropOrderRecorder {
                name: "liquid",
                order: Arc::clone(&order),
            },
            bitcoin: DropOrderRecorder {
                name: "bitcoin",
                order: Arc::clone(&order),
            },
        });

        assert_eq!(
            *order.lock().expect("the recorded order is never poisoned"),
            ["liquid", "bitcoin"]
        );
    }

    // Catches a regression in the five arguments that are the whole difference between a peg pair
    // and two unrelated fixtures. A wrong host, port, or credential produces a node that starts
    // and then cannot validate a claim, which surfaces as an unexplained peg-in failure.
    #[test]
    fn the_peg_arguments_wire_elements_to_the_bitcoin_container() {
        let args = peg_node_args("nigiri-rs-bitcoind-abc");

        assert_eq!(
            args,
            vec![
                "-validatepegin=1".to_owned(),
                "-mainchainrpchost=nigiri-rs-bitcoind-abc".to_owned(),
                "-mainchainrpcport=18443".to_owned(),
                "-mainchainrpcuser=admin1".to_owned(),
                "-mainchainrpcpassword=123".to_owned(),
            ]
        );
    }

    // Catches the failure the merge in `node::merge_node_args` exists to prevent, at the one call
    // site that matters: the standalone Liquid chain sets `-validatepegin=0`, and a pair that ships
    // both values has undocumented precedence deciding whether peg-in works at all.
    #[test]
    fn the_peg_arguments_replace_the_standalone_chains_validatepegin() {
        use nigiri_rs_core::Liquid;

        use crate::chain::FixtureChain;

        let merged = merge_node_args(Liquid::node_cmd(), &peg_node_args("nigiri-rs-bitcoind-abc"));

        assert_eq!(
            merged
                .iter()
                .filter(|argument| argument.starts_with("-validatepegin"))
                .collect::<Vec<_>>(),
            vec![&"-validatepegin=1".to_owned()],
            "{merged:?}"
        );
    }

    // Catches a regression that changes what a caller gets without asking: the four pinned images
    // and the doubled budget four containers need.
    #[test]
    fn builder_defaults_are_pinned_and_two_minutes() {
        let builder = PegPair::builder();

        assert_eq!(builder.startup_timeout, Duration::from_secs(120));
        assert_eq!(builder.bitcoind_image, ContainerImage::bitcoind_default());
        assert_eq!(
            builder.bitcoin_electrs_image,
            ContainerImage::electrs_default()
        );
        assert_eq!(builder.elements_image, ContainerImage::elements_default());
        assert_eq!(
            builder.liquid_electrs_image,
            ContainerImage::electrs_liquid_default()
        );
    }

    // Catches a regression that drops a builder override, which would silently start a pinned image
    // a caller had explicitly replaced.
    #[test]
    fn builder_methods_return_updated_values() {
        let image = ContainerImage::new("registry.invalid/image", "v1");

        let builder = PegPair::builder()
            .startup_timeout(Duration::from_secs(200))
            .bitcoind_image(image.clone())
            .bitcoin_electrs_image(image.clone())
            .elements_image(image.clone())
            .liquid_electrs_image(image.clone());

        assert_eq!(builder.startup_timeout, Duration::from_secs(200));
        assert_eq!(builder.bitcoind_image, image);
        assert_eq!(builder.bitcoin_electrs_image, image);
        assert_eq!(builder.elements_image, image);
        assert_eq!(builder.liquid_electrs_image, image);
    }

    // Catches a regression that defers image validation or budget validation until Docker has
    // already been asked to start something.
    #[tokio::test]
    async fn invalid_inputs_are_rejected_before_any_container_is_started() {
        let rejected = [
            PegPair::builder().startup_timeout(Duration::ZERO),
            PegPair::builder().elements_image(ContainerImage::new("", "v1")),
        ];

        for builder in rejected {
            let error = builder
                .start()
                .await
                .expect_err("invalid pair input must be rejected");
            assert!(
                matches!(error, crate::FixtureError::InvalidConfiguration { .. }),
                "{error}"
            );
        }
    }

    // Catches a regression in any of the wiring guarantees a caller reads straight off `PegPair` —
    // this is the one test that proves the whole assembly, so a pair that reports itself started
    // must have four containers on one network and a `Peg` that already verified the two chains
    // are paired.
    #[cfg(feature = "docker-tests")]
    #[tokio::test]
    async fn a_started_pair_is_wired_and_reports_both_chains() {
        let pair = PegPair::start()
            .await
            .expect("a pinned peg pair must start against a real daemon");

        assert_eq!(
            pair.handles.bitcoin.network_name(),
            pair.handles.liquid.network_name(),
            "both stacks must share one network"
        );

        let bitcoin_height = pair
            .bitcoin()
            .block_height()
            .await
            .expect("the Bitcoin half must serve its Esplora tip");
        assert_eq!(bitcoin_height, 101);
        let liquid_height = pair
            .liquid()
            .block_height()
            .await
            .expect("the Liquid half must serve its Esplora tip");
        assert_eq!(liquid_height, 1);

        assert_eq!(
            pair.peg().pegin_confirmation_depth(),
            8,
            "both Elements images tested report a depth of 8; a change here is a chain change"
        );

        drop(pair);
    }

    /// The label Docker puts on a volume it created implicitly for a container.
    #[cfg(feature = "docker-tests")]
    const DOCKER_ANONYMOUS_VOLUME_LABEL: &str = "com.docker.volume.anonymous";

    // Catches a regression that leaves one of a pair's four containers, its shared network, or a
    // volume behind. A composite is where teardown is easiest to get wrong: the network belongs to
    // neither stack alone, and removing it with the first drop would strand the second.
    #[cfg(feature = "docker-tests")]
    #[tokio::test]
    async fn explicit_shutdown_removes_every_resource_it_created() {
        use bollard::{Docker, models::MountPointTypeEnum};

        let pair = PegPair::start()
            .await
            .expect("a pinned peg pair must start against a real daemon");

        let mut containers = Vec::new();
        containers.extend(pair.handles.bitcoin.container_ids());
        containers.extend(pair.handles.liquid.container_ids());
        assert_eq!(containers.len(), 4, "a pair owns four containers");
        let network = pair.handles.bitcoin.network_name().to_owned();

        let docker = Docker::connect_with_local_defaults()
            .expect("the daemon that just served the pair is reachable");

        let mut volumes = Vec::new();
        for container in &containers {
            let inspected = docker
                .inspect_container(container, None)
                .await
                .expect("a running pair container can be inspected");
            for mount in inspected.mounts.unwrap_or_default() {
                assert_eq!(
                    mount.typ,
                    Some(MountPointTypeEnum::VOLUME),
                    "a pair may only mount Docker volumes: {mount:?}"
                );
                let name = mount.name.clone().expect("a volume mount names its volume");
                let volume = docker
                    .inspect_volume(&name)
                    .await
                    .expect("a mounted volume can be inspected");
                assert_eq!(
                    volume.labels.keys().collect::<Vec<_>>(),
                    vec![DOCKER_ANONYMOUS_VOLUME_LABEL],
                    "{name} is not a volume this pair created"
                );
                volumes.push(name);
            }
        }
        println!("created containers={containers:?} network={network} volumes={volumes:?}");

        pair.shutdown()
            .await
            .expect("explicit pair cleanup must succeed");

        // Removal is asynchronous, so this polls rather than asserting once.
        let mut outstanding = Vec::new();
        for _ in 0..100 {
            outstanding.clear();
            for container in &containers {
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
            "shutting down the pair left these behind: {outstanding:?}"
        );
    }
}
