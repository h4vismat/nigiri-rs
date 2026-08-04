//! Every path a 0.2.0 consumer could import must still resolve through the facade.
//!
//! The facade re-exports with a glob, which makes an accidental omission invisible in this
//! workspace: nothing here would fail to compile, and the breakage would only appear in a
//! downstream crate. Naming the paths explicitly is what turns that into a local failure.

use nigiri_rs::{
    AddressStats, Bitcoin, BitcoinAddressInfo, BitcoinTxInfo, BitcoinUtxo,
    DEFAULT_MAX_RESPONSE_BYTES, IssuanceTxIn, LBTC_REGTEST_ASSET, Liquid, LiquidAddressInfo,
    LiquidTxInfo, LiquidUtxo, MAX_RESPONSE_BYTES_LIMIT, MintResponse, NigiriClient, NigiriConfig,
    NigiriError, NigiriNetwork, TxStatus,
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

    // Both operands are `pub const` in `nigiri-rs-core`, so clippy const-folds the comparison and
    // flags it as `assertions_on_constants`. The assertion still earns its keep: it is a
    // regression guard on the *re-exported values*, which could change in a future
    // `nigiri-rs-core` release even though they are compile-time constants today.
    #[allow(clippy::assertions_on_constants)]
    {
        assert!(DEFAULT_MAX_RESPONSE_BYTES > 0);
        assert!(MAX_RESPONSE_BYTES_LIMIT >= DEFAULT_MAX_RESPONSE_BYTES);
    }
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
