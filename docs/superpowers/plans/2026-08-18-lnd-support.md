# LND Client and Ready-to-Pay Fixtures Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a focused host-managed LND client and a four-container `LndPair` fixture that returns two authenticated nodes with a proven bidirectional payment channel.

**Architecture:** A new `nigiri-rs-lnd` crate owns vendor-neutral Lightning records and a private Tonic adapter generated from pinned official LND protobufs. `nigiri-rs-fixtures` composes its existing Bitcoin fixture with two LND containers through the runtime-neutral engine, while the facade and test macro expose the result without coupling either protocol crate to container lifecycle.

**Tech Stack:** Rust 2024/MSRV 1.88, Tonic 0.14.6, Prost 0.14, rustls through Tonic, `lightning-invoice` 0.34.1, Tokio 1.50, Bollard 0.20.2, Testcontainers-independent Docker runtime, official LND v0.21.1-beta protobufs and image.

**Spec:** `docs/superpowers/specs/2026-08-18-lnd-support-design.md`

## Global Constraints

- Preserve Rust 1.88 as the workspace MSRV and edition 2024.
- Keep `nigiri-rs-core` independent of LND, Tonic, fixtures, and Bollard.
- Keep all generated LND/Tonic/Prost types private; public signatures use crate-owned or Bitcoin/Lightning protocol types.
- Pin protobufs and `lightninglabs/lnd:v0.21.1-beta@sha256:4af8f9bbf98c8b86b0e54b065d6ea45d1387256a43fa9270c11ef849511abae0` to LND commit `2b87887`.
- Generate protobuf bindings without a network connection or external LND checkout by checking in the three source protos and using `protoc-bin-vendored` 3.2.0.
- Limit TLS certificate input and container file reads to 1 MiB; limit macaroon input to 64 KiB.
- Never expose macaroons, wallet passwords, seeds, RPC passwords, or TLS private keys through `Debug`, `Display`, sources, or fixture logs.
- Use one 180-second default deadline for the complete `LndPair` startup.
- Default channel capacity is 2,000,000 sats, default push amount is 1,000,000 sats, and each nominal side must retain at least 100,000 sats.
- `LndPair::start` succeeds only after a 1,000-millisat invoice has settled over the active channel.
- Preserve every existing Bitcoin, Liquid, peg, runtime, facade, and macro behavior.

---

### Task 1: Scaffold the LND crate and reproducible protocol bindings

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/nigiri-rs-lnd/Cargo.toml`
- Create: `crates/nigiri-rs-lnd/build.rs`
- Create: `crates/nigiri-rs-lnd/proto/PROVENANCE.md`
- Create: `crates/nigiri-rs-lnd/proto/lightning.proto`
- Create: `crates/nigiri-rs-lnd/proto/walletunlocker.proto`
- Create: `crates/nigiri-rs-lnd/proto/routerrpc/router.proto`
- Create: `crates/nigiri-rs-lnd/src/proto.rs`
- Create: `crates/nigiri-rs-lnd/src/lib.rs`
- Create: `crates/nigiri-rs-lnd/tests/protocol_baseline.rs`

**Interfaces:**
- Consumes: official source files from `https://raw.githubusercontent.com/lightningnetwork/lnd/v0.21.1-beta/lnrpc/`.
- Produces: private `proto::lnrpc` and `proto::routerrpc` modules; public constants `LND_PROTO_VERSION: &str = "v0.21.1-beta"` and `LND_PROTO_COMMIT: &str = "2b87887"`.

- [ ] **Step 1: Write the protocol-baseline test before adding the crate implementation**

```rust
use nigiri_rs_lnd::{LND_PROTO_COMMIT, LND_PROTO_VERSION};

#[test]
fn generated_protocol_baseline_is_the_pinned_lnd_release() {
    assert_eq!(LND_PROTO_VERSION, "v0.21.1-beta");
    assert_eq!(LND_PROTO_COMMIT, "2b87887");
}
```

- [ ] **Step 2: Add the workspace member and minimal manifest, then verify the missing constants fail**

Add `"crates/nigiri-rs-lnd"` to `[workspace].members`. Create the package with `rust-version = "1.88"`, the repository's MIT metadata, and these dependency floors:

```toml
[dependencies]
bitcoin = "0.32.7"
lightning-invoice = "0.34.1"
prost = "0.14.1"
thiserror = "2.0.18"
tokio = { version = "1.50.0", features = ["fs", "time"] }
tonic = { version = "0.14.6", features = ["tls-ring"] }
tonic-prost = "0.14.6"
url = "2.5.8"

[build-dependencies]
protoc-bin-vendored = "3.2.0"
tonic-prost-build = "0.14.6"
```

Run: `cargo test -p nigiri-rs-lnd --test protocol_baseline`

Expected: FAIL because `LND_PROTO_VERSION` and `LND_PROTO_COMMIT` do not exist.

- [ ] **Step 3: Vendor the exact protocol sources and provenance**

Copy only these files from tag `v0.21.1-beta` without editing their contents:

```text
lnrpc/lightning.proto             -> proto/lightning.proto
lnrpc/walletunlocker.proto        -> proto/walletunlocker.proto
lnrpc/routerrpc/router.proto      -> proto/routerrpc/router.proto
```

`PROVENANCE.md` must name tag `v0.21.1-beta`, commit `2b87887`, each original URL, retrieval date `2026-08-18`, and LND's MIT license. Confirm `walletunlocker.proto` and `router.proto` import only `lightning.proto`.

- [ ] **Step 4: Generate private clients and test servers from the checked-in files**

```rust
// build.rs
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    unsafe { std::env::set_var("PROTOC", protoc); }
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(false)
        .compile_protos(
            &["proto/lightning.proto", "proto/walletunlocker.proto", "proto/routerrpc/router.proto"],
            &["proto"],
        )?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
```

```rust
// src/proto.rs
pub(crate) mod lnrpc { tonic::include_proto!("lnrpc"); }
pub(crate) mod routerrpc { tonic::include_proto!("routerrpc"); }
```

Expose only the two baseline constants from `lib.rs`; keep `mod proto` private.

- [ ] **Step 5: Run the narrow tests and offline build**

Run: `cargo test -p nigiri-rs-lnd --test protocol_baseline && cargo build -p nigiri-rs-lnd --offline`

Expected: PASS, proving code generation needs no network or LND checkout.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/nigiri-rs-lnd
git commit -m "feat(lnd): add pinned private protocol bindings"
```

### Task 2: Add Lightning domain values, configuration, and safe errors

**Files:**
- Create: `crates/nigiri-rs-lnd/src/amount.rs`
- Create: `crates/nigiri-rs-lnd/src/config.rs`
- Create: `crates/nigiri-rs-lnd/src/error.rs`
- Create: `crates/nigiri-rs-lnd/src/types.rs`
- Modify: `crates/nigiri-rs-lnd/src/lib.rs`
- Create: `crates/nigiri-rs-lnd/tests/foundation.rs`

**Interfaces:**
- Consumes: `bitcoin::{Address, OutPoint, hashes::sha256, secp256k1::PublicKey}` and `lightning_invoice::Bolt11Invoice`.
- Produces: `Sats`, `Millisats`, `LndConfig`, `LndError`, `NodeInfo`, `WalletBalance`, `PeerAddress`, `Peer`, `OpenChannelRequest`, `Channel`, `CreateInvoiceRequest`, `InvoiceRecord`, `InvoiceState`, `PaymentOptions`, `PaymentRecord`, and `PaymentState`.

- [ ] **Step 1: Write failing amount, configuration, and redaction tests**

```rust
#[test]
fn amount_conversion_is_checked() {
    assert_eq!(Millisats::try_from(Sats::new(21)).unwrap().as_u64(), 21_000);
    assert!(Millisats::try_from(Sats::new(u64::MAX)).is_err());
}

#[test]
fn configuration_rejects_unsafe_endpoints_and_hides_credentials() {
    let cert = b"certificate-marker".to_vec();
    let macaroon = b"macaroon-marker".to_vec();
    assert!(LndConfig::new("http://localhost:10009", cert.clone(), macaroon.clone(), Duration::from_secs(1)).is_err());
    let config = LndConfig::new("https://localhost:10009", cert, macaroon, Duration::from_secs(1)).unwrap();
    let debug = format!("{config:?}");
    assert!(!debug.contains("certificate-marker"));
    assert!(!debug.contains("macaroon-marker"));
}
```

Run: `cargo test -p nigiri-rs-lnd --test foundation`

Expected: FAIL because the public domain and configuration types do not exist.

- [ ] **Step 2: Implement checked unit types**

```rust
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Sats(u64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Millisats(u64);

impl Sats {
    pub const fn new(value: u64) -> Self { Self(value) }
    pub const fn as_u64(self) -> u64 { self.0 }
}

impl TryFrom<Sats> for Millisats {
    type Error = LndError;
    fn try_from(value: Sats) -> Result<Self, Self::Error> {
        value.0.checked_mul(1_000).map(Self).ok_or(LndError::InvalidRequest {
            detail: "satoshi amount overflows millisatoshis".into(),
        })
    }
}
```

Add equivalent `Millisats::new`, `as_u64`, and checked whole-satoshi conversion.

- [ ] **Step 3: Implement configuration validation and file loading**

Use private `Arc<[u8]>` credential fields, `MAX_TLS_CERTIFICATE_BYTES = 1_048_576`, and `MAX_MACAROON_BYTES = 65_536`. Reject zero timeout; non-HTTPS URLs; missing host/port; userinfo, query, or fragment; and empty/oversized credentials. Implement:

```rust
pub fn new(endpoint: impl AsRef<str>, certificate: Vec<u8>, macaroon: Vec<u8>, timeout: Duration) -> Result<Self, LndError>;
pub async fn from_files(endpoint: impl AsRef<str>, certificate_path: impl AsRef<Path>, macaroon_path: impl AsRef<Path>, timeout: Duration) -> Result<Self, LndError>;
pub fn endpoint(&self) -> &Url;
pub fn timeout(&self) -> Duration;
```

Manual `Debug` may print endpoint and timeout only.

- [ ] **Step 4: Implement non-exhaustive errors and domain records**

`LndError` variants must include `InvalidRequest`, `CredentialRead`, `Transport`, `Authentication`, `Status`, `Timeout`, `InvalidResponse { operation, detail, identifier: Option<String> }`, `PaymentFailed { payment_hash, reason }`, and `OutcomeUnknown { operation, identifier: Option<String> }`. Use bounded `Cow<'static, str>` operation/detail fields and retain sources only after verifying they do not include metadata.

Define records with private fields plus accessors. Use `Unknown(i32)` in state enums. `PeerAddress::new` rejects blank hosts and zero ports. `OpenChannelRequest::new` rejects zero capacity and push not lower than capacity. `CreateInvoiceRequest::new` rejects zero amount or expiry. `PaymentOptions::new` rejects zero timeout.

- [ ] **Step 5: Run tests and documentation checks**

Run: `cargo test -p nigiri-rs-lnd --all-targets && cargo test -p nigiri-rs-lnd --doc`

Expected: PASS, including error source and redaction assertions.

- [ ] **Step 6: Commit**

```bash
git add crates/nigiri-rs-lnd
git commit -m "feat(lnd): add safe Lightning domain types"
```

### Task 3: Build authenticated transport and client readiness

**Files:**
- Create: `crates/nigiri-rs-lnd/src/transport.rs`
- Create: `crates/nigiri-rs-lnd/src/client.rs`
- Create: `crates/nigiri-rs-lnd/src/convert.rs`
- Modify: `crates/nigiri-rs-lnd/build.rs`
- Modify: `crates/nigiri-rs-lnd/src/lib.rs`
- Create: `crates/nigiri-rs-lnd/proto/test/harness.proto`

**Interfaces:**
- Consumes: `LndConfig`; private generated `LightningClient`, `WalletUnlockerClient`, and `RouterClient`.
- Produces: `LndClient::with_config`, `LndClient::wait_ready`, crate-private `UnauthenticatedLndClient`, `authenticated_request`, and `map_status`.

- [ ] **Step 1: Write failing TLS, macaroon, timeout, and readiness tests**

Add a project-owned test-only `harness.proto` containing one unary `Probe` RPC, compile its client and server from `build.rs` in a separate `tonic_prost_build` invocation, and run it with a certificate from `rcgen`. Add `rcgen = "0.14.5"` and `tokio-stream = { version = "0.1.17", features = ["net"] }` as dev dependencies. Exercise the same TLS channel, metadata interceptor, timeout wrapper, and status mapper used by LND. Assert:

```rust
let response = probe_with_transport(server.config()).await.unwrap();
assert_eq!(response, "ready");
assert_eq!(server.last_macaroon().await.as_deref(), Some(server.macaroon_hex()));
```

The second build invocation is exact and isolated from official packages:

```rust
tonic_prost_build::configure()
    .build_client(true)
    .build_server(true)
    .compile_protos(&["proto/test/harness.proto"], &["proto/test"])?;
```

Include the generated `harness` module only under `#[cfg(test)]` in `transport.rs`.

Add separate tests for an untrusted certificate, `Unauthenticated`, and a server delayed past the configured timeout.

Run: `cargo test -p nigiri-rs-lnd transport::tests`

Expected: FAIL because `LndClient` and transport helpers do not exist.

- [ ] **Step 2: Implement lazy TLS channels and macaroon metadata**

Build a Tonic `Endpoint` from the validated URL, set `ClientTlsConfig::ca_certificate(Certificate::from_pem(...))` and `domain_name(endpoint.host_str())`, then call `connect_lazy`. Store the channel, lowercase hex macaroon metadata value, and timeout in an `Arc<ClientInner>`. Never store a formatted request containing the macaroon.

```rust
#[derive(Clone)]
pub struct LndClient { inner: Arc<ClientInner> }

impl LndClient {
    pub fn with_config(config: LndConfig) -> Result<Self, LndError>;
    pub async fn wait_ready(&self) -> Result<NodeInfo, LndError> { self.get_info().await }
}
```

- [ ] **Step 3: Centralize bounded request execution and status mapping**

Every RPC must pass through `tokio::time::timeout`. Map `Code::Unauthenticated` and `Code::PermissionDenied` to `LndError::Authentication`; other statuses to `Status`; elapsed timers to `Timeout`. The helper accepts a borrowed static operation name and never formats request metadata.

- [ ] **Step 4: Add the unauthenticated WalletUnlocker constructor**

```rust
pub(crate) struct UnauthenticatedLndClient { channel: Channel, timeout: Duration }
pub(crate) fn unauthenticated(config: &LndConfig) -> Result<UnauthenticatedLndClient, LndError>;
```

This uses the same pinned certificate but does not attach a macaroon. It is crate-private now and will be exposed to fixtures through one narrowly scoped public wallet-initialization function in Task 7.

- [ ] **Step 5: Run the focused and full crate tests**

Run: `cargo test -p nigiri-rs-lnd transport::tests && cargo test -p nigiri-rs-lnd --all-targets`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/nigiri-rs-lnd
git commit -m "feat(lnd): add authenticated gRPC transport"
```

### Task 4: Implement node, wallet, peer, and channel operations

**Files:**
- Create: `crates/nigiri-rs-lnd/src/node.rs`
- Create: `crates/nigiri-rs-lnd/src/channel.rs`
- Modify: `crates/nigiri-rs-lnd/src/client.rs`
- Modify: `crates/nigiri-rs-lnd/src/convert.rs`
- Modify: `crates/nigiri-rs-lnd/src/lib.rs`

**Interfaces:**
- Consumes: Task 2 domain records and Task 3 transport helpers.
- Produces: inherent client methods and `LightningNode` methods for `get_info`, `new_address`, `wallet_balance`, `connect_peer`, `list_peers`, `open_channel`, and `list_channels`.

- [ ] **Step 1: Write failing request-mapping and malformed-response tests**

Test pure request builders and response converters directly. Pin exact expectations:

```rust
assert_eq!(captured_new_address.address_type, proto::lnrpc::AddressType::WitnessPubkeyHash as i32);
assert_eq!(captured_connect.addr.unwrap().pubkey, bob_key.to_string());
assert_eq!(captured_open.local_funding_amount, 2_000_000);
assert_eq!(captured_open.push_sat, 1_000_000);
```

Return invalid public keys, addresses, channel txids, negative amounts, and mismatched channel points from separate cases; each must produce `LndError::InvalidResponse`.

Put these tests in `node.rs` and `channel.rs` so they can exercise private pure builders and converters. Run: `cargo test -p nigiri-rs-lnd node::tests && cargo test -p nigiri-rs-lnd channel::tests`

Expected: FAIL because the methods are absent.

- [ ] **Step 2: Implement pure protobuf conversions**

Add functions with these exact boundaries:

```rust
fn node_info(GetInfoResponse) -> Result<NodeInfo, LndError>;
fn wallet_balance(WalletBalanceResponse) -> Result<WalletBalance, LndError>;
fn peer(Peer) -> Result<crate::Peer, LndError>;
fn channel(Channel) -> Result<crate::Channel, LndError>;
fn channel_point(ChannelPoint) -> Result<bitcoin::OutPoint, LndError>;
```

Use `TryFrom` for signed LND integers and reject negative values before casting.

- [ ] **Step 3: Implement unary node and peer methods**

Each method clones the generated client over the shared channel, injects authenticated metadata, applies the operation timeout, and converts the response. `connect_peer` sets `perm = false` and uses `PeerAddress`'s `public_key@host:port` components without shell or CLI formatting.

- [ ] **Step 4: Implement streaming channel opening safely**

Use `Lightning/OpenChannel`, not the REST-oriented sync method. Consume updates until `chan_open` supplies a funding outpoint. If the stream ends first, return `InvalidResponse`; if timeout/status occurs after an outpoint, return `OutcomeUnknown` carrying that outpoint.

- [ ] **Step 5: Define and implement the portability trait**

```rust
pub trait LightningNode: Clone + Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;
    fn get_info(&self) -> impl Future<Output = Result<NodeInfo, Self::Error>> + Send;
    fn new_address(&self) -> impl Future<Output = Result<Address<NetworkUnchecked>, Self::Error>> + Send;
    fn wallet_balance(&self) -> impl Future<Output = Result<WalletBalance, Self::Error>> + Send;
    fn connect_peer(&self, peer: &PeerAddress) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn list_peers(&self) -> impl Future<Output = Result<Vec<Peer>, Self::Error>> + Send;
    fn open_channel(&self, request: OpenChannelRequest) -> impl Future<Output = Result<OutPoint, Self::Error>> + Send;
    fn list_channels(&self) -> impl Future<Output = Result<Vec<Channel>, Self::Error>> + Send;
}
```

Task 5 extends the same trait with invoice/payment methods.

- [ ] **Step 6: Run tests and commit**

Run: `cargo test -p nigiri-rs-lnd --all-targets --doc`

Expected: PASS.

```bash
git add crates/nigiri-rs-lnd
git commit -m "feat(lnd): add node and channel operations"
```

### Task 5: Implement invoices and terminal payment state handling

**Files:**
- Create: `crates/nigiri-rs-lnd/src/payment.rs`
- Modify: `crates/nigiri-rs-lnd/src/client.rs`
- Modify: `crates/nigiri-rs-lnd/src/convert.rs`
- Modify: `crates/nigiri-rs-lnd/src/lib.rs`

**Interfaces:**
- Consumes: generated Lightning `AddInvoice`/`LookupInvoice` and Router `SendPaymentV2`/`TrackPaymentV2`.
- Produces: `create_invoice`, `lookup_invoice`, `pay_invoice`, and `lookup_payment` on `LndClient` and `LightningNode`.

- [ ] **Step 1: Write failing payment state-machine tests**

Cover these exact streams: `InFlight -> Succeeded`, immediate `Failed`, stream end while `InFlight`, malformed 31-byte hash/preimage, duplicate terminal update, and timeout after hash discovery. Assert success returns value and fee, failure preserves hash/reason, and uncertain timeout preserves hash.

```rust
let paid = client.pay_invoice(&invoice, PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(5)).unwrap()).await.unwrap();
assert_eq!(paid.state(), PaymentState::Succeeded);
assert_eq!(paid.payment_hash(), invoice.payment_hash());
```

Put the state-machine tests in `payment.rs` so they can drive private generated records without exposing them. Run: `cargo test -p nigiri-rs-lnd payment::tests`

Expected: FAIL because payment methods do not exist.

- [ ] **Step 2: Add pure invoice and payment conversions**

Parse `AddInvoiceResponse.payment_request` into `Bolt11Invoice`; require its payment hash to match `r_hash`. Map every known LND invoice/payment state and preserve unknown integer values. Reject negative `value_msat` and `fee_msat`, missing terminal preimages, and hashes that are not 32 bytes.

- [ ] **Step 3: Implement invoice methods**

`create_invoice` sends millisatoshis, memo, and integer expiry seconds. `lookup_invoice` sends the 32-byte hash. A timeout reports `OutcomeUnknown` because LND may have committed the invoice; a malformed response uses `InvalidResponse.identifier` when `r_hash` was present and valid.

- [ ] **Step 4: Implement Router streaming methods**

`pay_invoice` sends the BOLT11 string, `fee_limit_msat`, `timeout_seconds`, and `no_inflight_updates = false`; it consumes until one terminal state. `lookup_payment` calls `TrackPaymentV2` with `no_inflight_updates = true` and applies the same terminal conversion. A duplicate terminal record is accepted only when it is byte-for-byte equivalent; a contradictory duplicate is invalid.

- [ ] **Step 5: Extend `LightningNode` and run all LND tests**

Add these four methods with the same argument and return types as the inherent methods:

```rust
fn create_invoice(&self, request: CreateInvoiceRequest) -> impl Future<Output = Result<InvoiceRecord, Self::Error>> + Send;
fn lookup_invoice(&self, payment_hash: sha256::Hash) -> impl Future<Output = Result<InvoiceRecord, Self::Error>> + Send;
fn pay_invoice(&self, invoice: &Bolt11Invoice, options: PaymentOptions) -> impl Future<Output = Result<PaymentRecord, Self::Error>> + Send;
fn lookup_payment(&self, payment_hash: sha256::Hash) -> impl Future<Output = Result<PaymentRecord, Self::Error>> + Send;
```

Run:

`cargo test -p nigiri-rs-lnd --all-targets --doc`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/nigiri-rs-lnd
git commit -m "feat(lnd): add invoice and payment operations"
```

### Task 6: Extend the runtime adapter and container specifications for LND

**Files:**
- Modify: `crates/nigiri-rs-fixtures/Cargo.toml`
- Modify: `crates/nigiri-rs-fixtures/src/runtime/engine.rs`
- Modify: `crates/nigiri-rs-fixtures/src/runtime/supervisor.rs`
- Modify: `crates/nigiri-rs-fixtures/src/runtime/spec.rs`
- Modify: `crates/nigiri-rs-fixtures/src/runtime/mod.rs`
- Modify: `crates/nigiri-rs-fixtures/src/image.rs`
- Modify: `crates/nigiri-rs-fixtures/src/chain/bitcoin.rs`
- Create: `crates/nigiri-rs-fixtures/src/lnd.rs`

**Interfaces:**
- Consumes: `nigiri-rs-lnd` and existing `ContainerEngine`, `ContainerSpec`, `Startup`, and `ContainerImage`.
- Produces: `ContainerEngine::read_container_file`, `Startup::read_container_file`, `lnd_spec`, LND constants, and Bitcoin ZMQ argument construction.

- [ ] **Step 1: Write failing fake-engine and spec tests**

Add tests proving `Startup::read_container_file("alice", "/root/.lnd/tls.cert", 1_048_576)` delegates and rejects 1,048,577 bytes. Add an exact LND spec assertion for ports 10009 and 9735 and arguments pointing to the fixture bitcoind RPC/28332/28333 endpoints. Add an image test requiring `lightninglabs/lnd`, tag `v0.21.1-beta`, a lowercase SHA256 digest, and equality with `nigiri_rs_lnd::LND_PROTO_VERSION`.

Run: `cargo test -p nigiri-rs-fixtures --lib`

Expected: FAIL because runtime reading and LND specs do not exist.

- [ ] **Step 2: Add dependencies and runtime-neutral file reads**

Add `nigiri-rs-lnd`, `tar = "0.4.44"`, and `getrandom = "0.3.3"`. Extend the trait exactly:

```rust
async fn read_container_file(&self, id: &str, path: &str, max_bytes: usize) -> EngineResult<Vec<u8>>;
```

The Bollard implementation uses `download_from_container`, buffers at most `max_bytes` plus tar overhead, parses one regular file, rejects links/directories/multiple entries, and returns `EngineError` with operation `read container file`. Add the same cancellation-aware wrapper to `Startup`.

- [ ] **Step 3: Add the pinned image descriptor**

Verify the multi-architecture registry digest with `docker buildx imagetools inspect lightninglabs/lnd:v0.21.1-beta`, then encode `sha256:4af8f9bbf98c8b86b0e54b065d6ea45d1387256a43fa9270c11ef849511abae0` in `ContainerImage::lnd_default()`. Do not substitute a platform-specific child digest. The existing digest-validation test must cover it.

- [ ] **Step 4: Add LND and ZMQ container specs**

Define constants `LND_GRPC_PORT = 10009`, `LND_PEER_PORT = 9735`, `BITCOIN_ZMQ_BLOCK_PORT = 28332`, `BITCOIN_ZMQ_TX_PORT = 28333`, `TLS_CERT_PATH = "/root/.lnd/tls.cert"`. `lnd_spec` includes `--bitcoin.active`, `--bitcoin.regtest`, `--bitcoin.node=bitcoind`, RPC credentials, both ZMQ URLs, gRPC/peer listeners, and either `--tlsextraip` or `--tlsextradomain` for the runtime endpoint host. Do not set `--noseedbackup`: WalletUnlocker performs initialization in Task 7. Keep secrets out of `Debug` output.

- [ ] **Step 5: Run unit tests and commit**

Run: `cargo test -p nigiri-rs-fixtures --lib`

Expected: PASS without Docker.

```bash
git add Cargo.lock crates/nigiri-rs-fixtures
git commit -m "feat(fixtures): add LND runtime primitives"
```

### Task 7: Start and initialize two authenticated LND nodes

**Files:**
- Create: `crates/nigiri-rs-lnd/src/wallet_unlocker.rs`
- Modify: `crates/nigiri-rs-lnd/src/lib.rs`
- Create: `crates/nigiri-rs-fixtures/src/lnd_pair.rs`
- Modify: `crates/nigiri-rs-fixtures/src/fixture.rs`
- Modify: `crates/nigiri-rs-fixtures/src/error.rs`
- Modify: `crates/nigiri-rs-fixtures/src/lib.rs`

**Interfaces:**
- Consumes: Task 3 unauthenticated transport and Task 6 runtime operations.
- Produces: `initialize_wallet(LndBootstrapConfig, wallet_password) -> Result<LndConfig, LndError>`, `LndPair`, and `LndPairBuilder` through the synchronized-node phase.

- [ ] **Step 1: Write failing wallet-initialization and builder tests**

A fake implementation of the crate-private `WalletUnlockerRpc` interface must observe `GenSeed`, then `InitWallet { stateless_init: true }`, and return a nonempty admin macaroon. Fixture builder tests pin 180 seconds, both default images, 2,000,000/1,000,000 sats, and reject zero timeout, invalid images, push >= capacity, or either nominal side below 100,000 sats before connecting to Docker.

Run: `cargo test -p nigiri-rs-lnd wallet_unlocker && cargo test -p nigiri-rs-fixtures lnd_pair::tests`

Expected: FAIL.

- [ ] **Step 2: Expose one narrow bootstrap API from `nigiri-rs-lnd`**

```rust
pub struct LndBootstrapConfig {
    pub endpoint: Url,
    pub tls_certificate: Vec<u8>,
    pub timeout: Duration,
}

pub async fn initialize_wallet(config: LndBootstrapConfig, wallet_password: &[u8]) -> Result<LndConfig, LndError>;
```

Define a crate-private `WalletUnlockerRpc` trait whose two methods return `impl Future + Send`, implement it with the generated client, and put the sequence in `initialize_wallet_with<R: WalletUnlockerRpc>`. Require at least 8 password bytes, call `GenSeed`, call `InitWallet` with its mnemonic and `stateless_init = true`, require a nonempty admin macaroon, and return a normal authenticated `LndConfig`. Neither password nor mnemonic may occur in an error.

- [ ] **Step 3: Make the inner Bitcoin fixture composable without exposing topology publicly**

Keep `network_name`, `node_container_name`, and `extra_node_args` crate-private. Add only the crate-private endpoint/engine access needed by `LndPair`; do not expose container IDs or names publicly.

- [ ] **Step 4: Implement LND node startup under one supervisor**

Generate UUID-scoped Alice/Bob names and 32 random password bytes with `getrandom`. Start both specs concurrently with `tokio::join!`; poll bounded `read_container_file` until each TLS certificate exists; initialize wallets; build clients; and wait until both `get_info` records report Bitcoin `regtest`, `synced_to_chain`, and `synced_to_graph`. Retain the two container IDs for diagnostics and teardown tests.

- [ ] **Step 5: Attach dependency-ordered diagnostics**

Add `FixtureError::Lightning(#[source] LndError)`. On startup failure attach Bob log, Alice log, Electrs log, then bitcoind log, so the most foundational service is nearest the final error. Extend redaction tests with raw and hex macaroon/password markers.

- [ ] **Step 6: Run unit and mocked tests, then commit**

Run: `cargo test -p nigiri-rs-lnd --all-targets && cargo test -p nigiri-rs-fixtures --lib`

Expected: PASS.

```bash
git add crates/nigiri-rs-lnd crates/nigiri-rs-fixtures
git commit -m "feat(fixtures): initialize an LND node pair"
```

### Task 8: Bootstrap the channel, prove payments, and verify lifecycle

**Files:**
- Modify: `crates/nigiri-rs-fixtures/src/lnd_pair.rs`
- Modify: `crates/nigiri-rs-fixtures/src/readiness.rs`
- Modify: `crates/nigiri-rs-fixtures/src/lib.rs`
- Create: `crates/nigiri-rs-fixtures/tests/lnd_pair.rs`
- Create: `crates/nigiri-rs-fixtures/tests/lnd_wiring.rs`

**Interfaces:**
- Consumes: `NigiriClient<Bitcoin>`, both `LndClient`s, `OpenChannelRequest`, invoice/payment APIs, and the shared `Deadline`.
- Produces: fully ready `LndPair::{bitcoin,alice,bob,channel_point,shutdown}` and Docker-backed payment guarantees.

- [ ] **Step 1: Write the real payment contract test first**

```rust
#[tokio::test]
async fn pair_settles_payments_in_both_directions() {
    let pair = LndPair::start().await.unwrap();
    let a = pair.alice().create_invoice(CreateInvoiceRequest::new(Millisats::new(25_000), "bob-to-alice", Duration::from_secs(60)).unwrap()).await.unwrap();
    let paid_a = pair.bob().pay_invoice(a.invoice(), PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(30)).unwrap()).await.unwrap();
    assert_eq!(paid_a.state(), PaymentState::Succeeded);
    assert_eq!(pair.alice().lookup_invoice(a.payment_hash()).await.unwrap().state(), InvoiceState::Settled);

    let b = pair.bob().create_invoice(CreateInvoiceRequest::new(Millisats::new(25_000), "alice-to-bob", Duration::from_secs(60)).unwrap()).await.unwrap();
    let paid_b = pair.alice().pay_invoice(b.invoice(), PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(30)).unwrap()).await.unwrap();
    assert_eq!(paid_b.state(), PaymentState::Succeeded);
}
```

Run: `cargo test -p nigiri-rs-fixtures --test lnd_pair pair_settles_payments_in_both_directions -- --nocapture`

Expected: FAIL because channel bootstrap is absent.

- [ ] **Step 2: Fund Alice and recognize the output**

Request a P2WPKH address from Alice, call the Bitcoin fixture client's `faucet` with channel capacity plus 200,000 sats (which commits and mines one block), and poll Alice's `wallet_balance().confirmed()` until it covers the channel request. Every poll consumes the shared deadline.

- [ ] **Step 3: Connect peers and open the balanced channel**

Connect Alice to `bob-container-name:9735`, open with configured capacity and push, mine six regtest blocks, and poll both `list_channels` results until they contain the same active `OutPoint`. Require each reported local spendable balance to exceed 1,000 millisats.

- [ ] **Step 4: Perform the startup payment proof**

Bob creates exactly 1,000 millisats with memo `nigiri-rs readiness probe`; Alice pays it with a 10,000-millisat fee limit; poll Bob's invoice by hash until settled. Store the channel point only, not the readiness invoice or password.

- [ ] **Step 5: Complete ownership and teardown behavior**

Hold the LND runtime before `Fixture<Bitcoin>` in `LndPair`. `shutdown` must attempt LND cleanup and Bitcoin cleanup even when the first fails, returning the first error. Add tests that inspect and then verify removal of four containers, their anonymous volumes, and the network; add a cancellation test for failure after Alice has started; add a parallel-pair collision test.

- [ ] **Step 6: Run Docker integration and all fixture tests**

Run: `cargo test -p nigiri-rs-fixtures --all-targets --all-features -- --nocapture`

Expected: PASS with Docker running; both directions settle and teardown leaves no owned resources.

- [ ] **Step 7: Commit**

```bash
git add crates/nigiri-rs-fixtures
git commit -m "feat(fixtures): add ready-to-pay LND pair"
```

### Task 9: Expose LND through the facade and test macro

**Files:**
- Modify: `crates/nigiri-rs/Cargo.toml`
- Modify: `crates/nigiri-rs/src/lib.rs`
- Modify: `crates/nigiri-rs/tests/facade_reexports.rs`
- Modify: `crates/nigiri-rs/tests/private_reexports.rs`
- Modify: `crates/nigiri-rs/tests/macro_smoke.rs`
- Modify: `crates/nigiri-rs-macros/src/parse.rs`
- Modify: `crates/nigiri-rs-macros/src/expand.rs`
- Modify: `crates/nigiri-rs-macros/tests/ui/unsupported_parameter.stderr`

**Interfaces:**
- Consumes: published `nigiri-rs-lnd` types and `nigiri_rs_fixtures::LndPair`.
- Produces: facade `lnd` feature/re-exports and `#[nigiri_rs::test] async fn test(pair: LndPair)` support.

- [ ] **Step 1: Write failing facade and macro parser tests**

Add compile assertions for `nigiri_rs::{LndClient, LndConfig, LightningNode}` under `lnd`, and parsing tests for bare and qualified `LndPair`. Update the accepted list to exactly `` `NigiriClient<Bitcoin>`, `NigiriClient<Liquid>`, `PegPair`, or `LndPair` ``.

Run: `cargo test -p nigiri-rs-macros && cargo test -p nigiri-rs --features lnd --test facade_reexports`

Expected: FAIL.

- [ ] **Step 2: Add facade features and re-exports**

```toml
lnd = ["dep:nigiri-rs-lnd"]
fixtures = ["lnd", "dep:nigiri-rs-fixtures", "dep:nigiri-rs-macros", "dep:tokio"]
```

Add optional `nigiri-rs-lnd` dependency and `#[cfg(feature = "lnd")] pub use nigiri_rs_lnd::*;`. Keep the fixture crate under the existing `fixtures` module.

- [ ] **Step 3: Add an `LndPair` macro variant**

Recognize only a final path segment `LndPair` with no generic arguments. Expand startup to `::nigiri_rs::__private::fixtures::LndPair::{start,builder}` and move the owning pair into the test body. Extend `ident`, failure-name, single-start, and concurrent-start match arms.

- [ ] **Step 4: Add the real macro payment smoke test**

Inject `LndPair`, create a 25,000-millisat Bob invoice, pay it from Alice, and assert `Succeeded` and `Settled`. Use `startup_timeout = 240` for first image pull CI headroom.

- [ ] **Step 5: Run macro, facade, and Docker smoke tests**

Run: `cargo test -p nigiri-rs-macros && cargo test -p nigiri-rs --all-features --all-targets -- --nocapture`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.lock crates/nigiri-rs crates/nigiri-rs-macros
git commit -m "feat: expose LND clients and fixtures"
```

### Task 10: Complete documentation, CI, packaging, and release verification

**Files:**
- Modify: `README.md`
- Modify: `TODOS.md`
- Modify: `docs/README.md`
- Modify: `docs/explanation-lifecycle-ownership.md`
- Modify: `docs/explanation-fixture-readiness.md`
- Modify: `docs/how-to-run-a-fixture.md`
- Modify: `docs/how-to-point-at-your-own-services.md`
- Modify: `docs/reference-client.md`
- Modify: `docs/reference-errors.md`
- Modify: `docs/reference-fixtures.md`
- Modify: `docs/reference-test-macro.md`
- Create: `docs/how-to-use-lnd.md`
- Create: `docs/tutorial-lightning-payment.md`
- Modify: `.github/workflows/ci.yml`
- Modify: package descriptions in `crates/nigiri-rs*/Cargo.toml`

**Interfaces:**
- Consumes: every public API and feature finalized in Tasks 1-9.
- Produces: accurate Diataxis documentation, CI coverage, and publish order for all five crates.

- [ ] **Step 1: Add compile-checked host and fixture examples**

The host example must use `LndConfig::from_files("https://127.0.0.1:10009", "tls.cert", "admin.macaroon", Duration::from_secs(30))`, construct `LndClient`, and call `wait_ready`. The fixture tutorial must start `LndPair`, create a Bob invoice, pay from Alice, and assert terminal state by payment hash.

- [ ] **Step 2: Update reference and explanation documents**

Document every method, record, error case, feature implication, default amount/deadline, readiness-probe history entry, four-container teardown order, and host credential security warning. Replace every project description that says only Bitcoin and Liquid. Do not describe the core crate as owning Lightning.

- [ ] **Step 3: Update CI scopes and packaging checks**

Add `-p nigiri-rs-lnd` to all Docker-free test matrix cells. Package `nigiri-rs-lnd` alongside core and macros. Retain stable/all-features workspace testing as the only Docker cell. Increase only that job's timeout if measured cold-image startup exceeds 20 minutes; do not weaken or ignore LND tests.

- [ ] **Step 4: Run formatting and the Docker-free verification matrix**

Run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p nigiri-rs-core -p nigiri-rs-lnd -p nigiri-rs-macros --all-targets --all-features
cargo test --workspace --doc --all-features
cargo package -p nigiri-rs-core --all-features
cargo package -p nigiri-rs-lnd --all-features
cargo package -p nigiri-rs-macros --all-features
```

Expected: all commands PASS on Rust 1.88 and stable.

- [ ] **Step 5: Run the full Docker-backed release gate**

Run:

```bash
cargo test -p nigiri-rs-fixtures --all-targets --all-features -- --nocapture
cargo test -p nigiri-rs --all-targets --all-features -- --nocapture
cargo test --workspace --all-targets --all-features -- --nocapture
```

Expected: all commands PASS; the LND integration and macro tests settle real payments.

- [ ] **Step 6: Audit secrets, pins, and working tree**

Run:

```bash
rg -n "admin_macaroon|wallet_password|cipher_seed|rpcpassword" target/doc README.md docs crates --glob '!*.proto'
rg -n "v0\.21\.1-beta|2b87887|lightninglabs/lnd" crates/nigiri-rs-lnd crates/nigiri-rs-fixtures
git diff --check
git status --short
```

Expected: documentation names only paths/field concepts, no secret values appear, all three LND pins agree, no whitespace errors exist, and only intentional files are changed.

- [ ] **Step 7: Commit**

```bash
git add .github/workflows/ci.yml Cargo.lock README.md TODOS.md docs crates/*/Cargo.toml
git commit -m "docs: document LND clients and fixtures"
```

- [ ] **Step 8: Request final code review**

Use `superpowers:requesting-code-review` against the complete diff from the design commit. Resolve correctness, security, lifecycle, and documentation findings before considering the implementation complete.
