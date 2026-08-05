# How to point the client at services you run

Configure `NigiriClient` for a host-owned Nigiri installation, a remote regtest host, or any
compatible Bitcoin/Elements node plus Esplora indexer.

Nothing here starts, stops, or deletes anything. See
[Lifecycle ownership](explanation-lifecycle-ownership.md) for why.

## Prerequisites

- Services already running. For a host-owned Nigiri:

  ```sh
  nigiri start            # Bitcoin only
  nigiri start --liquid   # Bitcoin and Liquid
  ```

  The verified CLI and port contract is Nigiri v0.5.16, commit
  `39fd5891d093bfb8c2575b79640b95a830834f9c`.

- The `nigiri-rs` dependency. The `testcontainers` feature is **not** needed for this:

  ```toml
  [dependencies]
  nigiri-rs = "0.5"
  url = "2"
  ```

## Use the defaults

If your services are on Nigiri's published ports, there is nothing to configure:

```rust
use nigiri_rs::{Bitcoin, Liquid, NigiriClient};

let bitcoin = NigiriClient::<Bitcoin>::new();
let liquid = NigiriClient::<Liquid>::new();
```

| Network | Node JSON-RPC | Esplora | Electrum |
| --- | --- | --- | --- |
| Bitcoin | `http://localhost:18443/` | `http://localhost:30000/` | `localhost:50000` |
| Liquid | `http://localhost:18884/` | `http://localhost:30001/` | `localhost:50001` |

Credentials default to Nigiri's public regtest pair, user `admin1` and password `123`. They appear in
`NigiriConfig`'s `Debug` output on purpose: they are published defaults, not secrets.

### Verification

```rust,ignore
bitcoin.wait_ready().await?;
println!("tip: {}", bitcoin.block_height().await?);
```

`wait_ready` polls Esplora every 100 ms and gives up after the configured timeout (30 s by default)
with `NigiriError::Timeout`.

## Custom endpoints

```rust
use std::time::Duration;
use nigiri_rs::{Bitcoin, NigiriClient, NigiriConfig};
use url::Url;

# fn example() -> Result<(), Box<dyn std::error::Error>> {
let config = NigiriConfig {
    esplora_url: Url::parse("http://regtest-host:30000")?,
    node_rpc_url: Url::parse("http://regtest-host:18443")?,
    node_rpc_user: "admin1".to_owned(),
    node_rpc_password: "123".to_owned(),
    timeout: Duration::from_secs(20),
    ..Default::default()
};

let client = NigiriClient::<Bitcoin>::with_config(config)?;
# let _ = client;
# Ok(())
# }
```

`with_config` validates and normalizes before returning. It rejects, as
`NigiriError::InvalidRequest`:

- a scheme other than `http` or `https`;
- a URL carrying a query string or a fragment;
- a zero `timeout`;
- a zero or over-limit `max_response_bytes`.

`esplora_url` is a base URL and gets a trailing slash added. `node_rpc_url` does **not** — its path is
preserved exactly, so you can target a wallet-scoped Bitcoin Core endpoint:

```rust,ignore
node_rpc_url: Url::parse("http://regtest-host:18443/wallet/mywallet")?,
```

## For Liquid, start from `liquid()`, not `Default`

```rust
use nigiri_rs::{Liquid, NigiriClient, NigiriConfig};

# fn example() -> Result<(), Box<dyn std::error::Error>> {
let config = NigiriConfig {
    node_rpc_password: "something-else".to_owned(),
    ..NigiriConfig::liquid()
};

let client = NigiriClient::<Liquid>::with_config(config)?;
# let _ = client;
# Ok(())
# }
```

**Do not reach for `..Default::default()` and override the two service URLs by hand.**
`NigiriConfig::default()` returns the *Bitcoin* configuration for every network, and Rust evaluates
`default()` before applying any struct-update override. Three fields are chain-dependent —
`esplora_url`, `node_rpc_url`, and `electrum` — so naming only the first two leaves a Liquid client
pointed at Bitcoin's Electrum port 50000:

```rust,ignore
// Broken. `electrum` silently keeps port 50000.
let config = NigiriConfig {
    esplora_url: "http://localhost:30001".parse()?,
    node_rpc_url: "http://localhost:18884/".parse()?,
    ..Default::default()
};
```

`NigiriConfig::bitcoin()` and `NigiriConfig::liquid()` are public constructors for exactly this
reason. The failure is pinned by a test in `crates/nigiri-rs-core/src/config.rs` named
`naive_liquid_override_of_only_the_two_urls_keeps_bitcoins_electrum_port`.

This is structural, not a bug to fix: `Default` cannot be made chain-aware, because struct-update
syntax calls it before any type information about your intent is available. Before adding a
chain-dependent field to `NigiriConfig`, check every `..Default::default()` site.

## Set the Electrum endpoint

```rust
use nigiri_rs::{Bitcoin, ElectrumEndpoint, NigiriClient, NigiriConfig};

# fn example() -> Result<(), Box<dyn std::error::Error>> {
let config = NigiriConfig {
    electrum: ElectrumEndpoint::new("regtest-host", 50_000)?,
    ..NigiriConfig::bitcoin()
};
# let _ = NigiriClient::<Bitcoin>::with_config(config)?;
# Ok(())
# }
```

`ElectrumEndpoint::new` rejects an empty host and a zero port.

## Raise the response limit

One limit covers every response body, node JSON-RPC and Esplora alike. It defaults to 64 KiB
(`DEFAULT_MAX_RESPONSE_BYTES`) and is capped at 16 MiB (`MAX_RESPONSE_BYTES_LIMIT`). A body past the
limit is **rejected**, not truncated, with `NigiriError::InvalidResponse`.

```rust
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

Raise it deliberately for methods with large results: `listunspent`, `listtransactions`,
`getblock <hash> 2`. The ceiling exists because an unbounded value read from a config file or
environment variable could turn one response into an out-of-memory abort.

## Timeouts

`NigiriConfig::timeout` bounds each individual HTTP operation against an already-running service. It
does **not** bound a polling loop's total: `wait_for_confirmation` takes its own timeout argument.

**A timeout is not a rollback.** It says the client did not get a response in time; a mutating request
may still have committed. Inspect node state before retrying.

## Security notes

HTTP Basic authentication does not encrypt credentials. The built-in URLs are loopback-only regtest
defaults. If you point this at a remote host, use HTTPS or an isolated trusted network.

Arbitrary RPC methods can mutate node wallets and chain state. Against a **shared** node — a
host-owned Nigiri, a CI host — tests that mutate must coordinate access and restore valid state.
Against a fixture they need not: each one owns its chain.

## Troubleshooting

**`invalid request: endpoint URLs must use HTTP or HTTPS and support relative paths`** — check the
scheme. This fires before any network access.

**`invalid request: endpoint URLs must not contain a query or fragment`** — strip the `?...` or
`#...`.

**`wait for readiness timed out after 30s`** — the Esplora endpoint never answered. Check the URL and
that the indexer is up; `wait_ready` swallows individual failures and retries, so this is the only
signal you get.

**`HTTP status 401 during <method>`** — wrong `node_rpc_user` or `node_rpc_password`.

**Liquid client behaving like a Bitcoin one** — the `..Default::default()` trap above. Print
`client.electrum_endpoint().port()`; 50000 on a Liquid client means you hit it.

## Related

- [Client API reference](reference-client.md) — every field, default, and validation rule
- [How to call any node RPC](how-to-call-any-node-rpc.md)
- [Lifecycle ownership](explanation-lifecycle-ownership.md)
- [Errors](reference-errors.md)
