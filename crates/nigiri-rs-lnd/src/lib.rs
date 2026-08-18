//! Private LND protocol bindings and Lightning client APIs.

mod proto;

mod amount;
mod client;
mod config;
mod convert;
mod error;
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

/// The pinned upstream LND protobuf release.
pub const LND_PROTO_VERSION: &str = "v0.21.1-beta";

/// The pinned upstream LND protobuf commit.
pub const LND_PROTO_COMMIT: &str = "2b87887";
