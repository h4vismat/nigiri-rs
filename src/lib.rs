//! Typed Bitcoin and Liquid clients for an already-running Nigiri regtest environment.
//!
//! Nigiri's lifecycle is owned by the host application. This crate never starts,
//! stops, deletes, or otherwise manages Nigiri or its Docker resources.
//!
//! # Clients
//!
//! ```no_run
//! use bitcoin::Amount;
//! use nigiri_rs::{Bitcoin, Liquid, NigiriClient};
//!
//! # async fn example() -> Result<(), nigiri_rs::NigiriError> {
//! let bitcoin = NigiriClient::<Bitcoin>::new();
//! let liquid = NigiriClient::<Liquid>::new();
//! bitcoin.wait_ready().await?;
//! liquid.wait_ready().await?;
//! let address = bitcoin.new_address().await?;
//! let address_text = address.to_string();
//! bitcoin.faucet(&address_text, Some(Amount::from_sat(50_000))).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Advanced node RPC
//!
//! [`NigiriClient::rpc`] invokes an arbitrary node RPC with separately passed
//! CLI-style arguments and deserializes the result into a caller-selected type.
//!
//! ```no_run
//! use nigiri_rs::{Bitcoin, NigiriClient};
//!
//! # async fn example() -> Result<(), nigiri_rs::NigiriError> {
//! let client = NigiriClient::<Bitcoin>::new();
//! let height: u64 = client
//!     .rpc("getblockcount", std::iter::empty::<&str>())
//!     .await?;
//! assert!(height > 0);
//! # Ok(())
//! # }
//! ```
//!
//! Arbitrary RPC methods may mutate wallet or chain state. The host application
//! remains responsible for synchronization and restoration.
//!
//! Liquid-only methods are absent from the Bitcoin client at compile time:
//!
//! ```compile_fail
//! use nigiri_rs::{Bitcoin, NigiriClient};
//!
//! # async fn example() {
//! let bitcoin = NigiriClient::<Bitcoin>::new();
//! let _ = bitcoin.mint("bcrt1q...", 1000, "Asset", "AST").await;
//! # }
//! ```
//!
//! ```compile_fail
//! use bitcoin::Amount;
//! use nigiri_rs::{Bitcoin, NigiriClient};
//!
//! # async fn example(asset: elements::AssetId) {
//! let bitcoin = NigiriClient::<Bitcoin>::new();
//! let _ = bitcoin
//!     .faucet_asset("bcrt1q...", Amount::ONE_BTC, &asset)
//!     .await;
//! # }
//! ```

mod client;
mod config;
mod error;
mod http;
mod liquid;
mod network;
mod node_rpc;
mod rpc;
mod types;

#[cfg(feature = "bitcoin-rpc-types")]
pub use corepc_types as bitcoin_rpc_types;

pub use client::NigiriClient;
pub use config::{DEFAULT_MAX_RPC_RESPONSE_BYTES, MAX_RPC_RESPONSE_BYTES_LIMIT, NigiriConfig};
pub use error::NigiriError;
pub use network::{Bitcoin, Liquid, NigiriNetwork};
pub use types::{
    AddressStats, BitcoinAddressInfo, BitcoinTxInfo, BitcoinUtxo, IssuanceTxIn, LiquidAddressInfo,
    LiquidTxInfo, LiquidUtxo, MintResponse, TxStatus,
};

/// Native Liquid regtest policy asset identifier.
pub static LBTC_REGTEST_ASSET: std::sync::LazyLock<elements::AssetId> =
    std::sync::LazyLock::new(|| {
        "5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225"
            .parse()
            .expect("static Liquid regtest asset id is valid")
    });
