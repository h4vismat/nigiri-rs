# nigiri-rs

`nigiri-rs` is a typed asynchronous client for Bitcoin and Liquid services in an already-running [Nigiri](https://github.com/vulpemventures/nigiri) regtest environment.

Version 0.3.0 adds a public, type-directed `rpc<R>()` escape hatch for Bitcoin and Liquid, including an optional Bitcoin Core v30 response-type re-export. The curated network APIs retain their stronger native contracts.

Version 0.2.0 was the breaking release that introduced network marker types selecting native `bitcoin` or `elements` identifiers, addresses, hashes, and crate-owned Esplora response records at compile time.

## Lifecycle ownership

The host owns Nigiri's complete lifecycle. This library provides readiness checks but never:

- starts or stops Nigiri;
- invokes Docker;
- provisions or deletes datadirs;
- removes containers or volumes;
- performs cleanup from `Drop`.

Start the required services before running a client or an explicitly ignored host test:

```sh
# Bitcoin only
nigiri start

# Bitcoin and Liquid
nigiri start --liquid
```

The verified CLI and port contract is Nigiri v0.5.16, commit `39fd5891d093bfb8c2575b79640b95a830834f9c`.

## Quick start

```rust,no_run
use bitcoin::Amount;
use nigiri_rs::{Bitcoin, Liquid, NigiriClient};

# async fn example() -> Result<(), nigiri_rs::NigiriError> {
let bitcoin = NigiriClient::<Bitcoin>::new();
let liquid = NigiriClient::<Liquid>::new();

bitcoin.wait_ready().await?;
liquid.wait_ready().await?;

let bitcoin_address = bitcoin.new_address().await?;
let bitcoin_address_text = bitcoin_address.to_string();
let bitcoin_txid = bitcoin
    .faucet(&bitcoin_address_text, Some(Amount::from_sat(100_000)))
    .await?;

let liquid_address = liquid.new_address().await?;
let liquid_txid = liquid
    .faucet(&liquid_address.to_string(), Some(Amount::from_sat(100_000)))
    .await?;

println!("Bitcoin funding: {bitcoin_txid}");
println!("Liquid funding: {liquid_txid}");
# Ok(())
# }
```

There is deliberately no default generic parameter. `NigiriClient::new()` without an explicit network is not supported.

## Verified default endpoints

| Network | Chopsticks | Esplora/electrs |
| --- | --- | --- |
| Bitcoin | `http://localhost:3000/` | `http://localhost:30000/` |
| Liquid | `http://localhost:3001/` | `http://localhost:30001/` |

Nigiri v0.5.16 invokes node RPCs in these forms:

```text
nigiri rpc <method> <args...>
nigiri rpc --liquid <method> <args...>
```

## Advanced typed RPC

Both network clients expose `rpc<R>()` for node methods not covered by the curated API. Arguments use Nigiri's CLI ordering and must be passed separately:

```rust,no_run
use nigiri_rs::{Bitcoin, NigiriClient};

# async fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::new();
let height: u64 = client
    .rpc("getblockcount", std::iter::empty::<&str>())
    .await?;
let hundredth_hash: bitcoin::BlockHash = client.rpc("getblockhash", ["100"]).await?;
# let _ = height;
# let _ = hundredth_hash;
# Ok(())
# }
```

The caller selects the response type. Existing curated methods remain preferable when available because they guarantee method-specific native contracts. Advanced callers may deliberately select `String` or `serde_json::Value`. The method name may be computed at runtime; it is validated against an ASCII letter, digit, and underscore charset before any process is spawned.

### Response size limit

A single stdout or stderr stream is retained up to `NigiriConfig::max_rpc_response_bytes`, which defaults to `DEFAULT_MAX_RPC_RESPONSE_BYTES` (64 KiB) and is capped at `MAX_RPC_RESPONSE_BYTES_LIMIT` (16 MiB). Anything past the configured limit is rejected and the child is killed rather than buffered. Raise it for methods with large results:

```rust,no_run
use nigiri_rs::{Bitcoin, DEFAULT_MAX_RPC_RESPONSE_BYTES, NigiriClient, NigiriConfig};

# fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
    chopsticks_url: "http://localhost:3000".parse().unwrap(),
    esplora_url: "http://localhost:30000".parse().unwrap(),
    executable: "nigiri".into(),
    timeout: std::time::Duration::from_secs(30),
    max_rpc_response_bytes: 4 * DEFAULT_MAX_RPC_RESPONSE_BYTES,
})?;
# let _ = client;
# Ok(())
# }
```

A method that exits zero, writes nothing to stdout, and writes non-whitespace content to stderr is reported as `NigiriError::RpcFailed`, because that is how the node CLIs surface some errors. Whitespace-only stderr does not fail a void result. Keep the host `nigiri` wrapper's stderr free of unrelated noise or void RPCs will report spurious failures.

Arbitrary RPC methods may mutate node wallets or active chain state. Tests using mutating RPCs must coordinate host access and restore valid state. This API does not start, stop, delete, or otherwise manage Nigiri.

On timeout or a stream-limit breach the crate kills and reaps the child it spawned. Because real `nigiri` is a shell wrapper around `docker`, a `docker exec` it already started is not in that process group and runs to completion, so a mutating RPC that times out may still commit on the node.

### Bitcoin Core v30 response types

Enable the optional re-export when the caller wants maintained Bitcoin Core response records:

```toml
nigiri-rs = { version = "0.3", features = ["bitcoin-rpc-types"] }
```

```rust,no_run
use nigiri_rs::{Bitcoin, NigiriClient, bitcoin_rpc_types};

# async fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::new();
let info: bitcoin_rpc_types::v30::GetBlockchainInfo = client
    .rpc("getblockchaininfo", std::iter::empty::<&str>())
    .await?;
# let _ = info;
# Ok(())
# }
```

Nigiri v0.5.16 runs Bitcoin Core v30.0. Custom environments running another Core version must select the matching `corepc-types` module.

### Liquid response records

The `elements` crate supplies native Liquid values; callers own structured RPC records:

```rust,no_run
use nigiri_rs::{Liquid, NigiriClient};
use serde::Deserialize;

#[derive(Deserialize)]
struct BlockchainInfo {
    chain: String,
    blocks: u64,
    bestblockhash: elements::BlockHash,
}

# async fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Liquid>::new();
let info: BlockchainInfo = client
    .rpc("getblockchaininfo", std::iter::empty::<&str>())
    .await?;
# let _ = info;
# Ok(())
# }
```

## Supported API

Both `NigiriClient<Bitcoin>` and `NigiriClient<Liquid>` provide:

- readiness and block-height checks;
- address funding with exact `bitcoin::Amount` values;
- typed UTXO, address, transaction, and confirmation-status queries;
- raw transaction broadcast with a native transaction ID result;
- confirmation polling;
- native address creation and active-tip lookup;
- variable block generation;
- block invalidation and reconsideration.

`NigiriClient<Liquid>` additionally provides typed asset minting and asset faucet operations. These methods do not exist on the Bitcoin client.

### Deliberate scope limits

Nigiri v0.5.16's default Liquid regtest configuration does not enable PAK enforcement. Consequently, `initpegoutwallet` and `sendtomainchain` fail on the default environment and are not wrapped by this crate. `nigiri-rs` also does not simulate peg-in or peg-out behavior with unrelated transfers.

The crate models only capabilities that the verified default Nigiri networks can execute. Custom federation lifecycle, chain configuration, and cross-chain orchestration remain the host application's responsibility.

## Custom configuration

```rust
use std::{path::PathBuf, time::Duration};
use nigiri_rs::{Bitcoin, NigiriClient, NigiriConfig};
use url::Url;

let config = NigiriConfig {
    chopsticks_url: Url::parse("http://regtest-host:4300")?,
    esplora_url: Url::parse("http://regtest-host:4301")?,
    executable: PathBuf::from("/opt/nigiri/bin/nigiri"),
    timeout: Duration::from_secs(20),
};

let client = NigiriClient::<Bitcoin>::with_config(config)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Construction accepts only HTTP(S) base URLs, normalizes their trailing slash, rejects query/fragment components, requires a nonempty executable path, and requires a nonzero timeout. Cloning a client clones only immutable configuration and the shared HTTP transport; it never implies ownership of an external process.

## Typed network differences

Shared methods live on `NigiriClient<N>` and return the associated native types selected by `NigiriNetwork`:

| Contract | Bitcoin | Liquid |
| --- | --- | --- |
| Transaction ID | `bitcoin::Txid` | `elements::Txid` |
| Block hash | `bitcoin::BlockHash` | `elements::BlockHash` |
| New address | checked regtest `bitcoin::Address` | regtest `elements::Address` |
| UTXO | `BitcoinUtxo` | `LiquidUtxo` with explicit/commitment fields |
| Transaction | `BitcoinTxInfo` | `LiquidTxInfo` |
| Address information | `BitcoinAddressInfo` | `LiquidAddressInfo` |

Address inputs used as HTTP paths or CLI arguments remain `&str`, which avoids unnecessary conversions for LWK confidential addresses. Monetary Bitcoin and L-BTC values use `bitcoin::Amount`; serialization is exact decimal BTC and never passes through `f64`.

Liquid-only methods exist solely on `NigiriClient<Liquid>`:

- `mint`;
- `faucet_asset`.

They cannot be called on `NigiriClient<Bitcoin>`; compile-fail documentation tests enforce this boundary.

The dependency family is aligned with LWK 0.18.1: `elements 0.25.3` and compatible `bitcoin 0.32.x` types. `nigiri-rs` does not depend on LWK.

## Tests

Pure parsers, command construction, exact amounts, HTTP bounds, process failure, timeout termination, and network types run in the ordinary suite:

```sh
cargo test
cargo test --doc
```

Host integration tests are always explicit and never silently skip. They reuse the existing host chain and do not stop or delete Nigiri. Mutating tests acquire an exclusive cross-process mutation lock; read-only public RPC tests deliberately run without that lock:

```sh
cargo test --test host_bitcoin -- --ignored --test-threads=1
cargo test --test host_liquid -- --ignored --test-threads=1
```

Both Esplora endpoints must be ready before running the host suites. When reusing a stale regtest chain, the host may need to mine a fresh block so the node leaves initial block download and electrs begins serving requests.

The reorg tests record their baseline, invalidate only a tip created by that test, reconsider it before releasing the lock, and leave a valid active chain.

## Migrating from 0.1.x

See [MIGRATION.md](MIGRATION.md) for the breaking API changes.

## License

MIT
