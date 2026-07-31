# Migrating from nigiri-rs 0.1.x to 0.2.0

Version 0.2.0 intentionally breaks the Liquid-only, stringly typed API.

## Select a network explicitly

```rust
use nigiri_rs::{Bitcoin, Liquid, NigiriClient};

let bitcoin = NigiriClient::<Bitcoin>::new();
let liquid = NigiriClient::<Liquid>::new();
```

The non-generic `NigiriClient::new()` constructor was removed and there is no default network parameter.

## Replace `with_urls`

`with_urls` was removed. Construct a complete `NigiriConfig` containing typed URLs, the Nigiri executable path, and the operation timeout, then call `NigiriClient::<Network>::with_config(config)`.

Unlike `new()`, `with_config` returns `Result` because custom values are validated and normalized.

## Replace strings and floating-point amounts

- Funding and broadcast methods return `bitcoin::Txid` or `elements::Txid`.
- Mining and reorg methods use native `bitcoin::BlockHash` or `elements::BlockHash`.
- `new_address` returns a checked Bitcoin regtest address or an Elements regtest address.
- Faucet amounts are `Option<bitcoin::Amount>`, not `Option<f64>`.
- Esplora methods return typed records rather than `serde_json::Value` or unchecked strings.

Transaction lookups now take references to native transaction identifiers:

```rust,no_run
# use nigiri_rs::{Bitcoin, NigiriClient};
# async fn example(txid: bitcoin::Txid) -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::new();
let status = client.get_tx_status(&txid).await?;
# let _ = status;
# Ok(())
# }
```

## Liquid-only methods

The former synchronous `cli_mint` and `cli_faucet_asset` helpers were removed. Use asynchronous `mint` and `faucet_asset` on `NigiriClient<Liquid>`.

## RPC and lifecycle boundaries

There is no public raw RPC method. Public wrappers parse their exact expected response into native types.

The host must start and stop Nigiri. Version 0.2.0 does not start containers, detect-and-skip a missing host, delete datadirs, or clean up Nigiri from `Drop`.
