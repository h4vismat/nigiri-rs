use std::{fmt::Display, str::FromStr};

use bitcoin::address::NetworkUnchecked;
use elements::secp256k1_zkp::{Generator, PedersenCommitment};
use serde::{Deserialize, Deserializer};

/// Typed issuance input returned by Nigiri mint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuanceTxIn {
    pub txid: elements::Txid,
    pub vin: u32,
}

/// Result of minting a Liquid asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintResponse {
    pub asset: elements::AssetId,
    pub txid: elements::Txid,
    pub issuance_txin: IssuanceTxIn,
}

/// Confirmation data shared by Bitcoin and Liquid Esplora responses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(bound(deserialize = "H: Deserialize<'de>"))]
pub struct TxStatus<H> {
    pub confirmed: bool,
    #[serde(default)]
    pub block_height: Option<u64>,
    #[serde(default)]
    pub block_hash: Option<H>,
    #[serde(default)]
    pub block_time: Option<u64>,
}

/// Bitcoin address statistics reported by Esplora.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AddressStats {
    pub tx_count: u64,
    pub funded_txo_count: u64,
    #[serde(deserialize_with = "amount_from_sats")]
    pub funded_txo_sum: bitcoin::Amount,
    pub spent_txo_count: u64,
    #[serde(deserialize_with = "amount_from_sats")]
    pub spent_txo_sum: bitcoin::Amount,
}

/// Bitcoin address information returned by Esplora.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BitcoinAddressInfo {
    #[serde(deserialize_with = "from_string")]
    pub address: bitcoin::Address<NetworkUnchecked>,
    pub chain_stats: AddressStats,
    pub mempool_stats: AddressStats,
}

/// Liquid address statistics omit sums because confidential values are unknown.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LiquidAddressStats {
    pub tx_count: u64,
    pub funded_txo_count: u64,
    pub spent_txo_count: u64,
}

/// Liquid address information returned by Esplora.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LiquidAddressInfo {
    #[serde(deserialize_with = "from_string")]
    pub address: elements::Address,
    pub chain_stats: LiquidAddressStats,
    pub mempool_stats: LiquidAddressStats,
}

/// A Bitcoin unspent output.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BitcoinUtxo {
    pub txid: bitcoin::Txid,
    pub vout: u32,
    #[serde(deserialize_with = "amount_from_sats")]
    pub value: bitcoin::Amount,
    pub status: TxStatus<bitcoin::BlockHash>,
}

/// A Liquid unspent output, including explicit and confidential forms.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LiquidUtxo {
    pub txid: elements::Txid,
    pub vout: u32,
    #[serde(default, deserialize_with = "optional_amount_from_sats")]
    pub value: Option<bitcoin::Amount>,
    #[serde(default, deserialize_with = "optional_from_string")]
    pub asset: Option<elements::AssetId>,
    #[serde(
        rename = "valuecommitment",
        default,
        deserialize_with = "optional_from_string"
    )]
    pub value_commitment: Option<PedersenCommitment>,
    #[serde(
        rename = "assetcommitment",
        default,
        deserialize_with = "optional_from_string"
    )]
    pub asset_commitment: Option<Generator>,
    pub status: TxStatus<elements::BlockHash>,
}

/// Typed subset of a Bitcoin Esplora transaction response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BitcoinTxInfo {
    pub txid: bitcoin::Txid,
    pub size: u64,
    pub weight: u64,
    #[serde(deserialize_with = "amount_from_sats")]
    pub fee: bitcoin::Amount,
    pub status: TxStatus<bitcoin::BlockHash>,
}

/// Typed subset of a Liquid Esplora transaction response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LiquidTxInfo {
    pub txid: elements::Txid,
    pub size: u64,
    pub weight: u64,
    #[serde(deserialize_with = "amount_from_sats")]
    pub fee: bitcoin::Amount,
    pub status: TxStatus<elements::BlockHash>,
}

fn amount_from_sats<'de, D>(deserializer: D) -> Result<bitcoin::Amount, D::Error>
where
    D: Deserializer<'de>,
{
    u64::deserialize(deserializer).map(bitcoin::Amount::from_sat)
}

fn optional_amount_from_sats<'de, D>(deserializer: D) -> Result<Option<bitcoin::Amount>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<u64>::deserialize(deserializer).map(|value| value.map(bitcoin::Amount::from_sat))
}

fn from_string<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    let value = String::deserialize(deserializer)?;
    value.parse().map_err(serde::de::Error::custom)
}

fn optional_from_string<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| value.parse().map_err(serde::de::Error::custom))
        .transpose()
}
