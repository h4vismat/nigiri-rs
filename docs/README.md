# nigiri-rs documentation

Typed asynchronous Bitcoin, Liquid, and Lightning clients, with optional throwaway Docker fixtures.

The [repository README](../README.md) is the tour. These pages are the depth behind it.

## Start here

- **[Tutorial: your first fixture-backed test](tutorial-first-test.md)** — from an empty crate to a
  passing test that funds an address on a throwaway chain. No Nigiri installation, no manual setup.
- **[Tutorial: a round trip across Liquid's peg](tutorial-peg-round-trip.md)** — BTC into the
  sidechain and back out again, against a wired four-container pair. Start here if the peg is why
  you came.
- **[Tutorial: settle a Lightning payment](tutorial-lightning-payment.md)** — start two LND nodes
  with a proven bidirectional channel, pay an invoice, and verify both records by payment hash.

## How-to guides

Task-oriented. Each one assumes you have the crate building already.

- [How to run a throwaway regtest stack](how-to-run-a-fixture.md) — start a fixture by hand, tune its
  budget, swap its images.
- [How to point a wallet at a fixture](how-to-connect-a-wallet.md) — get the Esplora URL and Electrum
  host/port a BDK or LWK wallet needs.
- [How to point the client at services you run](how-to-point-at-your-own-services.md) — custom
  endpoints, credentials, timeouts, and response limits.
- [How to use a host-managed LND node](how-to-use-lnd.md) — load a pinned TLS certificate and
  macaroon, wait for readiness, and handle uncertain payment outcomes safely.
- [How to call any node RPC](how-to-call-any-node-rpc.md) — the typed `rpc()` escape hatch for
  methods the curated API does not wrap.
- [How to work with Liquid assets](how-to-work-with-liquid-assets.md) — mint an asset, send it, and
  read confidential UTXOs.
- [How to peg in and peg out](how-to-peg.md) — start a wired Bitcoin and Liquid pair, claim a real
  peg-in, and release a simulated peg-out.

## Reference

Complete, accurate, derived from the source.

- [Client API](reference-client.md) — Bitcoin/Liquid clients plus `LndClient`, `LightningNode`,
  Lightning request/response records, and checked amount types.
- [Fixture API](reference-fixtures.md) — `Fixture`, `PegPair`, `LndPair`, their builders,
  `ContainerImage`, and `FixtureChain`.
- [`#[nigiri_rs::test]`](reference-test-macro.md) — accepted arguments, accepted signatures
  (including `PegPair` and `LndPair`), and every rejection with its message.
- [Errors](reference-errors.md) — `NigiriError`, `LndError`, and `FixtureError`, variant by variant.

## Explanation

Why the design is shaped this way.

- [Lifecycle ownership](explanation-lifecycle-ownership.md) — why the client crate never starts,
  stops, or deletes anything, and why fixtures live in a separate crate.
- [Typed networks](explanation-typed-networks.md) — why `Bitcoin` and `Liquid` are type parameters
  rather than an enum, and what that buys at compile time.
- [What "ready" means](explanation-fixture-readiness.md) — why a fixture waits for three services to
  agree on a tip, and why `LndPair` also proves a channel in both directions.
- [What the peg simulates](explanation-what-the-peg-simulates.md) — which half of Liquid's peg is
  real on regtest, which half this crate plays, and what that means for what you can assert.

## The workspace

| Crate | Version | What it is |
| --- | --- | --- |
| `nigiri-rs` | 0.5.0 | The facade. Re-exports core, optional LND, and optional fixtures/macro. |
| `nigiri-rs-core` | 0.4.0 | Typed Bitcoin and Liquid clients only. No Docker or Lightning ownership. |
| [`nigiri-rs-lnd`](../crates/nigiri-rs-lnd/README.md) | 0.1.0 | Host-managed LND client and protocol-level Lightning types. No Docker. |
| `nigiri-rs-fixtures` | 0.2.0 | Ephemeral Bitcoin, Liquid, peg, and Lightning fixtures. |
| `nigiri-rs-macros` | 0.2.0 | `#[nigiri_rs::test]`. |

Rust edition 2024, MSRV 1.88.

## Installing

Depend on the facade. It re-exports the public surfaces of the other four crates, so you name one
dependency:

```toml
[dev-dependencies]
nigiri-rs = { version = "0.5", features = ["fixtures"] }
```

`dev-dependencies` is usually the right section: fixtures are a testing tool, and it keeps the Docker
client libraries out of your release build. Use `[dependencies]` only if you talk to a regtest
environment from the application itself, and then you probably want the feature off.

Note the version line: **`nigiri-rs` is 0.5.0, `nigiri-rs-core` is 0.4.0.** Separate crates, separate
version numbers — do not read the facade's number as the core crate's, which you do not name above.
The facade went 0.2.0 → 0.4.0 → 0.5.0 and never had a 0.3.0; that number belongs to the core crate.
Only 0.2.0 of the facade was ever published to crates.io; 0.4.0 and 0.5.0 are not yet installable
from there.

Every snippet in these pages assumes that dependency.

Working against unreleased changes instead? Point at git and pin a commit for reproducibility:

```toml
nigiri-rs = { git = "https://github.com/h4vismat/nigiri-rs", rev = "0900676", features = ["fixtures"] }
```

## Feature flags

All live on the `nigiri-rs` facade and are off by default.

| Feature | Pulls in | Enables |
| --- | --- | --- |
| `lnd` | `nigiri-rs-lnd` | `LndClient`, `LightningNode`, and all project-owned Lightning types |
| `fixtures` | `lnd`, `nigiri-rs-fixtures`, `nigiri-rs-macros`, `tokio` | `nigiri_rs::fixtures`, `#[nigiri_rs::test]`, including `LndPair` |
| `bitcoin-rpc-types` | `corepc-types` 0.15 | `nigiri_rs::bitcoin_rpc_types`, maintained Bitcoin Core response records — pick the module matching your node (`v31` for a fixture, `v30` for Nigiri) |

`fixtures` implies `lnd` because `LndPair` returns `LndClient` values. Standalone host-managed LND
use needs only `lnd`, so it does not pull Docker lifecycle dependencies.
