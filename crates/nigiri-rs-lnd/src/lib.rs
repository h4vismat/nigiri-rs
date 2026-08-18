//! Private LND protocol bindings and Lightning client APIs.

use std::future::Future;

use bitcoin::{Address, OutPoint, address::NetworkUnchecked};

mod proto;

mod amount;
mod channel;
mod client;
mod config;
mod convert;
mod endpoint;
mod error;
mod node;
mod transport;
mod types;

pub use amount::{Millisats, Sats};
pub use client::LndClient;
pub use config::{LndConfig, MAX_MACAROON_BYTES, MAX_TLS_CERTIFICATE_BYTES};
pub use error::LndError;
pub use types::{
    Channel, CreateInvoiceRequest, InvoiceRecord, InvoiceState, NodeInfo, OpenChannelRequest,
    PaymentOptions, PaymentRecord, PaymentState, Peer, PeerAddress, WalletBalance,
};

/// Portable, statically dispatched Lightning node operations.
pub trait LightningNode: Clone + Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn get_info(&self) -> impl Future<Output = Result<NodeInfo, Self::Error>> + Send;
    fn new_address(
        &self,
    ) -> impl Future<Output = Result<Address<NetworkUnchecked>, Self::Error>> + Send;
    fn wallet_balance(&self) -> impl Future<Output = Result<WalletBalance, Self::Error>> + Send;
    fn connect_peer(
        &self,
        peer: &PeerAddress,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn list_peers(&self) -> impl Future<Output = Result<Vec<Peer>, Self::Error>> + Send;
    fn open_channel(
        &self,
        request: OpenChannelRequest,
    ) -> impl Future<Output = Result<OutPoint, Self::Error>> + Send;
    fn list_channels(&self) -> impl Future<Output = Result<Vec<Channel>, Self::Error>> + Send;
}

impl LightningNode for LndClient {
    type Error = LndError;

    fn get_info(&self) -> impl Future<Output = Result<NodeInfo, Self::Error>> + Send {
        LndClient::get_info(self)
    }

    fn new_address(
        &self,
    ) -> impl Future<Output = Result<Address<NetworkUnchecked>, Self::Error>> + Send {
        LndClient::new_address(self)
    }

    fn wallet_balance(&self) -> impl Future<Output = Result<WalletBalance, Self::Error>> + Send {
        LndClient::wallet_balance(self)
    }

    fn connect_peer(
        &self,
        peer: &PeerAddress,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        LndClient::connect_peer(self, peer)
    }

    fn list_peers(&self) -> impl Future<Output = Result<Vec<Peer>, Self::Error>> + Send {
        LndClient::list_peers(self)
    }

    fn open_channel(
        &self,
        request: OpenChannelRequest,
    ) -> impl Future<Output = Result<OutPoint, Self::Error>> + Send {
        LndClient::open_channel(self, request)
    }

    fn list_channels(&self) -> impl Future<Output = Result<Vec<Channel>, Self::Error>> + Send {
        LndClient::list_channels(self)
    }
}

/// The pinned upstream LND protobuf release.
pub const LND_PROTO_VERSION: &str = "v0.21.1-beta";

/// The pinned upstream LND protobuf commit.
pub const LND_PROTO_COMMIT: &str = "2b87887";
