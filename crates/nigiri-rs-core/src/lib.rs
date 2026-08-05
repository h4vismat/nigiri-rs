//! Typed clients for compatible Bitcoin and Liquid regtest services.
//!
//! This lifecycle-neutral core crate does not provision services: point it at a
//! Nigiri installation or any compatible endpoints you already run.
//!
//! To have the services provisioned for you, use the companion `nigiri-rs-testcontainers`
//! crate in this workspace. It starts a throwaway Bitcoin or Liquid regtest stack per test,
//! hands back a client already pointed at it, and removes everything on drop. It
//! needs Docker and no Nigiri installation. This crate does not depend on it.
//!
//! # Clients
//!
//! ```no_run
//! use bitcoin::Amount;
//! use nigiri_rs_core::{Bitcoin, Liquid, NigiriClient};
//!
//! # async fn example() -> Result<(), nigiri_rs_core::NigiriError> {
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
//! [`NigiriClient::rpc`] invokes an arbitrary node RPC with JSON-serialized
//! parameters and deserializes the result into a caller-selected type. Use `()`
//! for no parameters and tuples for positional JSON parameters; JSON strings are
//! not coerced into the number or boolean types expected by a node method.
//!
//! ```no_run
//! use nigiri_rs_core::{Bitcoin, NigiriClient};
//!
//! # async fn example() -> Result<(), nigiri_rs_core::NigiriError> {
//! let client = NigiriClient::<Bitcoin>::new();
//! let height: u64 = client.rpc("getblockcount", ()).await?;
//! let hundredth_hash: bitcoin::BlockHash = client.rpc("getblockhash", (100_u64,)).await?;
//! assert!(height > 0);
//! let _ = hundredth_hash;
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
//! use nigiri_rs_core::{Bitcoin, NigiriClient};
//!
//! # async fn example() {
//! let bitcoin = NigiriClient::<Bitcoin>::new();
//! let _ = bitcoin.mint("bcrt1q...", 1000, "Asset", "AST").await;
//! # }
//! ```
//!
//! ```compile_fail
//! use bitcoin::Amount;
//! use nigiri_rs_core::{Bitcoin, NigiriClient};
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
mod endpoint;
mod error;
mod http;
mod liquid;
mod network;
mod node_rpc;
mod peg;
mod rpc;
mod types;

#[cfg(feature = "bitcoin-rpc-types")]
pub use corepc_types as bitcoin_rpc_types;

pub use client::NigiriClient;
pub use config::{DEFAULT_MAX_RESPONSE_BYTES, MAX_RESPONSE_BYTES_LIMIT, NigiriConfig};
pub use endpoint::ElectrumEndpoint;
pub use error::NigiriError;
pub use network::{Bitcoin, Liquid, NigiriNetwork};
pub use peg::{Peg, PegIn, PegInRequest, PegOut};
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
