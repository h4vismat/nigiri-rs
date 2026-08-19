//! Two stateless-wallet LND nodes sharing one synchronized Bitcoin fixture.

use std::{collections::BTreeSet, fmt, future::Future, time::Duration};

use bitcoin::{
    Address, Amount, Network, OutPoint, Txid, address::NetworkUnchecked, hashes::sha256,
    secp256k1::PublicKey,
};
use futures_util::future::Either;
use nigiri_rs_core::{Bitcoin, NigiriClient};
use nigiri_rs_lnd::{
    CreateInvoiceRequest, InvoiceRecord, InvoiceState, LndBootstrapConfig, LndClient, LndError,
    Millisats, NodeInfo, OpenChannelRequest, PaymentOptions, PaymentRecord, PaymentState,
    PeerAddress, Sats, initialize_wallet,
};
use url::Url;
use uuid::Uuid;

use crate::{
    ContainerImage, Fixture, FixtureError,
    chain::{FixtureChain, bitcoin_zmq_args},
    deadline::Deadline,
    diagnostics::{redacted_source, redacted_tail},
    lnd::{LND_GRPC_PORT, TLS_CERT_PATH},
    readiness::{RETRY_DELAY, wait_before_retry},
    runtime::{
        ContainerEngine, CoordinatorCancellation, RuntimeHandle, Startup, attach_container_log,
        cancelled_startup_error, coordinate_startup, lnd_spec, runtime_error, supervise,
        supervise_for_coordinator,
    },
};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_CHANNEL_CAPACITY: Sats = Sats::new(2_000_000);
const DEFAULT_PUSH_AMOUNT: Sats = Sats::new(1_000_000);
const MIN_NOMINAL_SIDE_BALANCE: u64 = 100_000;
const FUNDING_RESERVE: Sats = Sats::new(200_000);
const CHANNEL_CONFIRMATIONS: u64 = 6;
const READINESS_PAYMENT: Millisats = Millisats::new(1_000);
const READINESS_FEE_LIMIT: Millisats = Millisats::new(10_000);
const READINESS_MEMO: &str = "nigiri-rs readiness probe";
const LND_PEER_PORT: u16 = 9_735;

/// A synchronized Bitcoin fixture and two authenticated LND clients joined by a ready channel.
pub struct LndPair {
    handles: LndHandles<RuntimeHandle, Fixture<Bitcoin>>,
    alice: LndClient,
    bob: LndClient,
    channel_point: OutPoint,
    #[allow(
        dead_code,
        reason = "resource identifiers remain private and support lifecycle verification"
    )]
    container_ids: [String; 2],
}

/// Dependency-ordered ownership: Rust drops fields in declaration order.
struct LndHandles<LndRuntime, BitcoinStack> {
    lnd: LndRuntime,
    bitcoin: BitcoinStack,
}

struct StartedLndPair<Client, BitcoinStack> {
    handles: LndHandles<RuntimeHandle, BitcoinStack>,
    nodes: StartedLndNodes<Client>,
}

trait LndPairEnvironment: Clone + Send + Sync + 'static {
    type BitcoinStack: Send + 'static;
    type Engine: ContainerEngine;
    type Connector: LndNodeConnector;
    type BitcoinTip: BitcoinTip;

    async fn start_bitcoin_for_coordinator(
        &self,
        bitcoind_image: ContainerImage,
        electrs_image: ContainerImage,
        deadline: &Deadline,
    ) -> Result<Self::BitcoinStack, FixtureError>;

    fn engine(&self, bitcoin: &Self::BitcoinStack) -> Self::Engine;
    fn network_name(&self, bitcoin: &Self::BitcoinStack) -> String;
    fn node_container_name(&self, bitcoin: &Self::BitcoinStack) -> String;
    fn bitcoin_tip(&self, bitcoin: &Self::BitcoinStack) -> Self::BitcoinTip;
    fn connector(&self) -> Self::Connector;

    async fn attach_inner_logs(
        &self,
        bitcoin: &Self::BitcoinStack,
        deadline: &Deadline,
        error: FixtureError,
    ) -> FixtureError;

    async fn shutdown_bitcoin(&self, bitcoin: Self::BitcoinStack) -> Result<(), FixtureError>;
}

#[derive(Clone, Copy)]
struct RealLndPairEnvironment;

impl LndPairEnvironment for RealLndPairEnvironment {
    type BitcoinStack = Fixture<Bitcoin>;
    type Engine = crate::runtime::BollardEngine;
    type Connector = RealLndConnector;
    type BitcoinTip = NigiriClient<Bitcoin>;

    async fn start_bitcoin_for_coordinator(
        &self,
        bitcoind_image: ContainerImage,
        electrs_image: ContainerImage,
        deadline: &Deadline,
    ) -> Result<Self::BitcoinStack, FixtureError> {
        Fixture::<Bitcoin>::builder()
            .node_image(bitcoind_image)
            .electrs_image(electrs_image)
            .extra_node_args(bitcoin_zmq_args())
            .start_under_for_coordinator(deadline)
            .await
    }

    fn engine(&self, bitcoin: &Self::BitcoinStack) -> Self::Engine {
        bitcoin.engine()
    }

    fn network_name(&self, bitcoin: &Self::BitcoinStack) -> String {
        bitcoin.network_name().to_owned()
    }

    fn node_container_name(&self, bitcoin: &Self::BitcoinStack) -> String {
        bitcoin.node_container_name().to_owned()
    }

    fn bitcoin_tip(&self, bitcoin: &Self::BitcoinStack) -> Self::BitcoinTip {
        bitcoin.client().clone()
    }

    fn connector(&self) -> Self::Connector {
        RealLndConnector
    }

    async fn attach_inner_logs(
        &self,
        bitcoin: &Self::BitcoinStack,
        deadline: &Deadline,
        error: FixtureError,
    ) -> FixtureError {
        bitcoin.attach_inner_logs(deadline, error).await
    }

    async fn shutdown_bitcoin(&self, bitcoin: Self::BitcoinStack) -> Result<(), FixtureError> {
        bitcoin.shutdown().await
    }
}

impl fmt::Debug for LndPair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LndPair")
            .field("bitcoin", &self.handles.bitcoin)
            .field("alice", &self.alice)
            .field("bob", &self.bob)
            .field("channel_point", &self.channel_point)
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

    /// Starts the pinned topology and returns after its channel settles payments both ways.
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

    /// The confirmed funding output shared by Alice's and Bob's active channel views.
    #[must_use]
    pub const fn channel_point(&self) -> OutPoint {
        self.channel_point
    }

    /// Removes both LND nodes, then the backing Bitcoin stack, attempting both cleanup phases.
    pub async fn shutdown(self) -> Result<(), FixtureError> {
        let Self {
            handles: LndHandles { lnd, bitcoin },
            alice: _,
            bob: _,
            channel_point: _,
            container_ids: _,
        } = self;

        let lnd_result = lnd
            .shutdown()
            .await
            .map_err(|error| runtime_error("LND pair", error));
        let bitcoin_result = bitcoin.shutdown().await;
        lnd_result.and(bitcoin_result)
    }

    #[cfg(test)]
    #[allow(
        dead_code,
        reason = "lifecycle tests consume these private identifiers"
    )]
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

/// Overrides for the shared deadline, four images, and balanced channel allocation.
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

    /// Starts the backing fixture and ready-to-pay LND channel under one shared deadline.
    pub async fn start(self) -> Result<LndPair, FixtureError> {
        let started = self.start_with_environment(RealLndPairEnvironment).await?;
        let StartedLndPair { handles, nodes } = started;

        Ok(LndPair {
            handles,
            alice: nodes.alice,
            bob: nodes.bob,
            channel_point: nodes.channel_point,
            container_ids: nodes.container_ids,
        })
    }

    async fn start_with_environment<R>(
        &self,
        environment: R,
    ) -> Result<
        StartedLndPair<<R::Connector as LndNodeConnector>::Client, R::BitcoinStack>,
        FixtureError,
    >
    where
        R: LndPairEnvironment,
    {
        self.start_with_environment_and_handoff(environment, std::future::ready(()))
            .await
    }

    async fn start_with_environment_and_handoff<R, H>(
        &self,
        environment: R,
        before_handoff_ack: H,
    ) -> Result<
        StartedLndPair<<R::Connector as LndNodeConnector>::Client, R::BitcoinStack>,
        FixtureError,
    >
    where
        R: LndPairEnvironment,
        H: Future<Output = ()>,
    {
        let allocation = self.validate()?;
        let deadline = Deadline::new(self.startup_timeout)?;
        let bitcoind_image = self.bitcoind_image.clone();
        let bitcoin_electrs_image = self.bitcoin_electrs_image.clone();
        let alice_image = self.alice_image.clone();
        let bob_image = self.bob_image.clone();
        let coordinator_deadline = deadline.clone();
        let work_deadline = deadline.clone();
        let coordinated = coordinate_startup(
            coordinator_deadline,
            move |mut cancellation: CoordinatorCancellation| async move {
                let bitcoin = tokio::select! {
                    biased;
                    bitcoin = environment.start_bitcoin_for_coordinator(
                        bitcoind_image,
                        bitcoin_electrs_image,
                        &work_deadline,
                    ) => bitcoin?,
                    () = cancellation.cancelled() => {
                        return Err(cancelled_startup_error("LND pair"));
                    }
                };
                let engine = environment.engine(&bitcoin);
                let network_name = environment.network_name(&bitcoin);
                let node_container_name = environment.node_container_name(&bitcoin);
                let bitcoin_tip = environment.bitcoin_tip(&bitcoin);
                let connector = environment.connector();
                let nodes = tokio::select! {
                    biased;
                    nodes = start_lnd_nodes_for_coordinator(
                        engine,
                        network_name,
                        node_container_name,
                        alice_image,
                        bob_image,
                        &work_deadline,
                        connector,
                        bitcoin_tip,
                        allocation,
                    ) => Some(nodes),
                    () = cancellation.cancelled() => None,
                };

                let (started, lnd_runtime) = match nodes {
                    Some(Ok(nodes)) => nodes,
                    Some(Err(error)) => {
                        let error = environment
                            .attach_inner_logs(&bitcoin, &work_deadline, error)
                            .await;
                        let _ = environment.shutdown_bitcoin(bitcoin).await;
                        return Err(error);
                    }
                    None => {
                        let _ = environment.shutdown_bitcoin(bitcoin).await;
                        return Err(cancelled_startup_error("LND pair"));
                    }
                };

                Ok(StartedLndPair {
                    handles: LndHandles {
                        lnd: lnd_runtime,
                        bitcoin,
                    },
                    nodes: started,
                })
            },
            before_handoff_ack,
        );
        deadline
            .run("LND pair", "coordinating complete LND startup", coordinated)
            .await?
    }

    #[cfg(test)]
    async fn start_with_environment_before_handoff_ack<R, H>(
        &self,
        environment: R,
        before_ack: H,
    ) -> Result<
        StartedLndPair<<R::Connector as LndNodeConnector>::Client, R::BitcoinStack>,
        FixtureError,
    >
    where
        R: LndPairEnvironment,
        H: Future<Output = ()>,
    {
        self.start_with_environment_and_handoff(environment, before_ack)
            .await
    }

    fn validate(&self) -> Result<ChannelAllocation, FixtureError> {
        Deadline::validate_duration(self.startup_timeout)?;
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
        let funding_amount = capacity
            .checked_add(FUNDING_RESERVE.as_u64())
            .ok_or_else(|| invalid("LND channel funding amount overflowed"))?;
        i64::try_from(capacity)
            .map_err(|_| invalid("LND channel capacity exceeds the signed request range"))?;
        i64::try_from(push)
            .map_err(|_| invalid("LND channel push amount exceeds the signed request range"))?;
        if funding_amount > Amount::MAX_MONEY.to_sat() {
            return Err(invalid(
                "LND wallet funding amount exceeds Bitcoin's monetary range",
            ));
        }
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
        Ok(ChannelAllocation {
            capacity: self.channel_capacity,
            push: self.push_amount,
            funding_amount: Sats::new(funding_amount),
        })
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
    channel_point: OutPoint,
    /// Alice then Bob; diagnostics use the reverse dependency order explicitly.
    container_ids: [String; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ChannelAllocation {
    capacity: Sats,
    push: Sats,
    funding_amount: Sats,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LndSyncStatus {
    public_key: PublicKey,
    block_height: u32,
    network_is_regtest: bool,
    synced_to_chain: bool,
    synced_to_graph: bool,
}

impl From<NodeInfo> for LndSyncStatus {
    fn from(info: NodeInfo) -> Self {
        Self {
            public_key: info.public_key(),
            block_height: info.block_height(),
            network_is_regtest: info.network() == "regtest",
            synced_to_chain: info.synced_to_chain(),
            synced_to_graph: info.synced_to_graph(),
        }
    }
}

trait LndNodeConnector: Clone + Send + Sync + 'static {
    type Client: Send + Sync + 'static;
    type Invoice: Send + Sync + 'static;

    fn initialize(
        &self,
        config: &LndBootstrapConfig,
        password: &[u8],
    ) -> impl Future<Output = Result<Self::Client, LndError>> + Send;

    fn get_info(
        &self,
        client: &Self::Client,
    ) -> impl Future<Output = Result<LndSyncStatus, LndError>> + Send;

    fn new_address(
        &self,
        client: &Self::Client,
    ) -> impl Future<Output = Result<String, LndError>> + Send;

    fn confirmed_balance(
        &self,
        client: &Self::Client,
    ) -> impl Future<Output = Result<Sats, LndError>> + Send;

    fn peer_connected(
        &self,
        client: &Self::Client,
        public_key: PublicKey,
    ) -> impl Future<Output = Result<bool, LndError>> + Send;

    fn connect_peer(
        &self,
        client: &Self::Client,
        peer: PeerAddress,
    ) -> impl Future<Output = Result<(), LndError>> + Send;

    fn open_channel(
        &self,
        client: &Self::Client,
        request: OpenChannelRequest,
    ) -> impl Future<Output = Result<OutPoint, LndError>> + Send;

    fn channel_readiness(
        &self,
        client: &Self::Client,
        channel_point: OutPoint,
        remote_public_key: PublicKey,
    ) -> impl Future<Output = Result<Option<ChannelReadiness>, LndError>> + Send;

    fn create_readiness_invoice(
        &self,
        client: &Self::Client,
    ) -> impl Future<Output = Result<(Self::Invoice, sha256::Hash), LndError>> + Send;

    fn pay_readiness_invoice(
        &self,
        client: &Self::Client,
        invoice: &Self::Invoice,
    ) -> impl Future<Output = Result<PaymentReadiness, LndError>> + Send;

    fn invoice_state(
        &self,
        client: &Self::Client,
        payment_hash: sha256::Hash,
    ) -> impl Future<Output = Result<InvoiceState, LndError>> + Send;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ChannelReadiness {
    active: bool,
    local_balance: Millisats,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PaymentReadiness {
    payment_hash: sha256::Hash,
    state: PaymentState,
}

#[derive(Clone, Copy)]
struct RealLndConnector;

impl LndNodeConnector for RealLndConnector {
    type Client = LndClient;
    type Invoice = InvoiceRecord;

    async fn initialize(
        &self,
        config: &LndBootstrapConfig,
        password: &[u8],
    ) -> Result<Self::Client, LndError> {
        let config = initialize_wallet(
            LndBootstrapConfig {
                endpoint: config.endpoint.clone(),
                tls_certificate: config.tls_certificate.clone(),
                timeout: config.timeout,
            },
            password,
        )
        .await?;
        LndClient::with_config(config)
    }

    async fn get_info(&self, client: &Self::Client) -> Result<LndSyncStatus, LndError> {
        client.get_info().await.map(Into::into)
    }

    async fn new_address(&self, client: &Self::Client) -> Result<String, LndError> {
        let address: Address<NetworkUnchecked> = client.new_address().await?;
        address
            .require_network(Network::Regtest)
            .map(|address| address.to_string())
            .map_err(|_| LndError::InvalidResponse {
                operation: "new wallet address".into(),
                detail: "LND returned an address outside Bitcoin regtest".into(),
                identifier: None,
            })
    }

    async fn confirmed_balance(&self, client: &Self::Client) -> Result<Sats, LndError> {
        client
            .wallet_balance()
            .await
            .map(|balance| balance.confirmed())
    }

    async fn peer_connected(
        &self,
        client: &Self::Client,
        public_key: PublicKey,
    ) -> Result<bool, LndError> {
        client.list_peers().await.map(|peers| {
            peers
                .iter()
                .any(|peer| peer.public_key() == public_key && peer.connected())
        })
    }

    async fn connect_peer(&self, client: &Self::Client, peer: PeerAddress) -> Result<(), LndError> {
        client.connect_peer(&peer).await
    }

    async fn open_channel(
        &self,
        client: &Self::Client,
        request: OpenChannelRequest,
    ) -> Result<OutPoint, LndError> {
        client.open_channel(request).await
    }

    async fn channel_readiness(
        &self,
        client: &Self::Client,
        channel_point: OutPoint,
        remote_public_key: PublicKey,
    ) -> Result<Option<ChannelReadiness>, LndError> {
        client.list_channels().await.map(|channels| {
            channels
                .iter()
                .find(|channel| {
                    channel.channel_point() == channel_point
                        && channel.remote_public_key() == remote_public_key
                })
                .map(|channel| ChannelReadiness {
                    active: channel.active(),
                    local_balance: channel.local_balance(),
                })
        })
    }

    async fn create_readiness_invoice(
        &self,
        client: &Self::Client,
    ) -> Result<(Self::Invoice, sha256::Hash), LndError> {
        let request =
            CreateInvoiceRequest::new(READINESS_PAYMENT, READINESS_MEMO, Duration::from_secs(60))?;
        let invoice = client.create_invoice(request).await?;
        let payment_hash = invoice.payment_hash();
        Ok((invoice, payment_hash))
    }

    async fn pay_readiness_invoice(
        &self,
        client: &Self::Client,
        invoice: &Self::Invoice,
    ) -> Result<PaymentReadiness, LndError> {
        let options = PaymentOptions::new(READINESS_FEE_LIMIT, Duration::from_secs(30))?;
        client
            .pay_invoice(invoice.invoice(), options)
            .await
            .map(|payment| payment_readiness(&payment))
    }

    async fn invoice_state(
        &self,
        client: &Self::Client,
        payment_hash: sha256::Hash,
    ) -> Result<InvoiceState, LndError> {
        client
            .lookup_invoice(payment_hash)
            .await
            .map(|invoice| invoice.state())
    }
}

fn payment_readiness(payment: &PaymentRecord) -> PaymentReadiness {
    PaymentReadiness {
        payment_hash: payment.payment_hash(),
        state: payment.state(),
    }
}

trait BitcoinTip: Clone + Send + Sync + 'static {
    fn block_height(&self) -> impl Future<Output = Result<u64, FixtureError>> + Send;
    fn fund_address(
        &self,
        address: &str,
        amount: Sats,
    ) -> impl Future<Output = Result<(), FixtureError>> + Send;
    fn mempool_transactions(
        &self,
    ) -> impl Future<Output = Result<BTreeSet<Txid>, FixtureError>> + Send;
    fn mine_blocks(&self, blocks: u64) -> impl Future<Output = Result<(), FixtureError>> + Send;
}

impl BitcoinTip for NigiriClient<Bitcoin> {
    async fn block_height(&self) -> Result<u64, FixtureError> {
        self.rpc::<u64, _>("getblockcount", ())
            .await
            .map_err(FixtureError::Client)
    }

    async fn fund_address(&self, address: &str, amount: Sats) -> Result<(), FixtureError> {
        self.faucet(address, Some(Amount::from_sat(amount.as_u64())))
            .await
            .map(|_| ())
            .map_err(FixtureError::Client)
    }

    async fn mempool_transactions(&self) -> Result<BTreeSet<Txid>, FixtureError> {
        self.rpc::<Vec<Txid>, _>("getrawmempool", ())
            .await
            .map(|transactions| transactions.into_iter().collect())
            .map_err(FixtureError::Client)
    }

    async fn mine_blocks(&self, blocks: u64) -> Result<(), FixtureError> {
        let address = self
            .new_address()
            .await
            .map_err(FixtureError::Client)?
            .to_string();
        self.generate_to_address(blocks, &address)
            .await
            .map(|_| ())
            .map_err(FixtureError::Client)
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(
    dead_code,
    reason = "focused lifecycle tests exercise caller-owned supervisor cancellation directly"
)]
async fn start_lnd_nodes_under<E, C, B>(
    engine: E,
    network_name: String,
    bitcoind_name: String,
    alice_image: ContainerImage,
    bob_image: ContainerImage,
    deadline: &Deadline,
    connector: C,
    bitcoin_tip: B,
    allocation: ChannelAllocation,
) -> Result<(StartedLndNodes<C::Client>, RuntimeHandle), FixtureError>
where
    E: ContainerEngine,
    C: LndNodeConnector,
    B: BitcoinTip,
{
    start_lnd_nodes(
        engine,
        network_name,
        bitcoind_name,
        alice_image,
        bob_image,
        deadline,
        connector,
        bitcoin_tip,
        allocation,
        LndStartupOwner::CallerDeadline,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_lnd_nodes_for_coordinator<E, C, B>(
    engine: E,
    network_name: String,
    bitcoind_name: String,
    alice_image: ContainerImage,
    bob_image: ContainerImage,
    deadline: &Deadline,
    connector: C,
    bitcoin_tip: B,
    allocation: ChannelAllocation,
) -> Result<(StartedLndNodes<C::Client>, RuntimeHandle), FixtureError>
where
    E: ContainerEngine,
    C: LndNodeConnector,
    B: BitcoinTip,
{
    start_lnd_nodes(
        engine,
        network_name,
        bitcoind_name,
        alice_image,
        bob_image,
        deadline,
        connector,
        bitcoin_tip,
        allocation,
        LndStartupOwner::CompositeCoordinator,
    )
    .await
}

#[derive(Clone, Copy)]
enum LndStartupOwner {
    CallerDeadline,
    CompositeCoordinator,
}

#[allow(clippy::too_many_arguments)]
async fn start_lnd_nodes<E, C, B>(
    engine: E,
    network_name: String,
    bitcoind_name: String,
    alice_image: ContainerImage,
    bob_image: ContainerImage,
    deadline: &Deadline,
    connector: C,
    bitcoin_tip: B,
    allocation: ChannelAllocation,
    owner: LndStartupOwner,
) -> Result<(StartedLndNodes<C::Client>, RuntimeHandle), FixtureError>
where
    E: ContainerEngine,
    C: LndNodeConnector,
    B: BitcoinTip,
{
    let names = LndNames::scoped();
    let endpoint_host = engine.endpoint_host().to_owned();
    let work_deadline = deadline.clone();
    let supervisor_deadline = deadline.clone();

    let work = move |mut startup: Startup<E>| async move {
        let deadline = work_deadline;
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
                return Err(attach_lnd_logs(
                    &mut startup,
                    &deadline,
                    &names.alice,
                    &names.bob,
                    error,
                )
                .await);
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
                return Err(
                    attach_lnd_logs(&mut startup, &deadline, &alice_log, &bob_log, error).await,
                );
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
                    &deadline,
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
                        &deadline,
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
                    &deadline,
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
                    &deadline,
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
                    &deadline,
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
                    &deadline,
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
                    &deadline,
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
                    &deadline,
                    &alice_container.id,
                    &bob_container.id,
                    error,
                )
                .await);
            }
        }

        let bootstrapped = startup
            .run_until_cancelled(Box::pin(bootstrap_ready_channel(
                &connector,
                &alice,
                &bob,
                &bitcoin_tip,
                &names.bob,
                allocation,
                &deadline,
            )))
            .await;
        let channel_point = match bootstrapped {
            Ok(Ok(channel_point)) => channel_point,
            Ok(Err(error)) => {
                return Err(attach_lnd_logs(
                    &mut startup,
                    &deadline,
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
                    &deadline,
                    &alice_container.id,
                    &bob_container.id,
                    error,
                )
                .await);
            }
        };

        Ok(StartedLndNodes {
            alice,
            bob,
            channel_point,
            container_ids: [alice_container.id, bob_container.id],
        })
    };
    let supervised = match owner {
        LndStartupOwner::CallerDeadline => {
            Either::Left(supervise(engine, supervisor_deadline, work))
        }
        LndStartupOwner::CompositeCoordinator => {
            Either::Right(supervise_for_coordinator(engine, work))
        }
    };
    deadline
        .run("LND pair", "starting the complete LND topology", supervised)
        .await?
}

async fn bootstrap_ready_channel<C: LndNodeConnector, B: BitcoinTip>(
    connector: &C,
    alice: &C::Client,
    bob: &C::Client,
    bitcoin: &B,
    bob_host: &str,
    allocation: ChannelAllocation,
    deadline: &Deadline,
) -> Result<OutPoint, FixtureError> {
    let alice_info = lnd_operation(
        deadline,
        "lnd-alice",
        "query Alice identity for channel bootstrap",
        connector.get_info(alice),
    )
    .await?;
    let bob_info = lnd_operation(
        deadline,
        "lnd-bob",
        "query Bob identity for channel bootstrap",
        connector.get_info(bob),
    )
    .await?;

    let address = lnd_operation(
        deadline,
        "lnd-alice",
        "request Alice P2WPKH funding address",
        connector.new_address(alice),
    )
    .await?;
    deadline
        .run(
            "lightning-channel",
            "fund Alice on-chain wallet and mine its confirmation",
            bitcoin.fund_address(&address, allocation.funding_amount),
        )
        .await??;
    wait_for_confirmed_balance(connector, alice, allocation.capacity, deadline).await?;

    ensure_peer_connected(connector, alice, bob_info.public_key, bob_host, deadline).await?;
    let request =
        OpenChannelRequest::new(bob_info.public_key, allocation.capacity, allocation.push)
            .map_err(|error| lightning_bootstrap_error("configure channel opening", error))?;
    let channel_point =
        open_and_confirm_channel(connector, alice, bitcoin, request, deadline).await?;

    wait_for_active_channel(
        connector,
        alice,
        bob,
        bitcoin,
        channel_point,
        alice_info.public_key,
        bob_info.public_key,
        bob_host,
        deadline,
    )
    .await?;
    prove_readiness_payment(connector, alice, bob, deadline).await?;
    Box::pin(prove_reverse_readiness_with_retry(
        connector,
        alice,
        bob,
        bitcoin,
        channel_point,
        alice_info.public_key,
        bob_info.public_key,
        bob_host,
        deadline,
    ))
    .await?;
    Ok(channel_point)
}

async fn wait_for_confirmed_balance<C: LndNodeConnector>(
    connector: &C,
    alice: &C::Client,
    required: Sats,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut observation = format!(
        "waiting for Alice confirmed balance to reach {} satoshis",
        required.as_u64()
    );
    loop {
        match deadline
            .run(
                "lnd-alice",
                &observation,
                connector.confirmed_balance(alice),
            )
            .await?
        {
            Ok(balance) if balance >= required => return Ok(()),
            Ok(balance) => {
                observation = format!(
                    "Alice confirmed balance={} required={}",
                    balance.as_u64(),
                    required.as_u64()
                );
            }
            Err(error) if is_transient_lnd_readiness(&error) => {
                observation = redacted_tail(&format!("Alice wallet balance is not ready: {error}"));
            }
            Err(error) => {
                return Err(lightning_bootstrap_error(
                    "query Alice confirmed balance",
                    error,
                ));
            }
        }
        wait_before_retry(deadline, "lnd-alice", &observation).await?;
    }
}

async fn ensure_peer_connected<C: LndNodeConnector>(
    connector: &C,
    alice: &C::Client,
    bob_public_key: PublicKey,
    bob_host: &str,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let peer = PeerAddress::new(bob_public_key, bob_host, LND_PEER_PORT)
        .map_err(|error| lightning_bootstrap_error("configure Bob peer address", error))?;
    let mut observation = "waiting for Alice to connect to Bob over the private network".to_owned();
    loop {
        match deadline
            .run(
                "lightning-channel",
                &observation,
                connector.peer_connected(alice, bob_public_key),
            )
            .await?
        {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if is_transient_lnd_readiness(&error) => {
                observation = redacted_tail(&format!("peer listing is not ready: {error}"));
            }
            Err(error) => {
                return Err(lightning_bootstrap_error(
                    "query Alice peer connection",
                    error,
                ));
            }
        }

        match deadline
            .run(
                "lightning-channel",
                &observation,
                connector.connect_peer(alice, peer.clone()),
            )
            .await?
        {
            Ok(()) => {}
            Err(error) if is_transient_lnd_readiness(&error) || is_already_connected(&error) => {
                observation = redacted_tail(&format!("peer connection is converging: {error}"));
            }
            Err(error) => {
                return Err(lightning_bootstrap_error("connect Alice to Bob", error));
            }
        }
        wait_before_retry(deadline, "lightning-channel", &observation).await?;
    }
}

async fn open_and_confirm_channel<C: LndNodeConnector, B: BitcoinTip>(
    connector: &C,
    alice: &C::Client,
    bitcoin: &B,
    request: OpenChannelRequest,
    deadline: &Deadline,
) -> Result<OutPoint, FixtureError> {
    let baseline = deadline
        .run(
            "lightning-channel",
            "capture mempool before opening Alice-to-Bob channel",
            bitcoin.mempool_transactions(),
        )
        .await??;
    let open = deadline.run(
        "lightning-channel",
        "open Alice-to-Bob channel",
        connector.open_channel(alice, request),
    );
    tokio::pin!(open);
    let mut opened = None;
    let mut observation = "waiting for the channel funding transaction in mempool".to_owned();

    let trigger_transactions = loop {
        let current = {
            let mempool = deadline.run(
                "lightning-channel",
                &observation,
                bitcoin.mempool_transactions(),
            );
            tokio::pin!(mempool);
            tokio::select! {
                biased;
                result = &mut open, if opened.is_none() => {
                    opened = Some(flatten_lnd_operation("open channel", result)?);
                    None
                },
                result = &mut mempool => Some(result??),
            }
        };
        let Some(current) = current else {
            continue;
        };
        let newly_observed = current
            .difference(&baseline)
            .copied()
            .collect::<BTreeSet<_>>();
        if !newly_observed.is_empty() {
            break newly_observed;
        }
        observation = "channel funding transaction is not yet in mempool".to_owned();
        wait_before_retry(deadline, "lightning-channel", &observation).await?;
    };

    deadline
        .run(
            "lightning-channel",
            "mine six channel funding confirmations",
            bitcoin.mine_blocks(CHANNEL_CONFIRMATIONS),
        )
        .await??;
    let channel_point = match opened {
        Some(channel_point) => channel_point,
        None => flatten_lnd_operation("open channel", open.await)?,
    };
    // This fixture owns an isolated regtest mempool. If callers inject simultaneous transactions,
    // the trigger set may contain more than one txid, but the final funding txid must still be one
    // of the transactions whose appearance caused this fixture to mine exactly once.
    if !trigger_transactions.contains(&channel_point.txid) {
        return Err(lightning_bootstrap_error(
            "verify channel funding transaction",
            LndError::InvalidResponse {
                operation: "open channel".into(),
                detail: "funding transaction was not observed before confirmation mining".into(),
                identifier: Some(channel_point.txid.to_string()),
            },
        ));
    }
    Ok(channel_point)
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_active_channel<C: LndNodeConnector, B: BitcoinTip>(
    connector: &C,
    alice: &C::Client,
    bob: &C::Client,
    bitcoin: &B,
    channel_point: OutPoint,
    alice_public_key: PublicKey,
    bob_public_key: PublicKey,
    bob_host: &str,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut observation = format!("waiting for active channel {channel_point}");
    loop {
        ensure_peer_connected(connector, alice, bob_public_key, bob_host, deadline).await?;
        let (alice_channel, bob_channel, alice_info, bob_info, bitcoin_height) = tokio::join!(
            deadline.run(
                "lnd-alice",
                &observation,
                connector.channel_readiness(alice, channel_point, bob_public_key),
            ),
            deadline.run(
                "lnd-bob",
                &observation,
                connector.channel_readiness(bob, channel_point, alice_public_key),
            ),
            deadline.run("lnd-alice", &observation, connector.get_info(alice)),
            deadline.run("lnd-bob", &observation, connector.get_info(bob)),
            deadline.run("bitcoind", &observation, bitcoin.block_height(),),
        );
        let alice_channel = channel_observation("query Alice channel", alice_channel?)?;
        let bob_channel = channel_observation("query Bob channel", bob_channel?)?;
        let alice_info = info_observation("query Alice graph synchronization", alice_info?)?;
        let bob_info = info_observation("query Bob graph synchronization", bob_info?)?;
        let bitcoin_height = bitcoin_height??;
        if channel_is_spendable(alice_channel)
            && channel_is_spendable(bob_channel)
            && alice_info.is_some_and(|info| graph_synchronized(info, bitcoin_height))
            && bob_info.is_some_and(|info| graph_synchronized(info, bitcoin_height))
        {
            return Ok(());
        }
        observation = format!(
            "channel {channel_point} at Bitcoin height {bitcoin_height}: Alice={alice_channel:?} info={alice_info:?}; Bob={bob_channel:?} info={bob_info:?}"
        );
        wait_before_retry(deadline, "lightning-channel", &observation).await?;
    }
}

fn info_observation(
    operation: &'static str,
    result: Result<LndSyncStatus, LndError>,
) -> Result<Option<LndSyncStatus>, FixtureError> {
    match result {
        Ok(info) => Ok(Some(info)),
        Err(error) if is_transient_lnd_readiness(&error) => Ok(None),
        Err(error) => Err(lightning_bootstrap_error(operation, error)),
    }
}

fn channel_observation(
    operation: &'static str,
    result: Result<Option<ChannelReadiness>, LndError>,
) -> Result<Option<ChannelReadiness>, FixtureError> {
    match result {
        Ok(channel) => Ok(channel),
        Err(error) if is_transient_lnd_readiness(&error) => Ok(None),
        Err(error) => Err(lightning_bootstrap_error(operation, error)),
    }
}

fn channel_is_spendable(channel: Option<ChannelReadiness>) -> bool {
    matches!(
        channel,
        Some(ChannelReadiness { active: true, local_balance })
            if local_balance > READINESS_PAYMENT
    )
}

async fn prove_readiness_payment<C: LndNodeConnector>(
    connector: &C,
    alice: &C::Client,
    bob: &C::Client,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let (invoice, payment_hash) =
        create_readiness_invoice(connector, bob, "lnd-bob", deadline).await?;
    let payment = lnd_operation(
        deadline,
        "lnd-alice",
        "pay Alice-to-Bob 1000-msat readiness invoice",
        connector.pay_readiness_invoice(alice, &invoice),
    )
    .await?;
    verify_readiness_payment(payment_hash, payment)?;
    wait_for_settled_invoice(connector, bob, "lnd-bob", payment_hash, deadline).await
}

#[allow(clippy::too_many_arguments)]
async fn prove_reverse_readiness_with_retry<C: LndNodeConnector, B: BitcoinTip>(
    connector: &C,
    alice: &C::Client,
    bob: &C::Client,
    bitcoin: &B,
    channel_point: OutPoint,
    alice_public_key: PublicKey,
    bob_public_key: PublicKey,
    bob_host: &str,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut observation = "waiting for Bob-to-Alice routing policy propagation".to_owned();
    loop {
        wait_for_active_channel(
            connector,
            alice,
            bob,
            bitcoin,
            channel_point,
            alice_public_key,
            bob_public_key,
            bob_host,
            deadline,
        )
        .await?;
        let (invoice, payment_hash) =
            create_readiness_invoice(connector, alice, "lnd-alice", deadline).await?;
        let payment = deadline
            .run(
                "lnd-bob",
                &observation,
                connector.pay_readiness_invoice(bob, &invoice),
            )
            .await?;
        match payment {
            Ok(payment) => {
                verify_readiness_payment(payment_hash, payment)?;
                return wait_for_settled_invoice(
                    connector,
                    alice,
                    "lnd-alice",
                    payment_hash,
                    deadline,
                )
                .await;
            }
            Err(error) if is_transient_routing_failure(&error) => {
                observation = redacted_tail(&format!(
                    "Bob-to-Alice route is not ready for invoice {payment_hash}: {error}"
                ));
            }
            Err(error) => {
                return Err(lightning_bootstrap_error(
                    "pay Bob-to-Alice readiness invoice",
                    error,
                ));
            }
        }
        wait_before_retry(deadline, "lightning-channel", &observation).await?;
    }
}

async fn create_readiness_invoice<C: LndNodeConnector>(
    connector: &C,
    receiver: &C::Client,
    service: &'static str,
    deadline: &Deadline,
) -> Result<(C::Invoice, sha256::Hash), FixtureError> {
    lnd_operation(
        deadline,
        service,
        "create 1000-msat readiness invoice",
        connector.create_readiness_invoice(receiver),
    )
    .await
}

fn verify_readiness_payment(
    payment_hash: sha256::Hash,
    payment: PaymentReadiness,
) -> Result<(), FixtureError> {
    if payment.payment_hash != payment_hash || payment.state != PaymentState::Succeeded {
        return Err(lightning_bootstrap_error(
            "verify readiness payment",
            LndError::InvalidResponse {
                operation: "readiness payment".into(),
                detail: "payment did not succeed with the readiness invoice hash".into(),
                identifier: Some(payment_hash.to_string()),
            },
        ));
    }
    Ok(())
}

async fn wait_for_settled_invoice<C: LndNodeConnector>(
    connector: &C,
    receiver: &C::Client,
    service: &'static str,
    payment_hash: sha256::Hash,
    deadline: &Deadline,
) -> Result<(), FixtureError> {
    let mut observation = format!("waiting for readiness invoice {payment_hash} to settle");
    loop {
        match deadline
            .run(
                service,
                &observation,
                connector.invoice_state(receiver, payment_hash),
            )
            .await?
        {
            Ok(InvoiceState::Settled) => return Ok(()),
            Ok(InvoiceState::Open | InvoiceState::Accepted) => {}
            Ok(state) => {
                return Err(lightning_bootstrap_error(
                    "verify readiness invoice",
                    LndError::InvalidResponse {
                        operation: "readiness invoice".into(),
                        detail: format!("invoice reached non-settled state {state:?}").into(),
                        identifier: Some(payment_hash.to_string()),
                    },
                ));
            }
            Err(error) if is_transient_lnd_readiness(&error) => {
                observation = redacted_tail(&format!("readiness invoice is not ready: {error}"));
            }
            Err(error) => {
                return Err(lightning_bootstrap_error("lookup readiness invoice", error));
            }
        }
        wait_before_retry(deadline, service, &observation).await?;
    }
}

fn is_transient_routing_failure(error: &LndError) -> bool {
    matches!(
        error,
        LndError::PaymentFailed { reason, .. }
            if matches!(reason.as_ref(), "no route" | "insufficient balance")
    )
}

async fn lnd_operation<T>(
    deadline: &Deadline,
    service: &'static str,
    operation: &'static str,
    future: impl Future<Output = Result<T, LndError>>,
) -> Result<T, FixtureError> {
    let result = deadline.run(service, operation, future).await?;
    result.map_err(|error| lightning_bootstrap_error(operation, error))
}

fn flatten_lnd_operation<T>(
    operation: &'static str,
    result: Result<Result<T, LndError>, FixtureError>,
) -> Result<T, FixtureError> {
    result?.map_err(|error| lightning_bootstrap_error(operation, error))
}

fn is_already_connected(error: &LndError) -> bool {
    matches!(
        error,
        LndError::Status { detail, .. } if detail.as_ref() == "gRPC status AlreadyExists"
    )
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
            Ok(Err(error)) if error.is_transient_file_unavailable() => {
                observation = redacted_tail(&format!("LND TLS certificate is not ready: {error}"));
            }
            Ok(Err(error)) => return Err(runtime_error(service, error)),
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
        let (alice_info, bob_info, bitcoin_height) = {
            let alice_probe = deadline.run(service, &observation, connector.get_info(alice));
            let bob_probe = deadline.run("lnd-bob", &observation, connector.get_info(bob));
            let bitcoin_probe = deadline.run("bitcoind", &observation, bitcoin.block_height());
            tokio::pin!(alice_probe, bob_probe, bitcoin_probe);
            let mut alice_info = None;
            let mut bob_info = None;
            let mut bitcoin_height = None;

            loop {
                tokio::select! {
                    biased;
                    result = &mut alice_probe, if alice_info.is_none() => {
                        match result {
                            Ok(Err(error)) if !is_transient_lnd_readiness(&error) => {
                                return Err(lightning_bootstrap_error(
                                    "query Alice synchronization",
                                    error,
                                ));
                            }
                            result => alice_info = Some(result),
                        }
                    }
                    result = &mut bob_probe, if bob_info.is_none() => {
                        match result {
                            Ok(Err(error)) if !is_transient_lnd_readiness(&error) => {
                                return Err(lightning_bootstrap_error(
                                    "query Bob synchronization",
                                    error,
                                ));
                            }
                            result => bob_info = Some(result),
                        }
                    }
                    result = &mut bitcoin_probe, if bitcoin_height.is_none() => {
                        bitcoin_height = Some(result);
                    }
                }

                match (alice_info, bob_info, bitcoin_height) {
                    (Some(alice), Some(bob), Some(bitcoin)) => {
                        break (alice, bob, bitcoin);
                    }
                    (alice, bob, bitcoin) => {
                        alice_info = alice;
                        bob_info = bob;
                        bitcoin_height = bitcoin;
                    }
                }
            }
        };

        match (alice_info, bob_info, bitcoin_height) {
            (Ok(Ok(alice)), Ok(Ok(bob)), Ok(Ok(bitcoin_height)))
                if chain_synchronized(alice, bitcoin_height)
                    && chain_synchronized(bob, bitcoin_height) =>
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
                service = if !chain_synchronized(alice, bitcoin_height) {
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

        wait_before_retry(deadline, service, &observation).await?;
    }
}

fn is_transient_lnd_readiness(error: &LndError) -> bool {
    match error {
        LndError::Transport { .. } | LndError::Timeout { .. } => true,
        LndError::Status { detail, .. } => matches!(
            detail.as_ref(),
            "gRPC status Unavailable"
                | "gRPC status DeadlineExceeded"
                | "gRPC status ResourceExhausted"
                | "gRPC status Aborted"
                | "gRPC status Unknown error"
        ),
        LndError::InvalidRequest { .. }
        | LndError::CredentialRead { .. }
        | LndError::Authentication { .. }
        | LndError::InvalidResponse { .. }
        | LndError::PaymentFailed { .. }
        | LndError::OutcomeUnknown { .. } => false,
        _ => false,
    }
}

fn chain_synchronized(status: LndSyncStatus, bitcoin_height: u64) -> bool {
    status.network_is_regtest
        && status.synced_to_chain
        && u64::from(status.block_height) == bitcoin_height
}

fn graph_synchronized(status: LndSyncStatus, bitcoin_height: u64) -> bool {
    chain_synchronized(status, bitcoin_height) && status.synced_to_graph
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
        initialize_lnd_client(
            connector,
            &alice_config,
            &alice_password,
            "lnd-alice",
            "initializing Alice wallet",
            deadline,
        ),
        initialize_lnd_client(
            connector,
            &bob_config,
            &bob_password,
            "lnd-bob",
            "initializing Bob wallet",
            deadline,
        )
    );
    Ok((alice?, bob?))
}

async fn initialize_lnd_client<C: LndNodeConnector>(
    connector: &C,
    config: &LndBootstrapConfig,
    password: &[u8],
    service: &'static str,
    operation: &'static str,
    deadline: &Deadline,
) -> Result<C::Client, FixtureError> {
    let mut observation = operation.to_owned();

    loop {
        match deadline
            .run(
                service,
                &observation,
                connector.initialize(config, password),
            )
            .await?
        {
            Ok(client) => return Ok(client),
            Err(source) if is_transient_seed_generation(&source) => {
                observation = redacted_tail(&source.to_string());
            }
            Err(source) => return Err(lightning_bootstrap_error(operation, source)),
        }

        wait_before_retry(deadline, service, &observation).await?;
    }
}

fn is_transient_seed_generation(error: &LndError) -> bool {
    match error {
        LndError::Transport { operation, .. } | LndError::Timeout { operation, .. } => {
            operation.as_ref() == "generate wallet seed"
        }
        LndError::Status { operation, detail } => {
            operation.as_ref() == "generate wallet seed"
                && matches!(
                    detail.as_ref(),
                    "gRPC status Unavailable"
                        | "gRPC status DeadlineExceeded"
                        | "gRPC status ResourceExhausted"
                        | "gRPC status Aborted"
                        | "gRPC status Unknown error"
                )
        }
        LndError::InvalidRequest { .. }
        | LndError::CredentialRead { .. }
        | LndError::Authentication { .. }
        | LndError::InvalidResponse { .. }
        | LndError::PaymentFailed { .. }
        | LndError::OutcomeUnknown { .. }
        | _ => false,
    }
}

async fn attach_lnd_logs<E: ContainerEngine>(
    startup: &mut Startup<E>,
    deadline: &Deadline,
    alice_id_or_name: &str,
    bob_id_or_name: &str,
    error: FixtureError,
) -> FixtureError {
    let with_bob = attach_container_log(startup, deadline, "lnd-bob", bob_id_or_name, error).await;
    attach_container_log(startup, deadline, "lnd-alice", alice_id_or_name, with_bob).await
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
        collections::{BTreeSet, HashMap},
        io,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use bitcoin::{
        OutPoint, Txid,
        hashes::{Hash as _, sha256},
        secp256k1::{PublicKey, SecretKey},
    };
    use nigiri_rs_lnd::{
        InvoiceState, LndBootstrapConfig, LndError, Millisats, OpenChannelRequest, PaymentState,
        PeerAddress, Sats,
    };
    use tokio::sync::{Barrier, Notify};

    use super::{
        BitcoinTip, ChannelAllocation, ChannelReadiness, LndHandles, LndNodeConnector, LndPair,
        LndPairEnvironment, LndSyncStatus, PaymentReadiness, initialize_lnd_clients,
        open_and_confirm_channel, prove_reverse_readiness_with_retry, start_lnd_nodes_under,
        wait_for_lnd_sync,
    };
    use crate::{
        ContainerImage, FixtureError,
        deadline::Deadline,
        lnd::TLS_CERT_PATH,
        runtime::{
            ContainerEngine, ContainerSpec, EngineError, EngineResult, lnd_spec,
            supervise_for_coordinator,
        },
    };

    #[derive(Clone, Copy)]
    enum CertificateRead {
        Valid,
        MissingOnce,
        Oversized,
        Malformed,
        Blocked,
    }

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

    // Catches capacity arithmetic, LND signed-wire, and Bitcoin MoneyRange checks being deferred
    // until after the backing fixture has crossed its Docker connection boundary.
    #[tokio::test]
    async fn numeric_channel_inputs_fail_before_the_backing_environment_connects() {
        let docker_connects = Arc::new(AtomicUsize::new(0));
        let environment = CountingPreflightEnvironment {
            docker_connects: Arc::clone(&docker_connects),
        };
        let rejected = [
            LndPair::builder().channel_capacity(Sats::new(18_446_744_073_709_551_615)),
            LndPair::builder().channel_capacity(Sats::new(9_223_372_036_854_775_808)),
            LndPair::builder().channel_capacity(Sats::new(18_446_744_073_709_351_616)),
            LndPair::builder().channel_capacity(Sats::new(2_099_999_999_800_001)),
        ];

        for builder in rejected {
            let error = match builder.start_with_environment(environment.clone()).await {
                Err(error) => error,
                Ok(_) => panic!("invalid numeric inputs must fail in builder preflight"),
            };
            assert!(matches!(error, FixtureError::InvalidConfiguration { .. }));
        }

        assert_eq!(
            docker_connects.load(Ordering::SeqCst),
            0,
            "numeric caller errors must not cross the backing Docker boundary"
        );
    }

    // Unlike image and amount checks, this exercises the exact absolute-Instant representation
    // boundary whose failure used to be deferred until wallet initialization after Docker work.
    #[test]
    fn an_unrepresentable_deadline_is_rejected_by_builder_validation() {
        let error = LndPair::builder()
            .startup_timeout(Duration::MAX)
            .validate()
            .expect_err("an unrepresentable absolute deadline must fail before Docker");

        assert!(matches!(error, FixtureError::InvalidConfiguration { .. }));
    }

    #[derive(Clone)]
    struct FakeEngine {
        starts_together: Arc<Barrier>,
        certificate_read: CertificateRead,
        certificate_reads: Arc<AtomicUsize>,
        read_entered: Arc<Notify>,
        block_logs: bool,
        log_entered: Arc<Notify>,
        block_bob_start: bool,
        alice_started: Arc<Notify>,
        removal_delay: Duration,
        specs: Arc<Mutex<Vec<ContainerSpec>>>,
        removed: Arc<Mutex<Vec<String>>>,
    }

    impl FakeEngine {
        fn new() -> Self {
            Self {
                starts_together: Arc::new(Barrier::new(2)),
                certificate_read: CertificateRead::Valid,
                certificate_reads: Arc::new(AtomicUsize::new(0)),
                read_entered: Arc::new(Notify::new()),
                block_logs: false,
                log_entered: Arc::new(Notify::new()),
                block_bob_start: false,
                alice_started: Arc::new(Notify::new()),
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

        async fn start_container(&self, id: &str) -> EngineResult<()> {
            if id.contains("alice") {
                self.alice_started.notify_one();
            } else if self.block_bob_start && id.contains("bob") {
                return std::future::pending().await;
            }
            Ok(())
        }

        async fn mapped_port(&self, id: &str, container_port: u16) -> EngineResult<u16> {
            assert_eq!(container_port, 10_009);
            Ok(if id.contains("alice") { 31_009 } else { 32_009 })
        }

        async fn logs(&self, id: &str) -> EngineResult<String> {
            self.log_entered.notify_one();
            if self.block_logs {
                return std::future::pending().await;
            }
            Ok(if id.contains("alice") {
                "--bitcoind.rpcpass=alice-rpc-secret\n\
                 wallet-password=raw-password\n\
                 cipher_seed_mnemonic=ability absent absorb abstract secret-mnemonic-tail\n\
                 macaroon_hex=deadbeef"
                    .to_owned()
            } else {
                "wallet_password_hex=70617373 macaroon=raw-macaroon\n\
                 -----BEGIN PRIVATE KEY-----\n\
                 bob-pem-private-secret\n\
                 -----END PRIVATE KEY-----"
                    .to_owned()
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
            let read = self.certificate_reads.fetch_add(1, Ordering::SeqCst);
            match self.certificate_read {
                CertificateRead::MissingOnce if read == 0 => {
                    return Err(EngineError::new(
                        "read container file",
                        io::Error::new(io::ErrorKind::NotFound, "certificate not created yet"),
                    ));
                }
                CertificateRead::Oversized => return Ok(vec![b'x'; max_bytes + 1]),
                CertificateRead::Malformed => {
                    return Err(EngineError::new(
                        "read container file",
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "container file archive metadata mismatch",
                        ),
                    ));
                }
                CertificateRead::Blocked => return std::future::pending().await,
                CertificateRead::Valid | CertificateRead::MissingOnce => {}
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

    #[derive(Clone, Copy)]
    enum InfoFailure {
        Authentication,
        InvalidResponse,
        UnavailableOnce,
        UnknownOnce,
        GraphUnsynced,
        WrongNetwork,
        WrongHeight,
        AliceAuthenticationBobPending,
        BobInvalidResponseAlicePending,
    }

    #[derive(Clone)]
    struct FakeConnector {
        initialized: Arc<Mutex<Vec<InitializationRecord>>>,
        fail_initialization: bool,
        transient_initializations_remaining: Arc<AtomicUsize>,
        info_failure: Option<InfoFailure>,
        info_calls: Arc<AtomicUsize>,
        invoice_calls: Arc<AtomicUsize>,
        payment_attempts: Arc<AtomicUsize>,
        payment_failures_remaining: Arc<AtomicUsize>,
        payment_failure_reason: Option<&'static str>,
        open_after_mining: Option<Arc<Notify>>,
        open_channel_point: OutPoint,
    }

    impl FakeConnector {
        fn succeeding() -> Self {
            Self {
                initialized: Arc::new(Mutex::new(Vec::new())),
                fail_initialization: false,
                transient_initializations_remaining: Arc::new(AtomicUsize::new(0)),
                info_failure: None,
                info_calls: Arc::new(AtomicUsize::new(0)),
                invoice_calls: Arc::new(AtomicUsize::new(0)),
                payment_attempts: Arc::new(AtomicUsize::new(0)),
                payment_failures_remaining: Arc::new(AtomicUsize::new(0)),
                payment_failure_reason: None,
                open_after_mining: None,
                open_channel_point: fake_channel_point(),
            }
        }
    }

    // GenSeed is read-only, so an LND transport race before it returns can be retried safely. This
    // catches both passwords being regenerated and a broad retry that would also repeat InitWallet.
    #[tokio::test]
    async fn transient_seed_generation_is_retried_with_the_original_passwords() {
        let connector = FakeConnector {
            transient_initializations_remaining: Arc::new(AtomicUsize::new(2)),
            ..FakeConnector::succeeding()
        };
        let alice_config = LndBootstrapConfig {
            endpoint: "https://127.0.0.1:31009".parse().unwrap(),
            tls_certificate: b"alice certificate".to_vec(),
            timeout: Duration::from_secs(1),
        };
        let bob_config = LndBootstrapConfig {
            endpoint: "https://127.0.0.1:32009".parse().unwrap(),
            tls_certificate: b"bob certificate".to_vec(),
            timeout: Duration::from_secs(1),
        };
        let deadline = Deadline::new(Duration::from_secs(1)).unwrap();

        initialize_lnd_clients(&connector, alice_config, bob_config, &deadline)
            .await
            .expect("known transient GenSeed statuses converge under the shared deadline");

        let initialized = connector.initialized.lock().unwrap();
        assert_eq!(initialized.len(), 4);
        for endpoint in ["https://127.0.0.1:31009/", "https://127.0.0.1:32009/"] {
            let passwords = initialized
                .iter()
                .filter(|record| record.endpoint == endpoint)
                .map(|record| record.password.as_slice())
                .collect::<Vec<_>>();
            assert!(!passwords.is_empty());
            assert!(passwords.windows(2).all(|pair| pair[0] == pair[1]));
        }
    }

    impl LndNodeConnector for FakeConnector {
        type Client = FakeClient;
        type Invoice = sha256::Hash;

        async fn initialize(
            &self,
            config: &LndBootstrapConfig,
            password: &[u8],
        ) -> Result<Self::Client, LndError> {
            let endpoint = config.endpoint.to_string();
            self.initialized
                .lock()
                .expect("fake initialization records are never poisoned")
                .push(InitializationRecord {
                    endpoint: endpoint.clone(),
                    certificate: config.tls_certificate.clone(),
                    password: password.to_vec(),
                });
            if self
                .transient_initializations_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                Err(LndError::Status {
                    operation: "generate wallet seed".into(),
                    detail: "gRPC status Unknown error".into(),
                })
            } else if self.fail_initialization {
                Err(LndError::Status {
                    operation: "initialize wallet".into(),
                    detail: "fake rejected initialization".into(),
                })
            } else {
                Ok(FakeClient { endpoint })
            }
        }

        async fn get_info(&self, client: &Self::Client) -> Result<LndSyncStatus, LndError> {
            let call = self.info_calls.fetch_add(1, Ordering::SeqCst);
            match self.info_failure {
                Some(InfoFailure::Authentication) => {
                    return Err(LndError::Authentication {
                        operation: "get node information".into(),
                        detail: "fake credentials rejected".into(),
                    });
                }
                Some(InfoFailure::InvalidResponse) => {
                    return Err(LndError::InvalidResponse {
                        operation: "get node information".into(),
                        detail: "fake malformed chain response".into(),
                        identifier: None,
                    });
                }
                Some(InfoFailure::UnavailableOnce) if call < 2 => {
                    return Err(LndError::Status {
                        operation: "get node information".into(),
                        detail: "gRPC status Unavailable".into(),
                    });
                }
                Some(InfoFailure::UnknownOnce) if call < 2 => {
                    return Err(LndError::Status {
                        operation: "get node information".into(),
                        detail: "gRPC status Unknown error".into(),
                    });
                }
                Some(InfoFailure::AliceAuthenticationBobPending) => {
                    if client.endpoint.contains("31009") {
                        return Err(LndError::Authentication {
                            operation: "get node information".into(),
                            detail: "Alice credentials rejected".into(),
                        });
                    }
                    return std::future::pending().await;
                }
                Some(InfoFailure::BobInvalidResponseAlicePending) => {
                    if client.endpoint.contains("32009") {
                        return Err(LndError::InvalidResponse {
                            operation: "get node information".into(),
                            detail: "Bob returned malformed chain state".into(),
                            identifier: None,
                        });
                    }
                    return std::future::pending().await;
                }
                None
                | Some(
                    InfoFailure::UnavailableOnce
                    | InfoFailure::UnknownOnce
                    | InfoFailure::GraphUnsynced
                    | InfoFailure::WrongNetwork
                    | InfoFailure::WrongHeight,
                ) => {}
            }
            Ok(LndSyncStatus {
                public_key: fake_public_key(client),
                block_height: if matches!(self.info_failure, Some(InfoFailure::WrongHeight)) {
                    100
                } else {
                    101
                },
                network_is_regtest: !matches!(self.info_failure, Some(InfoFailure::WrongNetwork)),
                synced_to_chain: true,
                synced_to_graph: !matches!(self.info_failure, Some(InfoFailure::GraphUnsynced)),
            })
        }

        async fn new_address(&self, _client: &Self::Client) -> Result<String, LndError> {
            Ok("bcrt1qfakealiceaddress".to_owned())
        }

        async fn confirmed_balance(&self, _client: &Self::Client) -> Result<Sats, LndError> {
            Ok(Sats::new(2_200_000))
        }

        async fn peer_connected(
            &self,
            _client: &Self::Client,
            _public_key: PublicKey,
        ) -> Result<bool, LndError> {
            Ok(true)
        }

        async fn connect_peer(
            &self,
            _client: &Self::Client,
            _peer: PeerAddress,
        ) -> Result<(), LndError> {
            Ok(())
        }

        async fn open_channel(
            &self,
            _client: &Self::Client,
            _request: OpenChannelRequest,
        ) -> Result<OutPoint, LndError> {
            if let Some(mined) = &self.open_after_mining {
                mined.notified().await;
            }
            Ok(self.open_channel_point)
        }

        async fn channel_readiness(
            &self,
            _client: &Self::Client,
            channel_point: OutPoint,
            _remote_public_key: PublicKey,
        ) -> Result<Option<ChannelReadiness>, LndError> {
            Ok(
                (channel_point == fake_channel_point()).then_some(ChannelReadiness {
                    active: true,
                    local_balance: Millisats::new(100_000_000),
                }),
            )
        }

        async fn create_readiness_invoice(
            &self,
            _client: &Self::Client,
        ) -> Result<(Self::Invoice, sha256::Hash), LndError> {
            let sequence = self.invoice_calls.fetch_add(1, Ordering::SeqCst);
            let hash = sha256::Hash::hash(&sequence.to_le_bytes());
            Ok((hash, hash))
        }

        async fn pay_readiness_invoice(
            &self,
            _client: &Self::Client,
            invoice: &Self::Invoice,
        ) -> Result<PaymentReadiness, LndError> {
            self.payment_attempts.fetch_add(1, Ordering::SeqCst);
            if self
                .payment_failures_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(LndError::PaymentFailed {
                    payment_hash: *invoice,
                    reason: self
                        .payment_failure_reason
                        .unwrap_or("insufficient balance")
                        .into(),
                });
            }
            Ok(PaymentReadiness {
                payment_hash: *invoice,
                state: PaymentState::Succeeded,
            })
        }

        async fn invoice_state(
            &self,
            _client: &Self::Client,
            _payment_hash: sha256::Hash,
        ) -> Result<InvoiceState, LndError> {
            Ok(InvoiceState::Settled)
        }
    }

    fn fake_public_key(client: &FakeClient) -> PublicKey {
        let byte = if client.endpoint.contains("31009") {
            1
        } else {
            2
        };
        let secret = SecretKey::from_slice(&[byte; 32]).unwrap();
        secret.public_key(&bitcoin::secp256k1::Secp256k1::new())
    }

    fn fake_channel_point() -> OutPoint {
        OutPoint::new(Txid::from_byte_array([3; 32]), 0)
    }

    const fn default_channel_allocation() -> ChannelAllocation {
        ChannelAllocation {
            capacity: Sats::new(2_000_000),
            push: Sats::new(1_000_000),
            funding_amount: Sats::new(2_200_000),
        }
    }

    #[tokio::test]
    async fn pending_channel_mines_exactly_six_blocks_for_its_observed_funding_txid() {
        let observed_txid = Txid::from_byte_array([4; 32]);
        let channel_point = OutPoint::new(observed_txid, 1);
        let mined = Arc::new(Notify::new());
        let mined_blocks = Arc::new(AtomicUsize::new(0));
        let mining_calls = Arc::new(AtomicUsize::new(0));
        let connector = FakeConnector {
            open_after_mining: Some(Arc::clone(&mined)),
            open_channel_point: channel_point,
            ..FakeConnector::succeeding()
        };
        let bitcoin = FundingTransitionBitcoinTip {
            observed_txid,
            mempool_calls: Arc::new(AtomicUsize::new(0)),
            mined_blocks: Arc::clone(&mined_blocks),
            mining_calls: Arc::clone(&mining_calls),
            mined,
        };
        let alice = FakeClient {
            endpoint: "https://127.0.0.1:31009/".to_owned(),
        };
        let bob = FakeClient {
            endpoint: "https://127.0.0.1:32009/".to_owned(),
        };
        let request = OpenChannelRequest::new(
            fake_public_key(&bob),
            Sats::new(2_000_000),
            Sats::new(1_000_000),
        )
        .unwrap();
        let deadline = Deadline::new(Duration::from_secs(1)).unwrap();

        let opened = open_and_confirm_channel(&connector, &alice, &bitcoin, request, &deadline)
            .await
            .expect("the observed funding transaction must open after six blocks");

        assert_eq!(opened, channel_point);
        assert_eq!(mined_blocks.load(Ordering::SeqCst), 6);
        assert_eq!(mining_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn opened_channel_rejects_a_txid_that_did_not_trigger_confirmation_mining() {
        let observed_txid = Txid::from_byte_array([4; 32]);
        let mined = Arc::new(Notify::new());
        let mined_blocks = Arc::new(AtomicUsize::new(0));
        let mining_calls = Arc::new(AtomicUsize::new(0));
        let connector = FakeConnector {
            open_after_mining: Some(Arc::clone(&mined)),
            open_channel_point: OutPoint::new(Txid::from_byte_array([5; 32]), 1),
            ..FakeConnector::succeeding()
        };
        let bitcoin = FundingTransitionBitcoinTip {
            observed_txid,
            mempool_calls: Arc::new(AtomicUsize::new(0)),
            mined_blocks: Arc::clone(&mined_blocks),
            mining_calls: Arc::clone(&mining_calls),
            mined,
        };
        let alice = FakeClient {
            endpoint: "https://127.0.0.1:31009/".to_owned(),
        };
        let bob = FakeClient {
            endpoint: "https://127.0.0.1:32009/".to_owned(),
        };
        let request = OpenChannelRequest::new(
            fake_public_key(&bob),
            Sats::new(2_000_000),
            Sats::new(1_000_000),
        )
        .unwrap();
        let deadline = Deadline::new(Duration::from_secs(1)).unwrap();

        let error = open_and_confirm_channel(&connector, &alice, &bitcoin, request, &deadline)
            .await
            .expect_err("the final channel point must belong to the observed mempool trigger set");

        assert!(matches!(error, FixtureError::Bootstrap { .. }));
        assert_eq!(mined_blocks.load(Ordering::SeqCst), 6);
        assert_eq!(mining_calls.load(Ordering::SeqCst), 1);
    }

    #[derive(Clone)]
    struct FakeBitcoinTip {
        mempool_calls: Arc<AtomicUsize>,
    }

    impl FakeBitcoinTip {
        fn new() -> Self {
            Self {
                mempool_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl BitcoinTip for FakeBitcoinTip {
        async fn block_height(&self) -> Result<u64, FixtureError> {
            Ok(101)
        }

        async fn fund_address(&self, _address: &str, _amount: Sats) -> Result<(), FixtureError> {
            Ok(())
        }

        async fn mempool_transactions(&self) -> Result<BTreeSet<Txid>, FixtureError> {
            if self.mempool_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(BTreeSet::new())
            } else {
                Ok(BTreeSet::from([fake_channel_point().txid]))
            }
        }

        async fn mine_blocks(&self, _blocks: u64) -> Result<(), FixtureError> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FundingTransitionBitcoinTip {
        observed_txid: Txid,
        mempool_calls: Arc<AtomicUsize>,
        mined_blocks: Arc<AtomicUsize>,
        mining_calls: Arc<AtomicUsize>,
        mined: Arc<Notify>,
    }

    impl BitcoinTip for FundingTransitionBitcoinTip {
        async fn block_height(&self) -> Result<u64, FixtureError> {
            Ok(101)
        }

        async fn fund_address(&self, _address: &str, _amount: Sats) -> Result<(), FixtureError> {
            Ok(())
        }

        async fn mempool_transactions(&self) -> Result<BTreeSet<Txid>, FixtureError> {
            let existing = Txid::from_byte_array([8; 32]);
            if self.mempool_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(BTreeSet::from([existing]))
            } else {
                Ok(BTreeSet::from([existing, self.observed_txid]))
            }
        }

        async fn mine_blocks(&self, blocks: u64) -> Result<(), FixtureError> {
            self.mining_calls.fetch_add(1, Ordering::SeqCst);
            self.mined_blocks
                .fetch_add(usize::try_from(blocks).unwrap(), Ordering::SeqCst);
            self.mined.notify_one();
            Ok(())
        }
    }

    #[derive(Clone)]
    struct PendingBitcoinTip;

    impl BitcoinTip for PendingBitcoinTip {
        async fn block_height(&self) -> Result<u64, FixtureError> {
            std::future::pending().await
        }

        async fn fund_address(&self, _address: &str, _amount: Sats) -> Result<(), FixtureError> {
            std::future::pending().await
        }

        async fn mempool_transactions(&self) -> Result<BTreeSet<Txid>, FixtureError> {
            std::future::pending().await
        }

        async fn mine_blocks(&self, _blocks: u64) -> Result<(), FixtureError> {
            std::future::pending().await
        }
    }

    struct FakeBitcoinStack {
        removed: Arc<Mutex<Vec<String>>>,
        removal_delay: Duration,
        active: bool,
    }

    #[derive(Clone)]
    struct CountingPreflightEnvironment {
        docker_connects: Arc<AtomicUsize>,
    }

    impl LndPairEnvironment for CountingPreflightEnvironment {
        type BitcoinStack = FakeBitcoinStack;
        type Engine = FakeEngine;
        type Connector = FakeConnector;
        type BitcoinTip = FakeBitcoinTip;

        async fn start_bitcoin_for_coordinator(
            &self,
            _bitcoind_image: ContainerImage,
            _electrs_image: ContainerImage,
            _deadline: &Deadline,
        ) -> Result<Self::BitcoinStack, FixtureError> {
            self.docker_connects.fetch_add(1, Ordering::SeqCst);
            Err(FixtureError::InvalidConfiguration {
                detail: "counting environment reached Docker".to_owned(),
            })
        }

        fn engine(&self, _bitcoin: &Self::BitcoinStack) -> Self::Engine {
            panic!("invalid preflight must not request a container engine")
        }

        fn network_name(&self, _bitcoin: &Self::BitcoinStack) -> String {
            panic!("invalid preflight must not request a network")
        }

        fn node_container_name(&self, _bitcoin: &Self::BitcoinStack) -> String {
            panic!("invalid preflight must not request a node container")
        }

        fn bitcoin_tip(&self, _bitcoin: &Self::BitcoinStack) -> Self::BitcoinTip {
            panic!("invalid preflight must not request a Bitcoin client")
        }

        fn connector(&self) -> Self::Connector {
            panic!("invalid preflight must not request an LND connector")
        }

        async fn attach_inner_logs(
            &self,
            _bitcoin: &Self::BitcoinStack,
            _deadline: &Deadline,
            _error: FixtureError,
        ) -> FixtureError {
            panic!("invalid preflight must not attach runtime logs")
        }

        async fn shutdown_bitcoin(&self, _bitcoin: Self::BitcoinStack) -> Result<(), FixtureError> {
            panic!("invalid preflight must not own backing resources")
        }
    }

    impl Drop for FakeBitcoinStack {
        fn drop(&mut self) {
            if !self.active {
                return;
            }
            for resource in ["electrs", "bitcoind"] {
                std::thread::sleep(self.removal_delay);
                self.removed.lock().unwrap().push(resource.to_owned());
            }
        }
    }

    #[derive(Clone)]
    struct FakeLndPairEnvironment {
        engine: FakeEngine,
        connector: FakeConnector,
        backing_removal_delay: Duration,
    }

    impl LndPairEnvironment for FakeLndPairEnvironment {
        type BitcoinStack = FakeBitcoinStack;
        type Engine = FakeEngine;
        type Connector = FakeConnector;
        type BitcoinTip = FakeBitcoinTip;

        async fn start_bitcoin_for_coordinator(
            &self,
            _bitcoind_image: ContainerImage,
            _electrs_image: ContainerImage,
            _deadline: &Deadline,
        ) -> Result<Self::BitcoinStack, FixtureError> {
            Ok(FakeBitcoinStack {
                removed: Arc::clone(&self.engine.removed),
                removal_delay: self.backing_removal_delay,
                active: true,
            })
        }

        fn engine(&self, _bitcoin: &Self::BitcoinStack) -> Self::Engine {
            self.engine.clone()
        }

        fn network_name(&self, _bitcoin: &Self::BitcoinStack) -> String {
            "shared-network".to_owned()
        }

        fn node_container_name(&self, _bitcoin: &Self::BitcoinStack) -> String {
            "private-bitcoind".to_owned()
        }

        fn bitcoin_tip(&self, _bitcoin: &Self::BitcoinStack) -> Self::BitcoinTip {
            FakeBitcoinTip::new()
        }

        fn connector(&self) -> Self::Connector {
            self.connector.clone()
        }

        async fn attach_inner_logs(
            &self,
            _bitcoin: &Self::BitcoinStack,
            _deadline: &Deadline,
            error: FixtureError,
        ) -> FixtureError {
            error
        }

        async fn shutdown_bitcoin(
            &self,
            mut bitcoin: Self::BitcoinStack,
        ) -> Result<(), FixtureError> {
            bitcoin.active = false;
            for resource in ["electrs", "bitcoind"] {
                tokio::time::sleep(bitcoin.removal_delay).await;
                bitcoin.removed.lock().unwrap().push(resource.to_owned());
            }
            Ok(())
        }
    }

    #[derive(Clone)]
    struct BackingStartupEnvironment {
        engine: FakeEngine,
        started: Arc<Notify>,
    }

    impl LndPairEnvironment for BackingStartupEnvironment {
        type BitcoinStack = FakeBitcoinStack;
        type Engine = FakeEngine;
        type Connector = FakeConnector;
        type BitcoinTip = FakeBitcoinTip;

        async fn start_bitcoin_for_coordinator(
            &self,
            _bitcoind_image: ContainerImage,
            _electrs_image: ContainerImage,
            _deadline: &Deadline,
        ) -> Result<Self::BitcoinStack, FixtureError> {
            let bitcoind = lnd_spec(
                ContainerImage::lnd_default(),
                "backing-network".to_owned(),
                "bitcoind".to_owned(),
                "bitcoind",
                "127.0.0.1",
            )?;
            let electrs = lnd_spec(
                ContainerImage::lnd_default(),
                "backing-network".to_owned(),
                "electrs".to_owned(),
                "bitcoind",
                "127.0.0.1",
            )?;
            let started = Arc::clone(&self.started);
            let supervised =
                supervise_for_coordinator(self.engine.clone(), move |mut startup| async move {
                    let (bitcoind, electrs) = startup.start_container_pair(bitcoind, electrs).await;
                    bitcoind?;
                    electrs?;
                    started.notify_one();
                    std::future::pending::<Result<(), FixtureError>>().await
                });
            let _ = supervised.await?;
            unreachable!("the backing-startup cancellation test never completes startup")
        }

        fn engine(&self, _bitcoin: &Self::BitcoinStack) -> Self::Engine {
            panic!("the cancellation test never finishes backing startup")
        }

        fn network_name(&self, _bitcoin: &Self::BitcoinStack) -> String {
            panic!("the cancellation test never finishes backing startup")
        }

        fn node_container_name(&self, _bitcoin: &Self::BitcoinStack) -> String {
            panic!("the cancellation test never finishes backing startup")
        }

        fn bitcoin_tip(&self, _bitcoin: &Self::BitcoinStack) -> Self::BitcoinTip {
            panic!("the cancellation test never finishes backing startup")
        }

        fn connector(&self) -> Self::Connector {
            panic!("the cancellation test never finishes backing startup")
        }

        async fn attach_inner_logs(
            &self,
            _bitcoin: &Self::BitcoinStack,
            _deadline: &Deadline,
            _error: FixtureError,
        ) -> FixtureError {
            panic!("the cancellation test never finishes backing startup")
        }

        async fn shutdown_bitcoin(&self, _bitcoin: Self::BitcoinStack) -> Result<(), FixtureError> {
            panic!("the cancellation test never finishes backing startup")
        }
    }

    // Catches public builder cancellation dropping the backing fixture on the caller while LND
    // cleanup is still detached. The one composite owner must return at the shared deadline and
    // eventually remove Bob, Alice, Electrs, then bitcoind without concurrent ledgers.
    #[tokio::test]
    async fn builder_start_cancellation_is_bounded_and_keeps_cross_stack_cleanup_order() {
        let mut engine = FakeEngine::new();
        engine.certificate_read = CertificateRead::Blocked;
        engine.removal_delay = Duration::from_millis(250);
        let read_entered = Arc::clone(&engine.read_entered);
        let removed = Arc::clone(&engine.removed);
        let environment = FakeLndPairEnvironment {
            engine,
            connector: FakeConnector::succeeding(),
            backing_removal_delay: Duration::from_millis(250),
        };

        let task = tokio::spawn(async move {
            LndPair::builder()
                .startup_timeout(Duration::from_millis(30))
                .start_with_environment(environment)
                .await
        });

        read_entered.notified().await;
        let cancelled_at = std::time::Instant::now();
        task.abort();
        let cancellation = match task.await {
            Err(error) => error,
            Ok(_) => panic!("aborting public builder startup must cancel it"),
        };
        assert!(cancellation.is_cancelled());
        assert!(
            cancelled_at.elapsed() < Duration::from_millis(150),
            "public builder cancellation outlived its 30ms deadline: {:?}",
            cancelled_at.elapsed()
        );

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if removed.lock().unwrap().len() == 4 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the detached composite coordinator must eventually remove all four containers");
        let removed = removed.lock().unwrap().clone();
        assert!(removed[0].contains("bob"), "{removed:?}");
        assert!(removed[1].contains("alice"), "{removed:?}");
        assert_eq!(&removed[2..], ["electrs", "bitcoind"], "{removed:?}");
    }

    // Catches backing startup using an independently deadline-detachable supervisor after the
    // public coordinator exists. Cancellation must detach only the coordinator while that owner
    // waits for nested Electrs/bitcoind cleanup to finish in dependency order.
    #[tokio::test]
    async fn builder_backing_startup_cancellation_has_one_bounded_owner() {
        let mut engine = FakeEngine::new();
        engine.removal_delay = Duration::from_millis(300);
        let removed = Arc::clone(&engine.removed);
        let started = Arc::new(Notify::new());
        let environment = BackingStartupEnvironment {
            engine,
            started: Arc::clone(&started),
        };
        let task = tokio::spawn(async move {
            LndPair::builder()
                .startup_timeout(Duration::from_millis(100))
                .start_with_environment(environment)
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("both backing resources must start before cancellation");
        let cancelled_at = std::time::Instant::now();
        task.abort();
        let cancellation = match task.await {
            Err(error) => error,
            Ok(_) => panic!("aborting during backing startup must cancel the public builder"),
        };
        assert!(cancellation.is_cancelled());
        assert!(
            cancelled_at.elapsed() < Duration::from_millis(250),
            "backing cleanup held the public caller past its deadline: {:?}",
            cancelled_at.elapsed()
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if removed.lock().unwrap().len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the detached coordinator must finish both backing removals");
        assert_eq!(
            *removed.lock().unwrap(),
            ["electrs-id", "bitcoind-id"],
            "each backing resource must be removed exactly once"
        );
    }

    // Catches a successful composite payload sitting unacknowledged in the result channel. If
    // receiver cancellation drops its armed runtimes on the caller, abort waits for all four slow
    // removals instead of returning within the shared deadline.
    #[tokio::test]
    async fn builder_success_handoff_cancellation_is_bounded_and_returns_ownership_once() {
        let mut engine = FakeEngine::new();
        engine.removal_delay = Duration::from_millis(300);
        let removed = Arc::clone(&engine.removed);
        let environment = FakeLndPairEnvironment {
            engine,
            connector: FakeConnector::succeeding(),
            backing_removal_delay: Duration::from_millis(300),
        };
        let handoff_published = Arc::new(Notify::new());
        let observed_publication = Arc::clone(&handoff_published);

        let task = tokio::spawn(async move {
            LndPair::builder()
                .startup_timeout(Duration::from_millis(100))
                .start_with_environment_before_handoff_ack(environment, async move {
                    handoff_published.notify_one();
                    std::future::pending().await
                })
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), observed_publication.notified())
            .await
            .expect("the coordinator must publish successful ownership before the deadline");
        let cancelled_at = std::time::Instant::now();
        task.abort();
        let cancellation = match task.await {
            Err(error) => error,
            Ok(_) => panic!("aborting the unacknowledged success handoff must cancel it"),
        };
        assert!(cancellation.is_cancelled());
        assert!(
            cancelled_at.elapsed() < Duration::from_millis(250),
            "unacknowledged success was dropped on the public caller: {:?}",
            cancelled_at.elapsed()
        );

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if removed.lock().unwrap().len() == 4 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the coordinator must eventually clean every returned resource exactly once");
        let removed = removed.lock().unwrap().clone();
        assert_eq!(removed.len(), 4, "{removed:?}");
        assert!(removed[0].contains("bob"), "{removed:?}");
        assert!(removed[1].contains("alice"), "{removed:?}");
        assert_eq!(&removed[2..], ["electrs", "bitcoind"], "{removed:?}");
    }

    // Catches unwind between successful publication and acknowledgement dropping armed handles
    // on the caller. The task panic remains prompt while the coordinator owns eventual cleanup.
    #[tokio::test]
    async fn builder_success_handoff_panic_returns_ownership_to_coordinator() {
        let mut engine = FakeEngine::new();
        engine.removal_delay = Duration::from_millis(300);
        let removed = Arc::clone(&engine.removed);
        let environment = FakeLndPairEnvironment {
            engine,
            connector: FakeConnector::succeeding(),
            backing_removal_delay: Duration::from_millis(300),
        };

        let panicked_at = std::time::Instant::now();
        let task = tokio::spawn(async move {
            LndPair::builder()
                .startup_timeout(Duration::from_millis(100))
                .start_with_environment_before_handoff_ack(environment, async move {
                    panic!("simulated caller panic before ownership acknowledgement")
                })
                .await
        });
        let panic = match task.await {
            Err(error) => error,
            Ok(_) => panic!("the caller-side handoff hook must panic"),
        };
        assert!(panic.is_panic());
        assert!(
            panicked_at.elapsed() < Duration::from_millis(250),
            "pre-ack panic synchronously dropped armed handles: {:?}",
            panicked_at.elapsed()
        );

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if removed.lock().unwrap().len() == 4 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the coordinator must recover ownership after pre-ack panic");
        let removed = removed.lock().unwrap().clone();
        assert_eq!(removed.len(), 4, "{removed:?}");
        assert!(removed[0].contains("bob"), "{removed:?}");
        assert!(removed[1].contains("alice"), "{removed:?}");
        assert_eq!(&removed[2..], ["electrs", "bitcoind"], "{removed:?}");
    }

    // Catches acknowledgement leaving a second coordinator-owned copy behind or failing to move
    // the one armed composite into the successfully returned caller value.
    #[tokio::test]
    async fn builder_success_acknowledgement_transfers_ownership_exactly_once() {
        let engine = FakeEngine::new();
        let removed = Arc::clone(&engine.removed);
        let environment = FakeLndPairEnvironment {
            engine,
            connector: FakeConnector::succeeding(),
            backing_removal_delay: Duration::ZERO,
        };

        let started = LndPair::builder()
            .start_with_environment(environment)
            .await
            .expect("the fake composite reaches its successful acknowledged handoff");
        assert!(removed.lock().unwrap().is_empty());
        drop(started);

        let removed = removed.lock().unwrap().clone();
        assert_eq!(removed.len(), 4, "{removed:?}");
        assert!(removed[0].contains("bob"), "{removed:?}");
        assert!(removed[1].contains("alice"), "{removed:?}");
        assert_eq!(&removed[2..], ["electrs", "bitcoind"], "{removed:?}");
    }

    // Pins the same dependency ordering for an ordinary startup error, where the public builder
    // remains present long enough to receive the typed LND failure from the coordinator.
    #[tokio::test]
    async fn builder_start_error_keeps_cross_stack_cleanup_order() {
        let engine = FakeEngine::new();
        let removed = Arc::clone(&engine.removed);
        let environment = FakeLndPairEnvironment {
            engine,
            connector: FakeConnector {
                initialized: Arc::new(Mutex::new(Vec::new())),
                fail_initialization: true,
                info_failure: None,
                info_calls: Arc::new(AtomicUsize::new(0)),
                ..FakeConnector::succeeding()
            },
            backing_removal_delay: Duration::ZERO,
        };

        let error = match LndPair::builder().start_with_environment(environment).await {
            Err(error) => error,
            Ok(_) => panic!("a rejected wallet initialization must fail the public builder"),
        };

        let FixtureError::Bootstrap { chain, source, .. } = &error else {
            panic!("wallet startup errors need typed Lightning bootstrap context: {error}");
        };
        assert_eq!(chain, &"Lightning");
        assert!(matches!(
            source.downcast_ref::<FixtureError>(),
            Some(FixtureError::Lightning(LndError::Status { .. }))
        ));
        let removed = removed.lock().unwrap().clone();
        assert!(removed[0].contains("bob"), "{removed:?}");
        assert!(removed[1].contains("alice"), "{removed:?}");
        assert_eq!(&removed[2..], ["electrs", "bitcoind"], "{removed:?}");
    }

    async fn assert_pending_sibling_is_cancelled(
        info_failure: InfoFailure,
        expected_operation: &'static str,
    ) {
        let connector = FakeConnector {
            initialized: Arc::new(Mutex::new(Vec::new())),
            fail_initialization: false,
            info_failure: Some(info_failure),
            info_calls: Arc::new(AtomicUsize::new(0)),
            ..FakeConnector::succeeding()
        };
        let alice = FakeClient {
            endpoint: "https://127.0.0.1:31009/".to_owned(),
        };
        let bob = FakeClient {
            endpoint: "https://127.0.0.1:32009/".to_owned(),
        };
        let deadline = Deadline::new(Duration::from_secs(5)).unwrap();

        let error = tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_lnd_sync(&connector, &alice, &bob, &PendingBitcoinTip, &deadline),
        )
        .await
        .expect("a permanent LND response must cancel pending sibling probes immediately")
        .expect_err("a permanent LND response must fail synchronization");

        let FixtureError::Bootstrap {
            chain,
            operation,
            source,
            ..
        } = &error
        else {
            panic!("a permanent LND response needs typed bootstrap context: {error}")
        };
        assert_eq!(chain, &"Lightning");
        assert_eq!(operation, &expected_operation);
        assert!(matches!(
            source.downcast_ref::<FixtureError>(),
            Some(FixtureError::Lightning(LndError::Authentication { .. }))
                | Some(FixtureError::Lightning(LndError::InvalidResponse { .. }))
        ));
    }

    // Catches join-all readiness polling waiting for Bob until the shared deadline after Alice has
    // already returned a terminal authentication failure.
    #[tokio::test]
    async fn alice_permanent_get_info_failure_cancels_pending_bob() {
        assert_pending_sibling_is_cancelled(
            InfoFailure::AliceAuthenticationBobPending,
            "query Alice synchronization",
        )
        .await;
    }

    // Catches a completion-order bias that is fail-fast only for Alice: Bob's permanent protocol
    // failure must likewise cancel a pending Alice probe.
    #[tokio::test]
    async fn bob_permanent_get_info_failure_cancels_pending_alice() {
        assert_pending_sibling_is_cancelled(
            InfoFailure::BobInvalidResponseAlicePending,
            "query Bob synchronization",
        )
        .await;
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
            FakeBitcoinTip::new(),
            default_channel_allocation(),
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
            info_failure: None,
            info_calls: Arc::new(AtomicUsize::new(0)),
            ..FakeConnector::succeeding()
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
            FakeBitcoinTip::new(),
            default_channel_allocation(),
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
        for secret in [
            "alice-rpc-secret",
            "raw-password",
            "secret-mnemonic-tail",
            "deadbeef",
            "70617373",
            "raw-macaroon",
            "bob-pem-private-secret",
        ] {
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
        engine.certificate_read = CertificateRead::Blocked;
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
                FakeBitcoinTip::new(),
                default_channel_allocation(),
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

    // Pins the asymmetric partial-start boundary: Alice has started, Bob's start future is still
    // pending, and cancellation must nevertheless remove both confirmed container IDs in reverse
    // dependency order.
    #[tokio::test]
    async fn cancellation_after_alice_starts_cleans_alice_and_reserved_bob() {
        let mut engine = FakeEngine::new();
        engine.block_bob_start = true;
        let alice_started = Arc::clone(&engine.alice_started);
        let task_engine = engine.clone();
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
                FakeBitcoinTip::new(),
                default_channel_allocation(),
            )
            .await
        });

        alice_started.notified().await;
        task.abort();
        let cancellation = match task.await {
            Err(error) => error,
            Ok(_) => panic!("aborting after Alice starts must cancel the startup task"),
        };
        assert!(cancellation.is_cancelled());

        let removed = engine.removed.lock().unwrap().clone();
        assert_eq!(
            removed.len(),
            2,
            "partial startup leaked a container: {removed:?}"
        );
        assert!(removed[0].contains("bob"), "{removed:?}");
        assert!(removed[1].contains("alice"), "{removed:?}");
    }

    // Catches failure diagnostics and supervisor cleanup coordination extending the advertised
    // whole-call clock. The fake log read never answers; advancing the one caller-owned deadline
    // must still complete the public startup future.
    #[tokio::test(start_paused = true)]
    async fn whole_startup_deadline_bounds_blocked_failure_diagnostics() {
        let mut engine = FakeEngine::new();
        engine.block_logs = true;
        let log_entered = Arc::clone(&engine.log_entered);
        let mut task = tokio::spawn(async move {
            let deadline = Deadline::new(Duration::from_secs(10)).unwrap();
            start_lnd_nodes_under(
                engine,
                "shared-network".to_owned(),
                "private-bitcoind".to_owned(),
                ContainerImage::lnd_default(),
                ContainerImage::lnd_default(),
                &deadline,
                FakeConnector {
                    initialized: Arc::new(Mutex::new(Vec::new())),
                    fail_initialization: true,
                    info_failure: None,
                    info_calls: Arc::new(AtomicUsize::new(0)),
                    ..FakeConnector::succeeding()
                },
                FakeBitcoinTip::new(),
                default_channel_allocation(),
            )
            .await
        });

        let entered = log_entered.notified();
        tokio::pin!(entered);
        loop {
            tokio::select! {
                biased;
                () = &mut entered => break,
                () = tokio::task::yield_now() => {}
            }
        }
        tokio::time::advance(Duration::from_secs(10)).await;
        let completed = tokio::time::timeout(Duration::from_millis(1), &mut task).await;
        if completed.is_err() {
            task.abort();
            let _ = task.await;
            panic!("blocked failure diagnostics outlived the absolute startup deadline");
        }
        assert!(completed.unwrap().unwrap().is_err());
    }

    // Catches cancellation synchronously joining cleanup past the caller's remaining startup
    // budget. Slow reverse-order removals continue in the dedicated supervisor after the cancelled
    // public future returns.
    #[tokio::test]
    async fn cancellation_wait_is_bounded_while_cleanup_finishes_in_background() {
        let mut engine = FakeEngine::new();
        engine.certificate_read = CertificateRead::Blocked;
        engine.removal_delay = Duration::from_millis(250);
        let task_engine = engine.clone();
        let read_entered = Arc::clone(&engine.read_entered);
        let started = std::time::Instant::now();

        let task = tokio::spawn(async move {
            let deadline = Deadline::new(Duration::from_millis(30)).unwrap();
            start_lnd_nodes_under(
                task_engine,
                "shared-network".to_owned(),
                "private-bitcoind".to_owned(),
                ContainerImage::lnd_default(),
                ContainerImage::lnd_default(),
                &deadline,
                FakeConnector::succeeding(),
                FakeBitcoinTip::new(),
                default_channel_allocation(),
            )
            .await
        });

        read_entered.notified().await;
        task.abort();
        let cancellation = match task.await {
            Err(error) => error,
            Ok(_) => panic!("aborting startup must cancel it"),
        };
        assert!(cancellation.is_cancelled());
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "cancellation waited past the 30ms startup deadline: {:?}",
            started.elapsed()
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if engine.removed.lock().unwrap().len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the detached supervisor must eventually clean both LND containers");
        let removed = engine.removed.lock().unwrap().clone();
        assert!(removed[0].contains("bob"), "{removed:?}");
        assert!(removed[1].contains("alice"), "{removed:?}");
    }

    // Catches archive/path/oversize failures being mistaken for a certificate that merely has not
    // been created yet. Both shapes must retain their engine source instead of degrading into a
    // source-less readiness timeout.
    #[tokio::test]
    async fn terminal_certificate_file_failures_return_immediately() {
        for certificate_read in [CertificateRead::Oversized, CertificateRead::Malformed] {
            let mut engine = FakeEngine::new();
            engine.certificate_read = certificate_read;
            let deadline = Deadline::new(Duration::from_millis(50)).unwrap();

            let error = match start_lnd_nodes_under(
                engine.clone(),
                "shared-network".to_owned(),
                "private-bitcoind".to_owned(),
                ContainerImage::lnd_default(),
                ContainerImage::lnd_default(),
                &deadline,
                FakeConnector::succeeding(),
                FakeBitcoinTip::new(),
                default_channel_allocation(),
            )
            .await
            {
                Err(error) => error,
                Ok(_) => panic!("an unsafe certificate file must fail startup"),
            };

            assert!(
                matches!(error, FixtureError::Runtime { ref operation, .. } if operation == "read container file"),
                "terminal certificate failures must retain runtime context: {error}"
            );
            assert_eq!(engine.certificate_reads.load(Ordering::SeqCst), 1);
        }
    }

    // Catches permanent GetInfo failures being rendered as retry observations until the shared
    // clock expires. The approved bootstrap wrapper must retain the typed LND cause.
    #[tokio::test]
    async fn permanent_get_info_failures_retain_the_lightning_source() {
        for info_failure in [InfoFailure::Authentication, InfoFailure::InvalidResponse] {
            let connector = FakeConnector {
                initialized: Arc::new(Mutex::new(Vec::new())),
                fail_initialization: false,
                info_failure: Some(info_failure),
                info_calls: Arc::new(AtomicUsize::new(0)),
                ..FakeConnector::succeeding()
            };
            let engine = FakeEngine::new();
            let deadline = Deadline::new(Duration::from_millis(50)).unwrap();

            let error = match start_lnd_nodes_under(
                engine,
                "shared-network".to_owned(),
                "private-bitcoind".to_owned(),
                ContainerImage::lnd_default(),
                ContainerImage::lnd_default(),
                &deadline,
                connector.clone(),
                FakeBitcoinTip::new(),
                default_channel_allocation(),
            )
            .await
            {
                Err(error) => error,
                Ok(_) => panic!("a permanent GetInfo failure must fail startup"),
            };

            let FixtureError::Bootstrap { chain, source, .. } = &error else {
                panic!("permanent GetInfo failures need Lightning bootstrap context: {error}")
            };
            assert_eq!(*chain, "Lightning");
            assert!(matches!(
                source.downcast_ref::<FixtureError>(),
                Some(FixtureError::Lightning(LndError::Authentication { .. }))
                    | Some(FixtureError::Lightning(LndError::InvalidResponse { .. }))
            ));
            assert_eq!(
                connector.info_calls.load(Ordering::SeqCst),
                1,
                "the first terminal response must end the round without waiting for its sibling"
            );
        }
    }

    // Pins the positive side of both retry classifiers: a missing certificate and an unavailable
    // GetInfo service are transient and can converge within the same deadline.
    #[tokio::test]
    async fn transient_certificate_and_get_info_failures_are_retried() {
        let mut engine = FakeEngine::new();
        engine.certificate_read = CertificateRead::MissingOnce;
        let connector = FakeConnector {
            initialized: Arc::new(Mutex::new(Vec::new())),
            fail_initialization: false,
            info_failure: Some(InfoFailure::UnavailableOnce),
            info_calls: Arc::new(AtomicUsize::new(0)),
            ..FakeConnector::succeeding()
        };
        let deadline = Deadline::new(Duration::from_secs(5)).unwrap();

        let (_, runtime) = start_lnd_nodes_under(
            engine.clone(),
            "shared-network".to_owned(),
            "private-bitcoind".to_owned(),
            ContainerImage::lnd_default(),
            ContainerImage::lnd_default(),
            &deadline,
            connector.clone(),
            FakeBitcoinTip::new(),
            default_channel_allocation(),
        )
        .await
        .expect("transient readiness failures must converge");

        assert!(engine.certificate_reads.load(Ordering::SeqCst) >= 3);
        assert!(connector.info_calls.load(Ordering::SeqCst) >= 4);
        runtime.shutdown().await.unwrap();
    }

    // Pinned LND can expose WalletUnlocker successfully and then briefly return status Unknown from
    // GetInfo while the unlocked Lightning server finishes starting. The absolute fixture deadline
    // still bounds this daemon transition; treating its first response as terminal makes every real
    // pair fail before channel bootstrap.
    #[tokio::test]
    async fn post_unlock_unknown_get_info_status_converges_within_the_shared_deadline() {
        let connector = FakeConnector {
            initialized: Arc::new(Mutex::new(Vec::new())),
            fail_initialization: false,
            info_failure: Some(InfoFailure::UnknownOnce),
            info_calls: Arc::new(AtomicUsize::new(0)),
            ..FakeConnector::succeeding()
        };
        let alice = FakeClient {
            endpoint: "https://127.0.0.1:31009/".to_owned(),
        };
        let bob = FakeClient {
            endpoint: "https://127.0.0.1:32009/".to_owned(),
        };
        let deadline = Deadline::new(Duration::from_secs(5)).unwrap();

        wait_for_lnd_sync(&connector, &alice, &bob, &FakeBitcoinTip::new(), &deadline)
            .await
            .expect("post-unlock Unknown status must be retried under the existing deadline");

        assert!(connector.info_calls.load(Ordering::SeqCst) >= 4);
    }

    #[tokio::test]
    async fn pre_peer_sync_allows_unsynced_graph_but_rejects_wrong_chain_identity_or_height() {
        let alice = FakeClient {
            endpoint: "https://127.0.0.1:31009/".to_owned(),
        };
        let bob = FakeClient {
            endpoint: "https://127.0.0.1:32009/".to_owned(),
        };
        let graph_unsynced = FakeConnector {
            initialized: Arc::new(Mutex::new(Vec::new())),
            fail_initialization: false,
            info_failure: Some(InfoFailure::GraphUnsynced),
            info_calls: Arc::new(AtomicUsize::new(0)),
            ..FakeConnector::succeeding()
        };
        let deadline = Deadline::new(Duration::from_secs(1)).unwrap();
        wait_for_lnd_sync(
            &graph_unsynced,
            &alice,
            &bob,
            &FakeBitcoinTip::new(),
            &deadline,
        )
        .await
        .expect("isolated nodes cannot graph-sync before their private peer connection exists");

        for failure in [InfoFailure::WrongNetwork, InfoFailure::WrongHeight] {
            let connector = FakeConnector {
                initialized: Arc::new(Mutex::new(Vec::new())),
                fail_initialization: false,
                info_failure: Some(failure),
                info_calls: Arc::new(AtomicUsize::new(0)),
                ..FakeConnector::succeeding()
            };
            let deadline = Deadline::new(Duration::from_millis(20)).unwrap();
            let error =
                wait_for_lnd_sync(&connector, &alice, &bob, &FakeBitcoinTip::new(), &deadline)
                    .await
                    .expect_err(
                        "wrong regtest identity or height must remain blocked by the deadline",
                    );
            assert!(matches!(error, FixtureError::ReadinessTimeout { .. }));
        }
    }

    #[tokio::test]
    async fn reverse_readiness_retries_only_transient_routes_with_fresh_invoices() {
        let mut connector = FakeConnector::succeeding();
        connector.payment_failures_remaining = Arc::new(AtomicUsize::new(2));
        connector.payment_failure_reason = Some("insufficient balance");
        let alice = FakeClient {
            endpoint: "https://127.0.0.1:31009/".to_owned(),
        };
        let bob = FakeClient {
            endpoint: "https://127.0.0.1:32009/".to_owned(),
        };
        let deadline = Deadline::new(Duration::from_secs(2)).unwrap();

        prove_reverse_readiness_with_retry(
            &connector,
            &alice,
            &bob,
            &FakeBitcoinTip::new(),
            fake_channel_point(),
            fake_public_key(&alice),
            fake_public_key(&bob),
            "private-bob",
            &deadline,
        )
        .await
        .expect("transient route propagation must converge with a fresh invoice per attempt");

        assert_eq!(connector.payment_attempts.load(Ordering::SeqCst), 3);
        assert_eq!(connector.invoice_calls.load(Ordering::SeqCst), 3);

        let mut permanent = FakeConnector::succeeding();
        permanent.payment_failures_remaining = Arc::new(AtomicUsize::new(2));
        permanent.payment_failure_reason = Some("incorrect payment details");
        let error = prove_reverse_readiness_with_retry(
            &permanent,
            &alice,
            &bob,
            &FakeBitcoinTip::new(),
            fake_channel_point(),
            fake_public_key(&alice),
            fake_public_key(&bob),
            "private-bob",
            &deadline,
        )
        .await
        .expect_err("a permanent payment failure must not be retried");
        assert!(matches!(error, FixtureError::Bootstrap { .. }));
        assert_eq!(permanent.payment_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(permanent.invoice_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reverse_route_retries_cannot_outlive_the_original_deadline() {
        let mut connector = FakeConnector::succeeding();
        connector.payment_failures_remaining = Arc::new(AtomicUsize::new(usize::MAX));
        connector.payment_failure_reason = Some("no route");
        let alice = FakeClient {
            endpoint: "https://127.0.0.1:31009/".to_owned(),
        };
        let bob = FakeClient {
            endpoint: "https://127.0.0.1:32009/".to_owned(),
        };
        let deadline = Deadline::new(Duration::from_millis(20)).unwrap();

        let error = prove_reverse_readiness_with_retry(
            &connector,
            &alice,
            &bob,
            &FakeBitcoinTip::new(),
            fake_channel_point(),
            fake_public_key(&alice),
            fake_public_key(&bob),
            "private-bob",
            &deadline,
        )
        .await
        .expect_err("route retries must stop at the one original startup deadline");

        assert!(matches!(error, FixtureError::ReadinessTimeout { .. }));
        assert!(connector.payment_attempts.load(Ordering::SeqCst) >= 1);
    }

    const DOCKER_ANONYMOUS_VOLUME_LABEL: &str = "com.docker.volume.anonymous";

    async fn inspect_pair_resources(
        pair: &LndPair,
    ) -> (bollard::Docker, Vec<String>, String, Vec<String>) {
        use bollard::{Docker, models::MountPointTypeEnum};

        let docker = Docker::connect_with_local_defaults()
            .expect("the daemon that started the pair remains reachable");
        let containers = pair.container_ids().into_iter().collect::<Vec<_>>();
        let network = pair.handles.bitcoin.network_name().to_owned();
        docker
            .inspect_network(&network, None)
            .await
            .expect("the pair's private network must exist while it is owned");

        let mut volumes = Vec::new();
        for container in &containers {
            let inspected = docker
                .inspect_container(container, None)
                .await
                .expect("all four owned containers must exist while the pair is alive");
            for mount in inspected.mounts.unwrap_or_default() {
                assert_eq!(mount.typ, Some(MountPointTypeEnum::VOLUME), "{mount:?}");
                let name = mount.name.expect("an anonymous volume mount must be named");
                let volume = docker
                    .inspect_volume(&name)
                    .await
                    .expect("an attached anonymous volume must be inspectable");
                assert_eq!(
                    volume.labels.keys().collect::<Vec<_>>(),
                    vec![DOCKER_ANONYMOUS_VOLUME_LABEL],
                    "{name} must be owned anonymously by this pair"
                );
                volumes.push(name);
            }
        }
        (docker, containers, network, volumes)
    }

    async fn assert_pair_resources_removed(
        docker: &bollard::Docker,
        containers: &[String],
        network: &str,
        volumes: &[String],
    ) {
        let mut outstanding = Vec::new();
        for _ in 0..100 {
            outstanding.clear();
            for container in containers {
                if docker.inspect_container(container, None).await.is_ok() {
                    outstanding.push(container.clone());
                }
            }
            if docker.inspect_network(network, None).await.is_ok() {
                outstanding.push(network.to_owned());
            }
            for volume in volumes {
                if docker.inspect_volume(volume).await.is_ok() {
                    outstanding.push(volume.clone());
                }
            }
            if outstanding.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("LND pair cleanup left owned resources behind: {outstanding:?}");
    }

    #[tokio::test]
    async fn explicit_shutdown_removes_four_containers_volumes_and_network() {
        let pair = LndPair::start()
            .await
            .expect("a real ready-to-pay LND pair must start");
        let (docker, containers, network, volumes) = inspect_pair_resources(&pair).await;
        assert_eq!(containers.len(), 4);

        pair.shutdown()
            .await
            .expect("explicit LND pair cleanup must attempt both dependency layers");
        assert_pair_resources_removed(&docker, &containers, &network, &volumes).await;
    }

    #[tokio::test]
    async fn dropping_pair_removes_four_containers_volumes_and_network() {
        let pair = LndPair::start()
            .await
            .expect("a real ready-to-pay LND pair must start");
        let (docker, containers, network, volumes) = inspect_pair_resources(&pair).await;
        assert_eq!(containers.len(), 4);

        drop(pair);
        assert_pair_resources_removed(&docker, &containers, &network, &volumes).await;
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
