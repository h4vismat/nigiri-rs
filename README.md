# nigiri-rs

`nigiri-rs` is a typed asynchronous client for compatible Bitcoin and Liquid regtest services. A host-owned [Nigiri](https://github.com/vulpemventures/nigiri) environment is one compatible setup, not the only architecture.

Version 0.3.0 sends node requests directly over JSON-RPC. It retains the public, type-directed `rpc<R, P>()` escape hatch for Bitcoin and Liquid, including an optional Bitcoin Core v30 response-type re-export. The curated network APIs retain their stronger native contracts.

Version 0.2.0 was the breaking release that introduced network marker types selecting native `bitcoin` or `elements` identifiers, addresses, hashes, and crate-owned Esplora response records at compile time.

## Lifecycle ownership

The host owns the complete lifecycle of its regtest services. This library provides readiness checks but never:

- starts or stops services;
- invokes Docker or Testcontainers;
- provisions or deletes service data directories;
- removes containers or volumes;
- performs cleanup from `Drop`.

Host-owned Nigiri remains a compatible setup; start it before using the default endpoints or an explicitly ignored host test. The forthcoming `nigiri-testcontainers` companion crate will provide fixture lifecycle separately, without adding a Docker or Testcontainers dependency to this core crate.

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

| Network | Node JSON-RPC | Esplora/electrs |
| --- | --- | --- |
| Bitcoin | `http://localhost:18443/` | `http://localhost:30000/` |
| Liquid | `http://localhost:18884/` | `http://localhost:30001/` |

The default node credentials are the public Nigiri regtest credentials: user `admin1`, password `123`. They are intentionally visible in `NigiriConfig`'s derived `Debug` output; they are not production secrets.

HTTP Basic authentication does not encrypt credentials. The built-in HTTP node URLs are loopback-only regtest defaults; use HTTPS or an isolated trusted network when configuring a remote Nigiri host.

## Advanced typed RPC

Both network clients expose `rpc<R, P>()` for node methods not covered by the curated API. Parameters are serialized as real JSON, so select a Rust shape that matches the node method's JSON parameter schema:

```rust,no_run
use nigiri_rs::{Bitcoin, NigiriClient};

# async fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::new();
let height: u64 = client.rpc("getblockcount", ()).await?;
let hundredth_hash: bitcoin::BlockHash = client.rpc("getblockhash", (100_u64,)).await?;
# let _ = height;
# let _ = hundredth_hash;
# Ok(())
# }
```

The caller selects the response type. Existing curated methods remain preferable when available because they guarantee method-specific native contracts. Advanced callers may deliberately select `String` or `serde_json::Value`. The method name may be computed at runtime and must use ASCII letters, digits, and underscores.

`()` is normalized to the empty JSON array `[]`; a tuple becomes a positional JSON array; arrays remain JSON arrays; and any serializable record can provide named parameters:

```rust,no_run
use nigiri_rs::{Bitcoin, NigiriClient};
use serde::Serialize;

#[derive(Serialize)]
struct BlockTemplateOptions {
    rules: [&'static str; 1],
}

# async fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::new();
let _: serde_json::Value = client.rpc("listunspent", (1_u64, 9_999_999_u64, ["bcrt1qexample"])).await?;
let _: serde_json::Value = client.rpc("getblocktemplate", BlockTemplateOptions { rules: ["segwit"] }).await?;
# Ok(())
# }
```

Unlike the former CLI transport, JSON-RPC does not coerce strings according to a method's argument schema. For example, `"100"` is a JSON string and is not a substitute for the JSON number `100`.

### Response size limit

One `NigiriConfig::max_response_bytes` limit applies to every response body: node JSON-RPC and Esplora. It defaults to `DEFAULT_MAX_RESPONSE_BYTES` (64 KiB) and is capped at `MAX_RESPONSE_BYTES_LIMIT` (16 MiB). Anything past the configured limit is rejected rather than buffered. Raise it for methods with large results:

```rust,no_run
use nigiri_rs::{Bitcoin, DEFAULT_MAX_RESPONSE_BYTES, NigiriClient, NigiriConfig};

# fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
    max_response_bytes: 4 * DEFAULT_MAX_RESPONSE_BYTES,
    ..Default::default()
})?;
# let _ = client;
# Ok(())
# }
```

Arbitrary RPC methods may mutate node wallets or active chain state. Tests using mutating RPCs must coordinate host access and restore valid state. This API does not start, stop, delete, or otherwise manage Nigiri.

`faucet` writes directly through the configured node wallet RPC and `broadcast_tx` writes directly through the configured node RPC. Each first commits its transaction, then mines exactly one block. If that mining step fails after a commit, both return `NigiriError::PostTransactionMiningFailed` with the committed transaction ID; inspect node state before retrying.

`NigiriConfig::timeout` bounds each HTTP operation against an already-running node or service. A timeout says the client did not receive a response in time; a mutating request may still have committed, so inspect node state before retrying.

### Bitcoin Core v30 response types

Enable the optional re-export when the caller wants maintained Bitcoin Core response records:

```toml
nigiri-rs = { version = "0.3.0", features = ["bitcoin-rpc-types"] }
```

```rust,no_run
use nigiri_rs::{Bitcoin, NigiriClient, bitcoin_rpc_types};

# async fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::new();
let info: bitcoin_rpc_types::v30::GetBlockchainInfo = client
    .rpc("getblockchaininfo", ())
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
    .rpc("getblockchaininfo", ())
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

`NigiriClient<Liquid>` additionally provides typed asset minting and asset faucet operations. These methods do not exist on the Bitcoin client. `mint` derives the asset identifier from the JSON contract it submits to Elements, then calls `issueasset` and `sendtoaddress`. Those calls are not atomic: if sending fails after issuance, the asset already exists. Inspect node state before retrying; retrying can create another asset.

### Deliberate scope limits

Nigiri v0.5.16's default Liquid regtest configuration does not enable PAK enforcement. Consequently, `initpegoutwallet` and `sendtomainchain` fail on the default environment and are not wrapped by this crate. `nigiri-rs` also does not simulate peg-in or peg-out behavior with unrelated transfers.

The crate models only capabilities that the verified default Nigiri networks can execute. Custom federation lifecycle, chain configuration, and cross-chain orchestration remain the host application's responsibility.

## Custom configuration

```rust
use std::time::Duration;
use nigiri_rs::{Bitcoin, NigiriClient, NigiriConfig};
use url::Url;

let config = NigiriConfig {
    esplora_url: Url::parse("http://regtest-host:4301")?,
    node_rpc_url: Url::parse("http://regtest-host:18443")?,
    node_rpc_user: "admin1".to_owned(),
    node_rpc_password: "123".to_owned(),
    timeout: Duration::from_secs(20),
    ..Default::default()
};

let client = NigiriClient::<Bitcoin>::with_config(config)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Construction accepts only HTTP(S) base URLs, normalizes their trailing slash, rejects query/fragment components, and requires a nonzero timeout and response limit. `NigiriConfig::default()` provides the Bitcoin endpoint and public regtest node credentials; `NigiriClient::<Liquid>::new()` selects the Liquid defaults. Cloning a client clones only immutable configuration and the shared HTTP transport; it never implies ownership of an external process.

`NigiriConfig::default()` is Bitcoin-specific. For a custom `NigiriClient<Liquid>`, override the Esplora and node JSON-RPC URLs rather than relying on struct update syntax alone.

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

Address inputs used as HTTP paths or JSON-RPC parameters remain `&str`, which avoids unnecessary conversions for LWK confidential addresses. Monetary Bitcoin and L-BTC values use `bitcoin::Amount`; JSON serialization is exact decimal BTC and never passes through `f64`.

Liquid-only methods exist solely on `NigiriClient<Liquid>`:

- `mint`;
- `faucet_asset`.

They cannot be called on `NigiriClient<Bitcoin>`; compile-fail documentation tests enforce this boundary.

The dependency family is aligned with LWK 0.18.1: `elements 0.25.3` and compatible `bitcoin 0.32.x` types. `nigiri-rs` does not depend on LWK.

## Tests

Pure parsers, JSON-RPC request construction, exact amounts, HTTP bounds, error mapping, and network types run in the ordinary suite:

```sh
cargo test
cargo test --doc
```

Bitcoin integration tests need Docker but no Nigiri installation. Each one starts its own funded regtest stack through `nigiri-testcontainers`, owns its chain, and removes everything it created when it finishes:

```sh
cargo test -p nigiri-testcontainers --test bitcoin_fixture -- --ignored --test-threads=1
```

Because a fixture owns its chain, those tests need no cross-process mutation lock: a reorg in one is invisible to every other.

One fixture is ready in about 6 seconds, well inside the 60-second default startup budget. Two started at once measured 103 seconds: the cost is contention, not queueing, because both nodes mine 101 blocks while both indexers follow them. Raise `startup_timeout` when starting fixtures in parallel.

Liquid has no fixture yet, so its integration tests still reuse a host Nigiri chain and do acquire an exclusive cross-process mutation lock. They never stop or delete Nigiri:

```sh
cargo test --test host_liquid -- --ignored --test-threads=1
```

The Liquid Esplora endpoint must be ready first. When reusing a stale regtest chain, the host may need to mine a fresh block so the node leaves initial block download and electrs begins serving requests. The Liquid reorg test records its baseline, invalidates only a tip it created, reconsiders it before releasing the lock, and leaves a valid active chain.

## License

MIT
