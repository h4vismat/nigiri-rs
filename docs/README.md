# nigiri-rs documentation

Typed asynchronous Bitcoin and Liquid regtest clients, with optional throwaway Docker fixtures.

The [repository README](../README.md) is the tour. These pages are the depth behind it.

## Start here

- **[Tutorial: your first fixture-backed test](tutorial-first-test.md)** — from an empty crate to a
  passing test that funds an address on a throwaway chain. No Nigiri installation, no manual setup.

## How-to guides

Task-oriented. Each one assumes you have the crate building already.

- [How to run a throwaway regtest stack](how-to-run-a-fixture.md) — start a fixture by hand, tune its
  budget, swap its images.
- [How to point a wallet at a fixture](how-to-connect-a-wallet.md) — get the Esplora URL and Electrum
  host/port a BDK or LWK wallet needs.
- [How to point the client at services you run](how-to-point-at-your-own-services.md) — custom
  endpoints, credentials, timeouts, and response limits.
- [How to call any node RPC](how-to-call-any-node-rpc.md) — the typed `rpc()` escape hatch for
  methods the curated API does not wrap.
- [How to work with Liquid assets](how-to-work-with-liquid-assets.md) — mint an asset, send it, and
  read confidential UTXOs.

## Reference

Complete, accurate, derived from the source.

- [Client API](reference-client.md) — `NigiriClient`, `NigiriConfig`, `ElectrumEndpoint`, the
  response records, and the network markers.
- [Fixture API](reference-fixtures.md) — `Fixture`, `FixtureBuilder`, `ContainerImage`,
  `FixtureChain`.
- [`#[nigiri_rs::test]`](reference-test-macro.md) — accepted arguments, accepted signatures, and
  every rejection with its message.
- [Errors](reference-errors.md) — `NigiriError` and `FixtureError`, variant by variant, with what
  triggers each.

## Explanation

Why the design is shaped this way.

- [Lifecycle ownership](explanation-lifecycle-ownership.md) — why the client crate never starts,
  stops, or deletes anything, and why fixtures live in a separate crate.
- [Typed networks](explanation-typed-networks.md) — why `Bitcoin` and `Liquid` are type parameters
  rather than an enum, and what that buys at compile time.
- [What "ready" means](explanation-fixture-readiness.md) — why a fixture waits for three services to
  agree on a tip before it hands you a client.

## The workspace

| Crate | Version | What it is |
| --- | --- | --- |
| `nigiri-rs` | 0.4.0 | The facade. Depend on this. Re-exports the client, plus fixtures and the test macro behind the `testcontainers` feature. |
| `nigiri-rs-core` | 0.3.0 | The typed clients. No Docker dependency, no lifecycle management. |
| `nigiri-rs-testcontainers` | 0.1.0 | Ephemeral Docker-backed regtest fixtures. |
| `nigiri-rs-macros` | 0.1.0 | `#[nigiri_rs::test]`. |

Rust edition 2024, MSRV 1.88.

## Installing

Depend on the facade. It re-exports everything the other three crates provide, so you name one
dependency:

```toml
[dev-dependencies]
nigiri-rs = { version = "0.4", features = ["testcontainers"] }
```

`dev-dependencies` is usually the right section: fixtures are a testing tool, and it keeps the Docker
client libraries out of your release build. Use `[dependencies]` only if you talk to a regtest
environment from the application itself, and then you probably want the feature off.

Note the version line: **`nigiri-rs` is 0.4, `nigiri-rs-core` is 0.3.** Separate crates, separate
version numbers. The facade went 0.2.0 → 0.4.0 and never had a 0.3.0; that number belongs to the
core crate, which you do not name.

Every snippet in these pages assumes that dependency.

Working against unreleased changes instead? Point at git and pin a commit for reproducibility:

```toml
nigiri-rs = { git = "https://github.com/h4vismat/nigiri-rs", rev = "8579e78", features = ["testcontainers"] }
```

## Feature flags

Both live on the `nigiri-rs` facade and are off by default.

| Feature | Pulls in | Enables |
| --- | --- | --- |
| `testcontainers` | `nigiri-rs-testcontainers`, `nigiri-rs-macros`, `tokio` | `nigiri_rs::testcontainers`, `#[nigiri_rs::test]` |
| `bitcoin-rpc-types` | `corepc-types` 0.15 | `nigiri_rs::bitcoin_rpc_types`, maintained Bitcoin Core v30 response records |

`testcontainers` is off by default because it pulls Docker client dependencies that a consumer
talking to services it already runs does not need.
