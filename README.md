# nigiri-rs

`nigiri-rs` is a typed asynchronous client for Bitcoin and Liquid services in an already-running [Nigiri](https://github.com/vulpemventures/nigiri) regtest environment.

Version 0.2.0 is a breaking release. Network marker types select native `bitcoin` or `elements` identifiers, addresses, hashes, and crate-owned Esplora response records at compile time.

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

The RPC executor is private. The public API exposes only typed fixture operations such as address creation, mining, invalidation, and reconsideration. Arbitrary raw RPC access is intentionally absent.

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

Host integration tests are always explicit and never silently skip. They reuse the existing host chain, acquire an exclusive cross-process mutation lock, and do not stop or delete Nigiri:

```sh
cargo test --test host_bitcoin -- --ignored --test-threads=1
cargo test --test host_liquid -- --ignored --test-threads=1
```

The reorg tests record their baseline, invalidate only a tip created by that test, reconsider it before releasing the lock, and leave a valid active chain.

## Migrating from 0.1.x

See [MIGRATION.md](MIGRATION.md) for the breaking API changes.

## License

MIT
