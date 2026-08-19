# Task 7 report — authenticated synchronized LND node pair

## Result

- Added the narrow public `initialize_wallet(LndBootstrapConfig, wallet_password)` bootstrap API.
  It validates transport and password bounds before RPC, generates a fresh 24-word seed, performs
  stateless `InitWallet`, requires a bounded nonempty returned admin macaroon, and constructs the
  normal authenticated `LndConfig` without a macaroon-file fallback.
- Added `LndPair` and `LndPairBuilder`. The builder validates all inputs before Docker, starts a
  ZMQ-enabled backing Bitcoin fixture and UUID-scoped Alice/Bob LND containers on its private
  network, initializes both wallets with independent 32-byte operating-system-random passwords,
  and returns authenticated clients only after both LND nodes report regtest chain/graph sync at
  the backing bitcoind height.
- One `Deadline` covers Bitcoin startup, concurrent LND startup, bounded 1 MiB TLS-certificate
  reads, wallet initialization, and synchronization. One LND supervisor owns both containers and
  reserves Alice then Bob before concurrent startup, giving deterministic Bob-then-Alice teardown.
- Added typed `FixtureError::Lightning(#[source] LndError)` failures. Startup errors retain that
  cause inside the existing Lightning bootstrap context and attach Bob, Alice, Electrs, then
  bitcoind diagnostics while redacting raw and hex password/macaroon markers.
- Tightened caller-cancellation behavior so aborting a startup waits for the dedicated supervisor
  to complete reverse-order cleanup. The LND runtime is owned before the backing Bitcoin fixture,
  so implicit and explicit teardown preserve dependency order.

No public container name/ID or runtime engine handle was exposed. Channel funding, channel
creation, payments, and a public channel point remain Task 8 work.

## Correctness-review fixes

- Fixture diagnostics now redact the complete structured value for the real `rpcpass` option and
  wallet-password punctuation variants, the complete line for seed/mnemonic fields, and complete
  PEM private-key blocks. Redaction runs over the full input before bounding, remains idempotent,
  and is exercised in direct diagnostics plus Alice, Bob, Electrs, and bitcoind log attachment.
- TLS-certificate polling retries only missing/not-yet-available file states. Oversize, malformed
  archive/framing/path, and metadata failures return immediately with runtime classification and a
  bounded source. LND synchronization retries transport/availability statuses only; permanent
  authentication, invalid-request, invalid-response, and other protocol failures immediately keep
  the `Bootstrap { chain: "Lightning" } -> FixtureError::Lightning -> LndError` source chain.
- A representable absolute Tokio `Instant` is now validated before Docker. The same `Deadline`
  bounds engine connection, topology startup, failure diagnostics, and cleanup coordination. If
  failure cleanup completes while budget remains, the caller joins it. At expiry, diagnostics are
  skipped or cancelled, shutdown is signalled, and the dedicated supervisor thread is detached so
  the public startup call returns on time while the supervisor continues best-effort reverse-order
  cleanup in the background. Normal post-start explicit and implicit shutdown still waits for
  cleanup.

## TDD evidence

The initial focused tests failed on the absent `WalletUnlockerRpc`, `initialize_wallet`,
`LndBootstrapConfig`, `LndPair`, and `LndPairBuilder` interfaces. The completed wallet tests cover
the exact `GenSeed`/stateless `InitWallet` sequence and fields, password and mnemonic bounds,
secret-free errors/debug output, returned configuration, and uncertain outcomes once
`InitWallet` may have committed.

The fixture tests cover defaults and every override, pre-Docker validation, concurrent container
startup, exact shared topology, the certificate byte cap, independent passwords, authenticated
client creation, backing-bitcoind synchronization, typed/redacted dependency diagnostics,
deterministic shutdown, ownership order, and caller cancellation. A strengthened cancellation test
first failed because aborting the caller could return before delayed container removals completed;
the supervisor now owns and joins its cleanup thread from the cancellation guard.

The correctness-review RED pass then exposed all three reported boundaries: exact RPC/seed/private
key fixtures remained visible; terminal certificate and `GetInfo` failures were retried; an
unrepresentable `Duration::MAX` was accepted, a blocked diagnostic read outlived the whole-call
clock, and 30-ms caller cancellation waited about 506 ms for two delayed removals. The GREEN pass
adds faithful terminal/transient fakes, paused-time blocking diagnostics, and delayed cleanup. The
last case returns within its caller budget and then observes Bob-before-Alice cleanup complete in
the detached supervisor.

The full workspace feature run also exposed a rustls test-harness integration bug: tonic's
production client path selects its compiled provider explicitly, but tonic's in-process TLS server
asks rustls for a process default. Workspace feature unification enables ring through tonic and
AWS-LC through reqwest, so inference panicked. A dev-only, idempotent test-harness initializer now
respects an existing provider and otherwise selects ring. Production transport behavior and the
public Task 3 contract are unchanged; the feature-unified regression passes.

## Security and protocol rulings

- The checked-in LND v0.21.1 proto requires at least 8 password bytes and defines/generated-seed
  behavior around exactly 24 mnemonic words. No stricter pinned implementation source is present,
  so private defensive caps are 64 KiB for the wallet password, 1 KiB per nonempty mnemonic word
  with checked aggregate arithmetic, 1 MiB for the TLS certificate, and the existing 64 KiB
  macaroon cap.
- Any transport/status failure after `InitWallet` is sent, or a missing/oversized returned admin
  macaroon, becomes `LndError::OutcomeUnknown`; blindly retrying with a different generated seed is
  unsafe because the wallet may already be committed.
- Passwords and generated mnemonics never enter errors, diagnostic registration, fixture fields,
  or `Debug`. Password arrays live only in the initialization helper's stack frame and are dropped
  before synchronization. Returned macaroons are consumed directly into `LndClient` configuration.
- Startup Lightning failures use the existing `Bootstrap { chain: "Lightning", ... }` context with
  `FixtureError::Lightning` as the typed nested source. Direct non-bootstrap conversions remain the
  exact `FixtureError::Lightning(#[source] LndError)` variant.

## Verification

- `cargo test --workspace --all-targets --all-features --locked` — passed, including all Docker-backed
  Bitcoin, Liquid, peg, macro, LND, protocol-baseline, and fixture suites; the LND library ran 87
  unit tests and the fixture library ran 120 unit tests.
- `cargo test -p nigiri-rs-fixtures lnd_pair::tests --locked` — 13 passed, including permanent and
  transient retry classification, blocked diagnostics, bounded cancellation, and eventual cleanup.
- `cargo test -p nigiri-rs-fixtures runtime::supervisor::tests --locked` — 6 passed.
- `cargo test -p nigiri-rs-fixtures diagnostics::tests --locked` — 12 passed.
- `cargo test -p nigiri-rs-lnd wallet_unlocker --locked` — 6 passed.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` — passed.
- `cargo test --doc --workspace --all-features --locked` — passed.
- `cargo doc --workspace --all-features --no-deps --locked` — passed.
- `cargo fmt --all -- --check` — passed.
- `git diff --check` — passed.

Only the installed stable Rust toolchain was available locally, so a separate Rust 1.88 toolchain
check was not run. Every modified crate retains `rust-version = "1.88"`, and the workspace checks
above pass without adding production dependencies or language features beyond that contract.

## Commit

- `feat(fixtures): initialize an LND node pair`
- `fix(fixtures): harden LND startup failure boundaries`
