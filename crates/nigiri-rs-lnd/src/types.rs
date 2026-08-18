use std::time::Duration;

use bitcoin::{OutPoint, hashes::sha256, secp256k1::PublicKey};
use lightning_invoice::Bolt11Invoice;

use crate::{LndError, Millisats, Sats, endpoint::normalize_outbound_peer_host};

/// Information reported by an LND node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeInfo {
    public_key: PublicKey,
    alias: String,
    version: String,
    block_height: u32,
    network: String,
    synced_to_chain: bool,
    synced_to_graph: bool,
}

impl NodeInfo {
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn new(
        public_key: PublicKey,
        alias: String,
        version: String,
        block_height: u32,
        network: String,
        synced_to_chain: bool,
        synced_to_graph: bool,
    ) -> Self {
        Self {
            public_key,
            alias,
            version,
            block_height,
            network,
            synced_to_chain,
            synced_to_graph,
        }
    }

    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.public_key
    }
    #[must_use]
    pub fn alias(&self) -> &str {
        &self.alias
    }
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }
    #[must_use]
    pub const fn block_height(&self) -> u32 {
        self.block_height
    }
    #[must_use]
    pub fn network(&self) -> &str {
        &self.network
    }
    #[must_use]
    pub const fn synced_to_chain(&self) -> bool {
        self.synced_to_chain
    }
    #[must_use]
    pub const fn synced_to_graph(&self) -> bool {
        self.synced_to_graph
    }
}

/// On-chain wallet amounts reported by LND.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalletBalance {
    total: Sats,
    confirmed: Sats,
    unconfirmed: Sats,
}

impl WalletBalance {
    #[allow(dead_code)]
    #[must_use]
    pub(crate) const fn new(total: Sats, confirmed: Sats, unconfirmed: Sats) -> Self {
        Self {
            total,
            confirmed,
            unconfirmed,
        }
    }
    #[must_use]
    pub const fn total(&self) -> Sats {
        self.total
    }
    #[must_use]
    pub const fn confirmed(&self) -> Sats {
        self.confirmed
    }
    #[must_use]
    pub const fn unconfirmed(&self) -> Sats {
        self.unconfirmed
    }
}

/// A remote Lightning node endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerAddress {
    public_key: PublicKey,
    host: String,
    port: u16,
}

impl PeerAddress {
    pub fn new(public_key: PublicKey, host: impl AsRef<str>, port: u16) -> Result<Self, LndError> {
        let host = host.as_ref().trim();
        if host.is_empty() {
            return Err(invalid("peer host must not be blank"));
        }
        if port == 0 {
            return Err(invalid("peer port must not be zero"));
        }
        let host = normalize_outbound_peer_host(host)
            .map_err(|()| invalid("peer host must be a valid host without a port"))?;
        Ok(Self {
            public_key,
            host,
            port,
        })
    }

    #[must_use]
    pub const fn public_key(&self) -> PublicKey {
        self.public_key
    }
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

/// The connection status of a Lightning peer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Peer {
    public_key: PublicKey,
    address: String,
    connected: bool,
}

impl Peer {
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn new(public_key: PublicKey, address: String, connected: bool) -> Self {
        Self {
            public_key,
            address,
            connected,
        }
    }
    #[must_use]
    pub const fn public_key(&self) -> PublicKey {
        self.public_key
    }
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }
    #[must_use]
    pub const fn connected(&self) -> bool {
        self.connected
    }
}

/// Validated channel-opening parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenChannelRequest {
    peer_public_key: PublicKey,
    capacity: Sats,
    push_amount: Sats,
}

impl OpenChannelRequest {
    pub fn new(
        peer_public_key: PublicKey,
        capacity: Sats,
        push_amount: Sats,
    ) -> Result<Self, LndError> {
        if capacity.as_u64() == 0 {
            return Err(invalid("channel capacity must be greater than zero"));
        }
        if push_amount >= capacity {
            return Err(invalid("channel push amount must be lower than capacity"));
        }
        Ok(Self {
            peer_public_key,
            capacity,
            push_amount,
        })
    }

    #[must_use]
    pub const fn peer_public_key(&self) -> PublicKey {
        self.peer_public_key
    }
    #[must_use]
    pub const fn capacity(&self) -> Sats {
        self.capacity
    }
    #[must_use]
    pub const fn push_amount(&self) -> Sats {
        self.push_amount
    }
}

/// A Lightning channel reported by LND.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Channel {
    channel_point: OutPoint,
    remote_public_key: PublicKey,
    active: bool,
    capacity: Sats,
    local_balance: Millisats,
    remote_balance: Millisats,
}

impl Channel {
    #[allow(dead_code)]
    #[must_use]
    pub(crate) const fn new(
        channel_point: OutPoint,
        remote_public_key: PublicKey,
        active: bool,
        capacity: Sats,
        local_balance: Millisats,
        remote_balance: Millisats,
    ) -> Self {
        Self {
            channel_point,
            remote_public_key,
            active,
            capacity,
            local_balance,
            remote_balance,
        }
    }
    #[must_use]
    pub const fn channel_point(&self) -> OutPoint {
        self.channel_point
    }
    #[must_use]
    pub const fn remote_public_key(&self) -> PublicKey {
        self.remote_public_key
    }
    #[must_use]
    pub const fn active(&self) -> bool {
        self.active
    }
    #[must_use]
    pub const fn capacity(&self) -> Sats {
        self.capacity
    }
    #[must_use]
    pub const fn local_balance(&self) -> Millisats {
        self.local_balance
    }
    #[must_use]
    pub const fn remote_balance(&self) -> Millisats {
        self.remote_balance
    }
}

/// Validated invoice creation parameters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateInvoiceRequest {
    amount: Millisats,
    memo: String,
    expiry: Duration,
}

impl CreateInvoiceRequest {
    pub fn new(
        amount: Millisats,
        memo: impl Into<String>,
        expiry: Duration,
    ) -> Result<Self, LndError> {
        if amount.as_u64() == 0 {
            return Err(invalid("invoice amount must be greater than zero"));
        }
        if expiry.is_zero() {
            return Err(invalid("invoice expiry must be greater than zero"));
        }
        Ok(Self {
            amount,
            memo: memo.into(),
            expiry,
        })
    }
    #[must_use]
    pub const fn amount(&self) -> Millisats {
        self.amount
    }
    #[must_use]
    pub fn memo(&self) -> &str {
        &self.memo
    }
    #[must_use]
    pub const fn expiry(&self) -> Duration {
        self.expiry
    }
}

/// LND invoice state with a forward-compatible daemon value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvoiceState {
    Open,
    Settled,
    Canceled,
    Accepted,
    Unknown(i32),
}

/// A parsed invoice record reported by LND.
#[derive(Clone, Debug)]
pub struct InvoiceRecord {
    invoice: Bolt11Invoice,
    payment_hash: sha256::Hash,
    amount: Millisats,
    state: InvoiceState,
}

impl InvoiceRecord {
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn new(
        invoice: Bolt11Invoice,
        payment_hash: sha256::Hash,
        amount: Millisats,
        state: InvoiceState,
    ) -> Self {
        Self {
            invoice,
            payment_hash,
            amount,
            state,
        }
    }
    #[must_use]
    pub fn invoice(&self) -> &Bolt11Invoice {
        &self.invoice
    }
    #[must_use]
    pub const fn payment_hash(&self) -> sha256::Hash {
        self.payment_hash
    }
    #[must_use]
    pub const fn amount(&self) -> Millisats {
        self.amount
    }
    #[must_use]
    pub const fn state(&self) -> InvoiceState {
        self.state
    }
}

/// Validated payment fee limit and operation timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaymentOptions {
    fee_limit: Millisats,
    timeout: Duration,
}

impl PaymentOptions {
    pub fn new(fee_limit: Millisats, timeout: Duration) -> Result<Self, LndError> {
        if timeout.is_zero() {
            return Err(invalid("payment timeout must be greater than zero"));
        }
        Ok(Self { fee_limit, timeout })
    }
    #[must_use]
    pub const fn fee_limit(&self) -> Millisats {
        self.fee_limit
    }
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// LND payment state with a forward-compatible daemon value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentState {
    InFlight,
    Succeeded,
    Failed,
    Unknown(i32),
}

/// A terminal or in-flight payment record reported by LND.
#[derive(Clone, Eq, PartialEq)]
pub struct PaymentRecord {
    payment_hash: sha256::Hash,
    preimage: Option<[u8; 32]>,
    value: Millisats,
    fee: Millisats,
    state: PaymentState,
}

impl PaymentRecord {
    #[allow(dead_code)]
    #[must_use]
    pub(crate) const fn new(
        payment_hash: sha256::Hash,
        preimage: Option<[u8; 32]>,
        value: Millisats,
        fee: Millisats,
        state: PaymentState,
    ) -> Self {
        Self {
            payment_hash,
            preimage,
            value,
            fee,
            state,
        }
    }
    #[must_use]
    pub const fn payment_hash(&self) -> sha256::Hash {
        self.payment_hash
    }
    #[must_use]
    pub const fn preimage(&self) -> Option<[u8; 32]> {
        self.preimage
    }
    #[must_use]
    pub const fn value(&self) -> Millisats {
        self.value
    }
    #[must_use]
    pub const fn fee(&self) -> Millisats {
        self.fee
    }
    #[must_use]
    pub const fn state(&self) -> PaymentState {
        self.state
    }
}

impl std::fmt::Debug for PaymentRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PaymentRecord")
            .field("payment_hash", &self.payment_hash)
            .field("value", &self.value)
            .field("fee", &self.fee)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

fn invalid(detail: &'static str) -> LndError {
    LndError::InvalidRequest {
        detail: detail.into(),
    }
}
