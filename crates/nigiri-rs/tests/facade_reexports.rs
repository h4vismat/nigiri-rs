//! Every path a 0.2.0 consumer could import must still resolve through the facade.
//!
//! The facade re-exports with a glob, which makes an accidental omission invisible in this
//! workspace: nothing here would fail to compile, and the breakage would only appear in a
//! downstream crate. Naming the paths explicitly is what turns that into a local failure.

use nigiri_rs::{
    AddressStats, Bitcoin, BitcoinAddressInfo, BitcoinTxInfo, BitcoinUtxo,
    DEFAULT_MAX_RESPONSE_BYTES, ElectrumEndpoint, IssuanceTxIn, LBTC_REGTEST_ASSET, Liquid,
    LiquidAddressInfo, LiquidTxInfo, LiquidUtxo, MAX_RESPONSE_BYTES_LIMIT, MintResponse,
    NigiriClient, NigiriConfig, NigiriError, NigiriNetwork, TxStatus,
};

// Catches a dropped re-export in the facade. Every item below was public at 0.2.0; naming it in a
// type position forces the compiler to resolve it.
#[test]
fn every_published_path_still_resolves() {
    fn accepts<T>() {}

    accepts::<NigiriClient<Bitcoin>>();
    accepts::<NigiriClient<Liquid>>();
    accepts::<NigiriConfig>();
    accepts::<NigiriError>();
    accepts::<ElectrumEndpoint>();
    accepts::<AddressStats>();
    accepts::<BitcoinAddressInfo>();
    accepts::<BitcoinTxInfo>();
    accepts::<BitcoinUtxo>();
    accepts::<IssuanceTxIn>();
    accepts::<LiquidAddressInfo>();
    accepts::<LiquidTxInfo>();
    accepts::<LiquidUtxo>();
    accepts::<MintResponse>();
    accepts::<TxStatus<bitcoin::BlockHash>>();

    let _: usize = DEFAULT_MAX_RESPONSE_BYTES;
    let _: usize = MAX_RESPONSE_BYTES_LIMIT;
    assert_eq!(LBTC_REGTEST_ASSET.to_string().len(), 64);

    fn is_network<N: NigiriNetwork>() {}
    is_network::<Bitcoin>();
    is_network::<Liquid>();
}

// Catches the fixtures being re-exported unconditionally, which would drag Docker dependencies
// into a client-only build.
#[cfg(feature = "testcontainers")]
#[test]
fn fixtures_are_reachable_when_the_feature_is_on() {
    fn accepts<T>() {}
    accepts::<nigiri_rs::testcontainers::Fixture<Bitcoin>>();
    accepts::<nigiri_rs::testcontainers::FixtureError>();
}

// Catches a broken feature forward in the facade manifest. `bitcoin_rpc_types` is the only core
// export that the glob re-export cannot carry on its own: it is gated in core, so it arrives here
// only if `bitcoin-rpc-types = ["nigiri-rs-core/bitcoin-rpc-types"]` is right. Nothing else in the
// workspace would notice that line breaking.
#[cfg(feature = "bitcoin-rpc-types")]
#[test]
fn gated_rpc_types_reach_the_facade() {
    fn accepts<T>() {}
    accepts::<nigiri_rs::bitcoin_rpc_types::v30::GetBlockchainInfo>();
}
