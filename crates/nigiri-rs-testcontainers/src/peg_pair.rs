//! A Bitcoin and Liquid pair wired for Liquid's peg: four containers, one network.

use std::{fmt, time::Duration};

use nigiri_rs_core::{Bitcoin, Liquid, NigiriClient, Peg};

use crate::{
    ContainerImage, Fixture, FixtureError, RPC_PASSWORD, RPC_USER, chain::FixtureChain,
    deadline::Deadline,
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
/// use nigiri_rs_testcontainers::PegPair;
///
/// # async fn example() -> Result<(), nigiri_rs_testcontainers::FixtureError> {
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
/// Dropping the pair removes all four containers, their anonymous volumes, and the shared network.
/// The Liquid stack is released first: `elementsd` holds an RPC connection to `bitcoind` and must
/// not outlive it.
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

// Written by hand rather than derived, exactly as for `Fixture`: both held clients carry the RPC
// password, and the rest of the crate works to keep that out of any caller-visible text.
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

        let bitcoin = Fixture::<Bitcoin>::builder()
            .node_image(self.bitcoind_image)
            .electrs_image(self.bitcoin_electrs_image)
            .start_under(&deadline)
            .await?;

        let liquid = match Fixture::<Liquid>::builder()
            .node_image(self.elements_image)
            .electrs_image(self.liquid_electrs_image)
            .network(bitcoin.network_name().to_owned())
            .extra_node_args(peg_node_args(bitcoin.node_container_name()))
            .start_under(&deadline)
            .await
        {
            Ok(liquid) => liquid,
            // The Liquid half failed against a Bitcoin node whose log only that fixture holds.
            Err(error) => return Err(bitcoin.attach_inner_logs(error).await),
        };

        // Run here rather than left to the first peg call: a mis-wired pair is a startup failure,
        // and charged to the same clock as everything above it. A mis-wired pair surfaces as
        // `NigiriError::PegNotConfigured`, whose detail already names both chains' block hashes;
        // `FixtureError::Client` has no diagnostics field to attach a container log to, so none is.
        let paired = deadline
            .run(
                PEG_SERVICE,
                "verifying both chains report the same parent",
                Peg::connect(bitcoin.client().clone(), liquid.client().clone()),
            )
            .await?;
        let peg = match paired {
            Ok(peg) => peg,
            Err(source) => return Err(FixtureError::Client(source)),
        };

        Ok(PegPair {
            handles: PegHandles { liquid, bitcoin },
            peg,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::{PegHandles, PegPair, peg_node_args};
    use crate::{ContainerImage, node::merge_node_args};

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

    // The one test that proves the whole assembly: a pair that reports itself started must have
    // four containers on one network and a `Peg` that already verified the two chains are paired.
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
    const DOCKER_ANONYMOUS_VOLUME_LABEL: &str = "com.docker.volume.anonymous";

    // Catches a regression that leaves one of a pair's four containers, its shared network, or a
    // volume behind. A composite is where teardown is easiest to get wrong: the network belongs to
    // neither stack alone, and removing it with the first drop would strand the second.
    #[tokio::test]
    async fn dropping_a_pair_removes_every_resource_it_created() {
        use testcontainers::bollard::{Docker, models::MountPointTypeEnum};

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

        drop(pair);

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
            "dropping the pair left these behind: {outstanding:?}"
        );
    }
}
