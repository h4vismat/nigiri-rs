use std::fmt::Display;

use bitcoin::address::{NetworkChecked, NetworkUnchecked};
use serde::de::DeserializeOwned;

use crate::NigiriError;
use crate::types::{
    BitcoinAddressInfo, BitcoinTxInfo, BitcoinUtxo, LiquidAddressInfo, LiquidTxInfo, LiquidUtxo,
    TxStatus,
};

/// Marker for Nigiri's Bitcoin regtest services.
#[derive(Debug, Clone, Copy)]
pub enum Bitcoin {}

/// Marker for Nigiri's Liquid regtest services.
#[derive(Debug, Clone, Copy)]
pub enum Liquid {}

/// A sealed Nigiri network and its native protocol response types.
pub trait NigiriNetwork: private::Sealed {
    type Txid: Display;
    type BlockHash: Display;
    type Address;
    type Utxo;
    type TxInfo;
    type AddressInfo;
}

impl NigiriNetwork for Bitcoin {
    type Txid = bitcoin::Txid;
    type BlockHash = bitcoin::BlockHash;
    type Address = bitcoin::Address<NetworkChecked>;
    type Utxo = BitcoinUtxo;
    type TxInfo = BitcoinTxInfo;
    type AddressInfo = BitcoinAddressInfo;
}

impl NigiriNetwork for Liquid {
    type Txid = elements::Txid;
    type BlockHash = elements::BlockHash;
    type Address = elements::Address;
    type Utxo = LiquidUtxo;
    type TxInfo = LiquidTxInfo;
    type AddressInfo = LiquidAddressInfo;
}

pub(crate) mod private {
    use super::*;

    pub trait Sealed: Sized {
        fn rpc_prefix() -> &'static [&'static str];

        fn parse_txid(
            operation: &'static str,
            value: &str,
        ) -> Result<<Self as NigiriNetwork>::Txid, NigiriError>
        where
            Self: NigiriNetwork;
        fn parse_block_hash(
            operation: &'static str,
            value: &str,
        ) -> Result<<Self as NigiriNetwork>::BlockHash, NigiriError>
        where
            Self: NigiriNetwork;
        fn parse_address(
            operation: &'static str,
            value: &str,
        ) -> Result<<Self as NigiriNetwork>::Address, NigiriError>
        where
            Self: NigiriNetwork;
        fn parse_utxos(
            operation: &'static str,
            body: &[u8],
        ) -> Result<Vec<<Self as NigiriNetwork>::Utxo>, NigiriError>
        where
            Self: NigiriNetwork;
        fn parse_tx_info(
            operation: &'static str,
            body: &[u8],
        ) -> Result<<Self as NigiriNetwork>::TxInfo, NigiriError>
        where
            Self: NigiriNetwork;
        fn parse_address_info(
            operation: &'static str,
            body: &[u8],
        ) -> Result<<Self as NigiriNetwork>::AddressInfo, NigiriError>
        where
            Self: NigiriNetwork;
        fn parse_tx_status(
            operation: &'static str,
            body: &[u8],
        ) -> Result<TxStatus<<Self as NigiriNetwork>::BlockHash>, NigiriError>
        where
            Self: NigiriNetwork;
    }

    impl Sealed for super::Bitcoin {
        fn rpc_prefix() -> &'static [&'static str] {
            &["rpc"]
        }

        fn parse_txid(
            operation: &'static str,
            value: &str,
        ) -> Result<<Self as NigiriNetwork>::Txid, NigiriError> {
            parse_string(operation, value, "Bitcoin txid")
        }

        fn parse_block_hash(
            operation: &'static str,
            value: &str,
        ) -> Result<<Self as NigiriNetwork>::BlockHash, NigiriError> {
            parse_string(operation, value, "Bitcoin block hash")
        }

        fn parse_address(
            operation: &'static str,
            value: &str,
        ) -> Result<<Self as NigiriNetwork>::Address, NigiriError> {
            let unchecked: bitcoin::Address<NetworkUnchecked> =
                parse_string(operation, value, "Bitcoin address")?;
            unchecked
                .require_network(bitcoin::Network::Regtest)
                .map_err(|_| invalid(operation, "Bitcoin address was not for regtest"))
        }

        fn parse_utxos(
            operation: &'static str,
            body: &[u8],
        ) -> Result<Vec<<Self as NigiriNetwork>::Utxo>, NigiriError> {
            parse_json(operation, body, "Bitcoin UTXO list")
        }

        fn parse_tx_info(
            operation: &'static str,
            body: &[u8],
        ) -> Result<<Self as NigiriNetwork>::TxInfo, NigiriError> {
            parse_json(operation, body, "Bitcoin transaction")
        }

        fn parse_address_info(
            operation: &'static str,
            body: &[u8],
        ) -> Result<<Self as NigiriNetwork>::AddressInfo, NigiriError> {
            parse_json(operation, body, "Bitcoin address information")
        }

        fn parse_tx_status(
            operation: &'static str,
            body: &[u8],
        ) -> Result<TxStatus<<Self as NigiriNetwork>::BlockHash>, NigiriError> {
            parse_json(operation, body, "Bitcoin transaction status")
        }
    }

    impl Sealed for super::Liquid {
        fn rpc_prefix() -> &'static [&'static str] {
            &["rpc", "--liquid"]
        }

        fn parse_txid(
            operation: &'static str,
            value: &str,
        ) -> Result<<Self as NigiriNetwork>::Txid, NigiriError> {
            parse_string(operation, value, "Liquid txid")
        }

        fn parse_block_hash(
            operation: &'static str,
            value: &str,
        ) -> Result<<Self as NigiriNetwork>::BlockHash, NigiriError> {
            parse_string(operation, value, "Liquid block hash")
        }

        fn parse_address(
            operation: &'static str,
            value: &str,
        ) -> Result<<Self as NigiriNetwork>::Address, NigiriError> {
            let address: elements::Address = parse_string(operation, value, "Liquid address")?;
            if address.params != &elements::AddressParams::ELEMENTS {
                return Err(invalid(operation, "Liquid address was not for regtest"));
            }
            Ok(address)
        }

        fn parse_utxos(
            operation: &'static str,
            body: &[u8],
        ) -> Result<Vec<<Self as NigiriNetwork>::Utxo>, NigiriError> {
            parse_json(operation, body, "Liquid UTXO list")
        }

        fn parse_tx_info(
            operation: &'static str,
            body: &[u8],
        ) -> Result<<Self as NigiriNetwork>::TxInfo, NigiriError> {
            parse_json(operation, body, "Liquid transaction")
        }

        fn parse_address_info(
            operation: &'static str,
            body: &[u8],
        ) -> Result<<Self as NigiriNetwork>::AddressInfo, NigiriError> {
            parse_json(operation, body, "Liquid address information")
        }

        fn parse_tx_status(
            operation: &'static str,
            body: &[u8],
        ) -> Result<TxStatus<<Self as NigiriNetwork>::BlockHash>, NigiriError> {
            parse_json(operation, body, "Liquid transaction status")
        }
    }

    fn parse_string<T>(
        operation: &'static str,
        value: &str,
        expected: &'static str,
    ) -> Result<T, NigiriError>
    where
        T: std::str::FromStr,
    {
        value
            .trim()
            .trim_matches('"')
            .parse()
            .map_err(|_| invalid(operation, expected))
    }

    fn parse_json<T>(
        operation: &'static str,
        body: &[u8],
        expected: &'static str,
    ) -> Result<T, NigiriError>
    where
        T: DeserializeOwned,
    {
        serde_json::from_slice(body).map_err(|_| invalid(operation, expected))
    }

    fn invalid(operation: &'static str, expected: &'static str) -> NigiriError {
        NigiriError::InvalidResponse {
            operation,
            detail: format!("expected {expected}"),
        }
    }
}
