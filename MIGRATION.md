# Migrating nigiri-rs

## From 0.3.x to 0.4.0

Version 0.4.0 moves all public node operations to direct JSON-RPC against an already-running Nigiri node. The host still owns lifecycle, synchronization, and recovery; the library has no CLI fallback for public calls.

### Raw RPC parameters are JSON values

`NigiriClient::rpc` is now `rpc<R, P>` where `P: serde::Serialize`. The response remains selected by `R`, but parameters are sent as JSON rather than CLI strings. Use `()` for no parameters, tuples for positional parameters, arrays for JSON arrays, and serializable records where the RPC accepts named parameters.

```rust
// Before: a CLI string that the wrapper could coerce.
let hash: bitcoin::BlockHash = client.rpc("getblockhash", ["100"]).await?;

// After: a JSON number in a positional parameter array.
let hash: bitcoin::BlockHash = client.rpc("getblockhash", (100_u64,)).await?;
```

In particular, `"100"` is a JSON string, not a number. `()` is encoded as the empty array `[]`, not JSON `null`.

### Configuration names and defaults changed

The response-size field and constants were renamed because the same limit now applies to node JSON-RPC, Chopsticks, and Esplora response bodies:

| 0.3.x | 0.4.0 |
| --- | --- |
| `max_rpc_response_bytes` | `max_response_bytes` |
| `DEFAULT_MAX_RPC_RESPONSE_BYTES` | `DEFAULT_MAX_RESPONSE_BYTES` |
| `MAX_RPC_RESPONSE_BYTES_LIMIT` | `MAX_RESPONSE_BYTES_LIMIT` |

`NigiriConfig` also has `node_rpc_url`, `node_rpc_user`, and `node_rpc_password`. `NigiriConfig::default()` supplies Bitcoin's Nigiri endpoints, including `http://localhost:18443/` with the public regtest credentials `admin1` / `123`; `NigiriClient::<Liquid>::new()` supplies Liquid's `http://localhost:18884/` endpoint. Prefer update syntax so future public fields do not break your literal:

```rust
use nigiri_rs::{DEFAULT_MAX_RESPONSE_BYTES, NigiriConfig};

let config = NigiriConfig {
    max_response_bytes: 4 * DEFAULT_MAX_RESPONSE_BYTES,
    ..Default::default()
};
```

`timeout` now bounds the HTTP request and response operation against an already-running service. A timeout does not prove that a mutating request did not reach the node.

### Error and mint result patterns changed

JSON-RPC node errors preserve the node code and message. Update destructuring patterns accordingly:

```rust
// Before
let NigiriError::RpcFailed { exit_code, stderr, .. } = error else { return };

// After
let NigiriError::RpcFailed { code, message, .. } = error else { return };
```

`NigiriError::ProcessSpawn` is removed. Transport errors are reported as HTTP transport, HTTP status, timeout, invalid response, invalid request, or JSON-RPC failure errors instead.

`MintResponse::issuance_txin` is now guaranteed when `mint` succeeds:

```rust
// Before
let input: Option<nigiri_rs::IssuanceTxIn> = minted.issuance_txin;

// After
let input: nigiri_rs::IssuanceTxIn = minted.issuance_txin;
```

Liquid `mint` now derives its asset identifier from the contract submitted to Elements' `issueasset`, then sends the asset with `sendtoaddress`. Identical mint inputs intentionally produce a different asset identifier from `nigiri mint`. The two calls are non-atomic: if issuance succeeds and the send fails, inspect the node before retrying because another attempt can issue another asset.

Existing curated methods remain available; prefer them where their native contracts cover the operation. Enable `bitcoin-rpc-types` to use the optional `corepc-types` re-export. Nigiri v0.5.16 runs Bitcoin Core v30.0, so its verified response module is `nigiri_rs::bitcoin_rpc_types::v30`.

## From 0.1.x to 0.2.0

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

In version 0.2.0, there was no public raw RPC method. Public wrappers parsed their exact expected response into native types.

The host must start and stop Nigiri. Version 0.2.0 does not start containers, detect-and-skip a missing host, delete datadirs, or clean up Nigiri from `Drop`.
