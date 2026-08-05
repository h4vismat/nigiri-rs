#![cfg(feature = "bitcoin-rpc-types")]

use serde::de::DeserializeOwned;

fn assert_deserialize_owned<T: DeserializeOwned>() {}

#[test]
fn reexport_exposes_bitcoin_core_v30_response_types() {
    assert_deserialize_owned::<nigiri_rs_core::bitcoin_rpc_types::v30::GetBlockchainInfo>();
}
