# LND Client and Ready-to-Pay Fixture Design

**Date:** 2026-08-18
**Status:** Approved design

## Objective

Add first-class LND support for both externally managed nodes and ephemeral integration tests.
The host-facing API must cover common Lightning payment operations without exposing generated LND
protobuf types. The fixture must return two authenticated LND clients connected by a confirmed
channel with spendable liquidity on both sides, and it must prove that a real Lightning payment can
settle before startup succeeds.

## Scope

This increment includes:

- a focused LND client for services managed outside this library;
- project-owned Lightning domain records and a generic `LightningNode` interface;
- two ephemeral LND nodes backed by the existing Bitcoin fixture;
- a pre-funded, active channel that can carry payments in either direction;
- facade features and re-exports;
- `#[nigiri_rs::test]` injection of an `LndPair`;
- unit, fake-runtime, compile-time, Docker integration, and documentation coverage.

The following are deliberately outside this increment:

- Core Lightning or any other Lightning implementation;
- Taproot Assets;
- a complete or publicly exposed generated LND RPC surface;
- BOLT 12 offers;
- multi-hop routing topologies;
- persistent fixture data, node restoration, channel backup, force-close, or recovery testing;
- production node lifecycle management.

## Architectural decisions

### A separate protocol crate

Add `crates/nigiri-rs-lnd` as a fifth workspace crate. It does not depend on
`nigiri-rs-core`: Bitcoin/Liquid node operations and Lightning operations have different protocols,
authentication, lifecycle, and domain models. Combining them would turn `nigiri-rs-core` into a
service container rather than a cohesive protocol client.

The dependency direction is:

```text
nigiri-rs-core      Bitcoin/Liquid protocol clients
nigiri-rs-lnd       Lightning domain API + private LND adapter
       \             /
        nigiri-rs-fixtures   Docker-compatible lifecycle
                 \          /
                  nigiri-rs  facade
```

`nigiri-rs-fixtures` depends on both protocol crates. Neither protocol crate depends on fixture or
container code.

The facade adds an optional `lnd` feature that enables and re-exports `nigiri-rs-lnd`. Its existing
`fixtures` feature implies `lnd`, because `LndPair` returns `LndClient` values. Users who only talk
to Bitcoin or Liquid retain the current dependency footprint.

### Private bindings generated from pinned official protobufs

The client uses Tonic and Prost against an in-repository copy of the minimum official LND protobuf
graph needed for Lightning, WalletUnlocker, and Router operations. The initial protocol baseline is
the signed LND `v0.21.1-beta` release at commit `2b87887`. The implementation records the upstream
tag and commit beside the protobuf sources and records their license and provenance.

The LND fixture image uses the same `v0.21.1-beta` release and is pinned by both tag and registry
digest, following the existing `ContainerImage` policy. A unit test compares the image version and
protobuf baseline constants so one cannot be upgraded without the other.

Generated modules are private. No public signature contains a Tonic, Prost, or LND-generated type.
This isolates protobuf churn and keeps application code independent of a particular Rust gRPC
binding crate. It also avoids depending on `tonic_lnd`, whose release and supported-protocol cadence
would otherwise become part of this project's compatibility boundary.

### Functional boundary

`LndClient` is an immutable, cheaply cloneable handle. Request construction and protobuf/domain
conversion are pure functions. Network calls do not mutate client configuration.

A statically dispatched `LightningNode` trait defines the focused behavior and has an associated
error type. Trait methods return `impl Future + Send`; the design does not require object safety,
boxed futures, or the `async-trait` macro. `LndClient` also exposes the same operations as inherent
async methods for direct use.

The trait is the application-level portability boundary. A future Core Lightning adapter may
implement it without changing generic consumers, but no Core Lightning implementation is part of
this work.

## Host-managed LND client

### Configuration and construction

`LndConfig` contains:

- an HTTPS gRPC endpoint;
- PEM-encoded server certificate bytes;
- macaroon bytes;
- one nonzero request timeout.

The endpoint must have an `https` scheme, a host, and a port, and must not contain credentials,
query parameters, or a fragment. Certificate and macaroon inputs must be nonempty. The same bounded
maximum sizes used for diagnostic safety apply during configuration parsing.

The primary constructor accepts credential bytes. File-loading convenience constructors read a
certificate and macaroon before delegating to the byte-based constructor; no operational method
depends on LND's default filesystem layout.

Certificate input is capped at 1 MiB and macaroon input at 64 KiB. These bounds are public
configuration constants and apply equally to byte-based and file-based construction.

`LndConfig`, credentials, interceptors, and `LndClient` use manual `Debug` implementations that
never print macaroons, certificate contents, or derived metadata values. Configuration is validated
synchronously. Connecting occurs lazily on the first operation, and `wait_ready` provides an
explicit readiness check.

### Public operations

The initial `LightningNode` and `LndClient` surface is:

- `get_info`;
- `new_address`;
- `wallet_balance`;
- `connect_peer`;
- `list_peers`;
- `open_channel`;
- `list_channels`;
- `create_invoice`;
- `lookup_invoice`;
- `pay_invoice`;
- `lookup_payment`.

Public records use protocol-level types where practical:

- `bitcoin::secp256k1::PublicKey` for node identities;
- `bitcoin::Address<NetworkUnchecked>` for addresses whose network must be checked by the caller;
- `bitcoin::OutPoint` for channel points;
- parsed `lightning_invoice::Bolt11Invoice` values instead of unchecked payment-request strings;
- `Sats` and `Millisats` checked newtypes instead of ambiguous integer amounts;
- exhaustive, crate-owned `InvoiceState` and `PaymentState` enums with an `Unknown(i32)` case for
  forward-compatible daemon values.

Amount constructors reject overflow, and conversions between sats and millisats are checked.
Response conversion rejects invalid public keys, addresses, hashes, channel points, invoices, and
internally inconsistent amount or state combinations.

### Payment semantics

`pay_invoice` consumes the Router service's server stream internally. It returns only after a
terminal update:

- `Succeeded` returns a settled payment record containing the payment hash, preimage, value, and
  fee;
- `Failed` returns a rejected-payment error containing the payment hash and a bounded failure
  reason;
- stream termination without a terminal state is an invalid response;
- a timeout reports the payment hash when known and explicitly marks the final state unknown.

A timeout does not mean LND cancelled or failed the payment. Callers must query payment state before
retrying. The same committed-but-uncertain principle applies to channel opening and invoice
creation: errors preserve a channel point or payment hash whenever the daemon supplied one.

## Ready-to-pay fixture

### Public API

`nigiri-rs-fixtures` adds:

- `LndPair`;
- `LndPairBuilder`;
- `ContainerImage::lnd_default()` internally;
- `LndPair` support in `#[nigiri_rs::test]`.

`LndPair` exposes `bitcoin()`, `alice()`, `bob()`, and `channel_point()`. `alice()` and `bob()` return
authenticated `LndClient` references. `bitcoin()` returns the existing `NigiriClient<Bitcoin>`, so
tests can mine blocks or inspect the backing chain without a second fixture.

`LndPairBuilder` configures:

- one startup deadline, defaulting to 180 seconds;
- Bitcoin and Electrs images;
- Alice and Bob LND images;
- channel capacity, defaulting to 2,000,000 sats;
- the amount pushed to Bob at open, defaulting to 1,000,000 sats.

Validation occurs before any container starts. The pushed amount must be nonzero and lower than the
capacity, and each nominal side must retain at least 100,000 sats. After activation, readiness
separately requires each daemon's reported spendable local balance to exceed the 1,000-millisat
probe amount.

### Topology

The pair owns four containers on one UUID-scoped network:

```text
                       bitcoind + Electrs
                          /          \
                  RPC + ZMQ          RPC + ZMQ
                       /              \
                  LND Alice ======== LND Bob
                       confirmed direct channel
```

The Bitcoin node keeps its current RPC and indexer configuration and additionally publishes raw
blocks on port 28332 and raw transactions on port 28333. These ZMQ endpoints are reachable only on
the private container network; they do not need host mappings.

Each LND node exposes its gRPC port and peer port through runtime-assigned loopback mappings. It
connects to bitcoind by the fixture-scoped container name and the existing fixture RPC credentials.
No fixed host port, global container name, bind mount, or persistent volume is used.

### Startup sequence

One `Deadline` covers every phase below:

1. Validate all images, amounts, and the timeout.
2. Start the Bitcoin fixture with ZMQ arguments and wait for bitcoind, Esplora, and Electrum to
   agree on the tip.
3. Start Alice and Bob concurrently on the Bitcoin fixture's network.
4. Wait for each LND TLS certificate to exist, then read it through a generic runtime
   `read_container_file` operation.
5. Connect to each unauthenticated WalletUnlocker service, generate a cipher seed, and initialize
   an ephemeral wallet with an independently generated in-memory password.
6. Construct authenticated `LndClient` values from the admin macaroons returned by wallet
   initialization. Seed material and passwords are discarded after initialization.
7. Wait until both nodes report Bitcoin `regtest` and full chain and graph synchronization.
8. Ask Alice for an on-chain address, fund it from the fixture Bitcoin wallet, mine confirmation,
   and wait for Alice's wallet to report the spendable output.
9. Connect Alice to Bob by Bob's private container name, port 9735, and public key. Host-mapped
   ports are never used for container-to-container traffic.
10. Open a 2,000,000-sat channel from Alice with 1,000,000 sats pushed to Bob, mine the required
    regtest confirmations, and wait until both clients report the same active channel point with
    spendable local balance.
11. Bob creates a 1,000-millisat invoice carrying the memo `nigiri-rs readiness probe`; Alice pays
    it; startup waits for both a successful payment and the matching settled invoice.

The readiness probe intentionally leaves one invoice and one payment in fixture history. This is
part of the documented startup contract; tests must identify their own records by payment hash
rather than assume empty histories.

### Runtime changes and ownership

The runtime-neutral `ContainerEngine` gains a byte-returning `read_container_file(container,
path)` operation. The Bollard adapter implements it with Docker's archive API and bounded tar-entry
decoding. Fake engines implement the same contract without Docker. The operation rejects missing
files, directories, links, multiple matching entries, and content over 1 MiB.

LND containers are supervised together and reuse the Bitcoin fixture's network without claiming
ownership of it. `LndPair` holds the LND runtime before the Bitcoin fixture so teardown always
removes Bob and Alice before Electrs and bitcoind. `shutdown()` explicitly awaits both stages and
returns the first cleanup error after attempting all cleanup. `Drop` retains best-effort cleanup.

The test macro treats `LndPair` like `PegPair`: it is the owning handle and moves into the test
body. Multiple requested fixtures continue to start concurrently.

## Errors and diagnostics

`LndError` is separate from `NigiriError` and is `#[non_exhaustive]`. It distinguishes:

- invalid configuration or caller input;
- TLS and gRPC transport failure;
- authentication rejection;
- operation timeout;
- daemon status rejection;
- malformed or inconsistent responses;
- failed payments;
- uncertain committed operations.

Errors name a bounded operation label and preserve a source where one exists. They never include
raw request metadata or credential bytes.

`FixtureError` gains a Lightning client/source variant and uses existing `Probe` and
`ReadinessTimeout` shapes for startup observations. Every failure names `lnd-alice`, `lnd-bob`, or
`lightning-channel` as appropriate. Failure enrichment attaches bounded logs for both LND nodes and
the inner Bitcoin stack in dependency order.

Diagnostic redaction covers fixture RPC credentials, wallet passwords, seed material, raw and
hexadecimal macaroons, and TLS private-key material. Certificate public data need not be treated as
a secret, but full PEM bodies are never logged.

Cancellation at any startup await triggers the existing supervisor's reverse-order cleanup. A
failure to start the second LND node, initialize either wallet, fund the channel, confirm it, or
settle the readiness invoice cannot leak a container or network.

## Verification strategy

### `nigiri-rs-lnd`

- configuration validation and normalized endpoint tests;
- secret-redacting `Debug`, display, and source-chain tests;
- checked satoshi/millisatoshi conversion tests;
- pure request and response conversion tests for every public operation;
- unknown enum-value preservation tests;
- payment stream state-machine tests covering success, daemon failure, premature end, timeout,
  duplicate terminal updates, and malformed records;
- in-process Tonic test services for TLS, macaroon injection, status mapping, and deadline behavior;
- compile checks ensuring public API examples require no generated protobuf imports.

### `nigiri-rs-fixtures`

- pure LND command/specification tests, including bitcoind RPC and ZMQ wiring;
- fake-engine tests for certificate reads, file bounds, cancellation, log attachment, and cleanup
  ordering;
- validation tests proving an invalid later image or channel allocation starts no resources;
- Docker integration tests proving both nodes report the same regtest chain and active channel;
- Alice-to-Bob and Bob-to-Alice invoice payments with payment-hash, settled-invoice, and balance
  assertions;
- explicit-shutdown and `Drop` teardown tests proving all four containers and the private network
  disappear;
- parallel-pair tests proving names and ports do not collide.

### Facade, macro, and documentation

- facade feature and re-export tests;
- macro parser UI tests for accepted `LndPair` and near-miss parameter types;
- macro smoke tests that settle a payment through an injected pair;
- README crate/feature/topology updates;
- host-managed LND configuration how-to;
- ready-to-pay fixture tutorial and fixture/API/error reference updates;
- lifecycle explanation updated to include the separate LND protocol crate and fixture ownership.

Docker-free CI continues to test the protocol and macro crates. Docker-backed CI runs the fixture
suite. Protobuf generation must be reproducible without an LND checkout or network access by using
the checked-in protocol sources and a pinned vendored `protoc` build dependency.

## Compatibility and release impact

Existing Bitcoin, Liquid, and peg APIs do not change. `lnd` is opt-in unless `fixtures` is enabled.
The new crate adds a publish-order dependency: publish `nigiri-rs-core` and `nigiri-rs-lnd` before
`nigiri-rs-fixtures`, then publish the facade. The feature and crate descriptions must stop
describing the project as Bitcoin-and-Liquid-only.

The implementation is complete when a consumer can:

1. construct an `LndClient` for a host-managed LND node from endpoint, certificate, and macaroon;
2. start `LndPair`, create an invoice on either node, pay it from the other, and observe terminal
   success and settlement;
3. drop or explicitly shut down the pair without leaving any owned container or network behind.

## Sources

- [LND v0.21.1-beta release](https://github.com/lightningnetwork/lnd/releases/tag/v0.21.1-beta)
- [Official LND protobuf definitions](https://github.com/lightningnetwork/lnd/tree/v0.21.1-beta/lnrpc)
- [LND installation and authentication](https://github.com/lightningnetwork/lnd/blob/v0.21.1-beta/docs/INSTALL.md)
- [Tonic gRPC implementation](https://github.com/grpc/grpc-rust)
- [Nigiri Lightning topology](https://github.com/vulpemventures/nigiri)
