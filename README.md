# nigiri-rs

## Crates

| Crate | What it is |
| --- | --- |
| `nigiri-rs` | The facade. Depend on this. Re-exports the client, and the fixtures behind the `testcontainers` feature. |
| `nigiri-rs-core` | The typed Bitcoin and Liquid clients. Re-exported in full by the facade. |
| `nigiri-rs-testcontainers` | Ephemeral Docker-backed regtest fixtures: a node, an indexer, and a funded wallet per test. |
| `nigiri-rs-macros` | Procedural macros for the above. |

The bare `nigiri-*` names on crates.io are deliberately unused: Nigiri is
[Vulpem Ventures'](https://github.com/vulpemventures/nigiri) project, and this is an unaffiliated
Rust port.

`nigiri-rs` is a typed asynchronous client for compatible Bitcoin and Liquid regtest services. A host-owned [Nigiri](https://github.com/vulpemventures/nigiri) environment is one compatible setup, not the only architecture.

Version 0.4.0 sends node requests directly over JSON-RPC. It retains the public, type-directed `rpc<R, P>()` escape hatch for Bitcoin and Liquid, including an optional Bitcoin Core response-type re-export. The curated network APIs retain their stronger native contracts.

Version 0.2.0 was the breaking release that introduced network marker types selecting native `bitcoin` or `elements` identifiers, addresses, hashes, and crate-owned Esplora response records at compile time.

## Documentation

This README is the tour. [`docs/`](docs/README.md) is the depth behind it, organized by what you are
trying to do.

**New here?** [Tutorial: your first fixture-backed test](docs/tutorial-first-test.md) takes you from
an empty crate to a passing test against a throwaway chain. [Tutorial: a round trip across Liquid's
peg](docs/tutorial-peg-round-trip.md) is the next one up: BTC into the sidechain and back out again,
against a wired four-container pair.

| Guide | For |
| --- | --- |
| [Run a throwaway regtest stack](docs/how-to-run-a-fixture.md) | Starting a fixture by hand, tuning its budget, swapping images |
| [Peg in and peg out](docs/how-to-peg.md) | Getting a wired pair, claiming a real peg-in, releasing a simulated peg-out |
| [Point a wallet at a fixture](docs/how-to-connect-a-wallet.md) | Wiring BDK or LWK to the runtime-mapped endpoints |
| [Point at services you run](docs/how-to-point-at-your-own-services.md) | Custom endpoints, credentials, timeouts, response limits |
| [Call any node RPC](docs/how-to-call-any-node-rpc.md) | Methods the curated API does not wrap |
| [Work with Liquid assets](docs/how-to-work-with-liquid-assets.md) | Minting, sending, and reading confidential UTXOs |

| Reference | Covers |
| --- | --- |
| [Client API](docs/reference-client.md) | `NigiriClient`, `NigiriConfig`, `Peg`, response records, network markers |
| [Fixture API](docs/reference-fixtures.md) | `Fixture`, `FixtureBuilder`, `PegPair`, `PegPairBuilder`, `ContainerImage`, `FixtureChain` |
| [`#[nigiri_rs::test]`](docs/reference-test-macro.md) | Arguments, accepted signatures, every rejection message |
| [Errors](docs/reference-errors.md) | `NigiriError` and `FixtureError`, variant by variant |

| Explanation | Question it answers |
| --- | --- |
| [Lifecycle ownership](docs/explanation-lifecycle-ownership.md) | Why the client crate never starts or stops anything |
| [Typed networks](docs/explanation-typed-networks.md) | Why `Bitcoin` and `Liquid` are type parameters |
| [What "ready" means](docs/explanation-fixture-readiness.md) | Why a fixture waits for three services to agree |
| [What the peg simulates](docs/explanation-what-the-peg-simulates.md) | Which half of the peg is real, and what that costs you in assertions |

## Lifecycle ownership

The host owns the complete lifecycle of its regtest services. This library provides readiness checks but never:

- starts or stops services;
- invokes Docker or Testcontainers;
- provisions or deletes service data directories;
- removes containers or volumes;
- performs cleanup from `Drop`.

Host-owned Nigiri remains a compatible setup; start it before pointing a client at the default endpoints. The `nigiri-rs-testcontainers` companion crate in this workspace provides fixture lifecycle separately, and this core crate does not depend on it: no Docker or Testcontainers dependency is added here. Nothing in this repository's own test suite needs a host Nigiri installation any more: both chains' integration tests run against ephemeral `nigiri-rs-testcontainers` fixtures and require only Docker.

Start the required services before pointing a client at them:

```sh
# Bitcoin only
nigiri start

# Bitcoin and Liquid
nigiri start --liquid
```

The verified CLI and port contract is Nigiri v0.5.16, commit `39fd5891d093bfb8c2575b79640b95a830834f9c`.

## Provisioning services

This crate does not start or stop anything. Two paths exist, and they can be used side by side.

**Ephemeral fixtures.** The companion `nigiri-rs-testcontainers` crate, reached through the facade's `testcontainers` feature, starts a throwaway Bitcoin or Liquid regtest stack for a test and removes it afterwards:

```toml
[dev-dependencies]
nigiri-rs = { version = "0.5", features = ["testcontainers"] }
```

```rust
use nigiri_rs::testcontainers::{Bitcoin, Fixture};

# async fn example() -> Result<(), nigiri_rs::testcontainers::FixtureError> {
let fixture = Fixture::<Bitcoin>::start().await?;
let client = fixture.client();
let electrum_host = fixture.electrum_endpoint().host();
let electrum_port = fixture.electrum_endpoint().port();
# let _ = (client, electrum_host, electrum_port);
# Ok(())
# }
```

Docker must be running; no Nigiri installation is needed. Containers, their anonymous volumes, and the network are removed when the fixture is dropped. Ports are assigned by the runtime, so read them from the fixture instead of assuming Nigiri's fixed ones. The first start on a machine pulls two pinned images per chain and is slow; later starts are ready in a few seconds. `Fixture::<Liquid>::start` starts the same way; swap the type parameter. Podman is untested.

**One attribute instead of a preamble.** The same feature provides `#[nigiri_rs::test]`, which starts a fixture per parameter and hands the body a ready client. It needs the `testcontainers` feature and Docker; nothing else:

```rust,ignore
use nigiri_rs::{Bitcoin, NigiriClient};

#[nigiri_rs::test]
async fn my_wallet_sees_its_funding(client: NigiriClient<Bitcoin>) -> Result<(), Box<dyn std::error::Error>> {
    // `client` is already pointed at a funded, synchronized stack.
    let address = client.new_address().await?;
    client.faucet(&address.to_string(), None).await?;

    // Point a wallet library at either endpoint; both report runtime-mapped ports.
    let _esplora = client.esplora_url();
    let _electrum = client.electrum_endpoint();
    Ok(())
}
```

One fixture is started per parameter, so a cross-chain test takes two: add a `NigiriClient<Liquid>` alongside the Bitcoin one. The chain comes from the parameter type, never an attribute argument, so the two cannot disagree. `startup_timeout = <seconds>` and `flavor = "multi_thread"` are accepted. Tests are not `#[ignore]`d — if Docker is unavailable they fail loudly rather than reporting green having run nothing.

The Electrum endpoint above is `fixture.electrum_endpoint()`, which delegates to the client. Any `NigiriClient<N>`, fixture-backed or not, exposes both endpoints a BDK or LWK wallet needs directly:

```rust,no_run
use nigiri_rs::{Bitcoin, NigiriClient};

# fn example() {
let client = NigiriClient::<Bitcoin>::new();
let esplora_url = client.esplora_url();
let electrum_host = client.electrum_endpoint().host();
let electrum_port = client.electrum_endpoint().port();
println!("esplora: {esplora_url}, electrum: {electrum_host}:{electrum_port}");
# }
```

**Services you run yourself.** Point `NigiriClient` at a host-owned Nigiri installation or any compatible endpoints, as the quick start below does. Nothing in this crate starts, stops, or deletes them.

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

| Network | Node JSON-RPC | Esplora/electrs | Electrum |
| --- | --- | --- | --- |
| Bitcoin | `http://localhost:18443/` | `http://localhost:30000/` | `localhost:50000` |
| Liquid | `http://localhost:18884/` | `http://localhost:30001/` | `localhost:50001` |

A fixture replaces the Electrum entry with its runtime-mapped port, so read it from the client rather than assuming the default.

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

### Bitcoin Core response types

Enable the optional re-export when the caller wants maintained Bitcoin Core response records:

```toml
nigiri-rs = { version = "0.5", features = ["bitcoin-rpc-types"] }
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

Nigiri v0.5.16 runs Bitcoin Core v30.0 (`v30`); the pinned fixture image runs v31.0 (`v31`). Custom environments running another Core version must select the matching `corepc-types` module.

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

Liquid's peg is covered by `Peg`, and half of it is real. Peg-in is genuine end to end: a real federation-controlled address, a real `claimpegin` with a real merkle proof. Peg-out is split — `sendtomainchain` is a genuine Elements call that genuinely burns L-BTC, but regtest has no federation to service it, so `release_peg_out` plays that part. **That release is a simulation with no reserve:** the BTC comes from the Bitcoin node's own wallet, total BTC on the mainchain side grows with every release, and no 1:1 invariant holds across the pair. `initpegoutwallet` is deliberately not wrapped: this chain runs with PAK enforcement off, so the node rejects the call outright, and `sendtomainchain` does not need it. See [Client API](docs/reference-client.md#peg) for the whole surface and [`PegPair`](docs/reference-fixtures.md#pegpair) for a wired four-container pair to run it against.

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

`NigiriConfig::default()` is Bitcoin-specific, so build a Liquid config from `NigiriConfig::liquid()` rather than from `Default`:

```rust
use nigiri_rs::{Liquid, NigiriClient, NigiriConfig};

let config = NigiriConfig {
    node_rpc_password: "something-else".to_owned(),
    ..NigiriConfig::liquid()
};

let client = NigiriClient::<Liquid>::with_config(config)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Do not reach for `..Default::default()` and override the two service URLs by hand. Rust evaluates `default()` before applying any override, so every chain-dependent field you do not name keeps Bitcoin's value — including the Electrum endpoint, which would leave a Liquid config pointing at port 50000. `NigiriConfig::bitcoin()` and `NigiriConfig::liquid()` exist as public constructors for exactly this reason, and `crates/nigiri-rs-core/src/config.rs` has a test pinning the trap.

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

Pure parsers, JSON-RPC request construction, exact amounts, HTTP bounds, error mapping, and network types need no Docker. A contributor without Docker installed runs:

```sh
cargo test -p nigiri-rs-core --all-targets
cargo test --doc
```

That is also exactly what three of this repository's four CI matrix cells run. A plain workspace `cargo test` is no longer that Docker-free command: nothing in this repository is `#[ignore]`d any more, so an unscoped `cargo test` also runs `nigiri-rs-testcontainers`'s Docker-backed integration tests. They need no feature flag to run because they live in that crate itself.

Bitcoin and Liquid integration tests need Docker but no Nigiri installation. Each one starts its own funded regtest stack through `nigiri-rs-testcontainers`, owns its chain, and removes everything it created when it finishes:

```sh
cargo test -p nigiri-rs-testcontainers --all-targets --all-features
```

Because a fixture owns its chain, those tests need no cross-process mutation lock: a reorg in one is invisible to every other, and they can all run at once. Nothing here is `#[ignore]`d, on purpose: an ignored Docker test reports green having verified nothing, and this project has shipped that exact failure mode twice — once as a CI filter that matched zero tests and exited 0, once as a test that had never run in any CI job. Ignoring a test loses that signal; running it fails loudly instead when Docker is unavailable.

One Bitcoin fixture is ready in about 3 seconds, well inside the 60-second default startup budget. Two started at once take about 4.4 seconds total, so parallelism itself costs roughly a second, not the 103 seconds an earlier note here claimed. That figure was recorded while unrelated runaway processes were saturating every core and said nothing about this crate. A Liquid fixture mines a single block instead of 101: Liquid has no block subsidy, so it funds its wallet by connecting the genesis outputs rather than by mining one. A `PegPair` is four containers rather than two and starts its two halves in sequence, since the Elements node reads `-mainchainrpc*` while starting and needs the `bitcoind` already answering; its default budget is 120 seconds for that reason.

## License

MIT
