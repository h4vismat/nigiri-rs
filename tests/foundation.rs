use std::{path::PathBuf, time::Duration};

use bitcoin::address::NetworkChecked;
use nigiri_rs::{
    Bitcoin, BitcoinAddressInfo, BitcoinTxInfo, BitcoinUtxo, DEFAULT_MAX_RPC_RESPONSE_BYTES,
    Liquid, LiquidAddressInfo, LiquidTxInfo, LiquidUtxo, NigiriClient, NigiriConfig, NigiriError,
    NigiriNetwork,
};
use url::Url;

fn assert_bitcoin_types<N>()
where
    N: NigiriNetwork<
            Txid = bitcoin::Txid,
            BlockHash = bitcoin::BlockHash,
            Address = bitcoin::Address<NetworkChecked>,
            Utxo = BitcoinUtxo,
            TxInfo = BitcoinTxInfo,
            AddressInfo = BitcoinAddressInfo,
        >,
{
}

fn assert_liquid_types<N>()
where
    N: NigiriNetwork<
            Txid = elements::Txid,
            BlockHash = elements::BlockHash,
            Address = elements::Address,
            Utxo = LiquidUtxo,
            TxInfo = LiquidTxInfo,
            AddressInfo = LiquidAddressInfo,
        >,
{
}

#[test]
fn explicit_network_markers_select_native_types_and_defaults() {
    assert_bitcoin_types::<Bitcoin>();
    assert_liquid_types::<Liquid>();

    let bitcoin = NigiriClient::<Bitcoin>::new();
    let liquid = NigiriClient::<Liquid>::new();

    assert_eq!(bitcoin.esplora_url().as_str(), "http://localhost:30000/");
    assert_eq!(liquid.esplora_url().as_str(), "http://localhost:30001/");
}

#[test]
fn custom_configuration_is_normalized_once() {
    let config = NigiriConfig {
        chopsticks_url: Url::parse("http://127.0.0.1:4100/api").unwrap(),
        esplora_url: Url::parse("http://127.0.0.1:4200/api").unwrap(),
        executable: PathBuf::from("/opt/nigiri"),
        timeout: Duration::from_secs(7),
        max_rpc_response_bytes: DEFAULT_MAX_RPC_RESPONSE_BYTES,
    };

    let client = NigiriClient::<Bitcoin>::with_config(config).unwrap();

    assert_eq!(client.esplora_url().as_str(), "http://127.0.0.1:4200/api/");
}

#[test]
fn invalid_configuration_is_rejected() {
    let config = NigiriConfig {
        chopsticks_url: Url::parse("ftp://127.0.0.1/faucet").unwrap(),
        esplora_url: Url::parse("http://127.0.0.1:4200").unwrap(),
        executable: PathBuf::from("nigiri"),
        timeout: Duration::from_secs(7),
        max_rpc_response_bytes: DEFAULT_MAX_RPC_RESPONSE_BYTES,
    };

    let error = NigiriClient::<Liquid>::with_config(config).unwrap_err();
    assert!(
        matches!(error, NigiriError::InvalidRequest { .. }),
        "expected a pre-spawn rejection, got {error}"
    );
}

#[test]
fn bitcoin_esplora_records_use_native_identifiers_hashes_and_amounts() {
    let json = r#"{
        "txid":"1111111111111111111111111111111111111111111111111111111111111111",
        "vout":2,
        "value":125000,
        "status":{
            "confirmed":true,
            "block_height":101,
            "block_hash":"2222222222222222222222222222222222222222222222222222222222222222",
            "block_time":1700000000
        }
    }"#;

    let utxo: BitcoinUtxo = serde_json::from_str(json).unwrap();

    assert_eq!(utxo.txid.to_string(), "11".repeat(32));
    assert_eq!(utxo.value, bitcoin::Amount::from_sat(125_000));
    assert_eq!(utxo.status.block_hash.unwrap().to_string(), "22".repeat(32));
}

#[test]
fn liquid_esplora_records_parse_native_assets_and_confidential_outputs() {
    let asset_tag = elements::secp256k1_zkp::Tag::from([7_u8; 32]);
    let asset_commitment = elements::secp256k1_zkp::Generator::new_unblinded(
        elements::secp256k1_zkp::SECP256K1,
        asset_tag,
    );
    let json = serde_json::json!({
        "txid": "3333333333333333333333333333333333333333333333333333333333333333",
        "vout": 0,
        "valuecommitment": "0907a63fabe3e49d5713e9dafcabfecae48a137c1a1d832a21d49797590087c9fe",
        "assetcommitment": asset_commitment.to_string(),
        "asset": "5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225",
        "status": {"confirmed": false}
    });

    let utxo: LiquidUtxo = serde_json::from_value(json).unwrap();

    assert_eq!(utxo.txid.to_string(), "33".repeat(32));
    assert_eq!(
        utxo.asset.unwrap().to_string(),
        "5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225"
    );
    assert!(utxo.value_commitment.is_some());
    assert_eq!(utxo.asset_commitment, Some(asset_commitment));
    assert!(utxo.value.is_none());
}
