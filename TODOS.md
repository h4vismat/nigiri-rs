# TODOS

Open work for `nigiri-rs`, grouped by area then priority (P0 highest, P4 lowest).
Completed items move to the bottom with the version that shipped them.

## CI / Distribution

### Add a crates.io publish workflow

**Priority:** P2

Each of the four workspace manifests (`nigiri-rs-core`, `nigiri-rs-macros`,
`nigiri-rs-testcontainers`, `nigiri-rs`) carries `license`, `description`, and `repository`, so
every crate is shaped for publication, but nothing automates `cargo publish` on a tag. Releases
are manual and unverified. Make it idempotent so re-running a failed release does not error.

Publish order, worked out for CI dependency resolution: `nigiri-rs-core` first (no workspace
dependencies); then `nigiri-rs-macros` and `nigiri-rs-testcontainers`, which depend only on
`nigiri-rs-core` and can publish in either order relative to each other; then `nigiri-rs`, the
facade, which depends on all three.

## Test suite hygiene

### Deduplicate test scaffolding

**Priority:** P4

Surfaced by the 0.3.0 specialist review:

- `LiquidBlockchainInfo` is declared in both `crates/nigiri-rs-core/tests/rpc_api.rs` and
  `crates/nigiri-rs-testcontainers/tests/liquid_fixture.rs` with the same three fields but
  `blocks: i64` in one and `u64` in the other. The divergence is arbitrary.
## Completed

### Exercise all feature combinations in CI

**Completed:** current main

`.github/workflows/ci.yml` runs formatting, Clippy with warnings denied, default- and
all-feature target tests on Rust 1.88 and stable, all-feature doctests, and packaging checks.
Exactly one matrix cell (stable, all-features) runs the full workspace, including the
Docker-backed `nigiri-rs-testcontainers` suite; the other three cells scope to
`nigiri-rs-core`, which needs no Docker.

### Bound `max_rpc_response_bytes` from above

**Completed:** v0.3.0

`validate_and_normalize` rejected only zero, leaving the sole ceiling on process-output
buffering unbounded above. Added `MAX_RPC_RESPONSE_BYTES_LIMIT` (16 MiB) and a rejection with
a boundary test.

### Use a distinct error variant for pre-spawn input rejection

**Completed:** v0.3.0

Added `NigiriError::InvalidRequest { detail }`. Configuration validation and RPC method-name
validation no longer masquerade as `InvalidResponse` with a synthetic `"configuration"` or
`"RPC method validation"` operation label, so callers stop string-matching a pseudo-operation
to distinguish caller error from node failure. Breaking; batched with the `Cow` error-label
change in the same release.

### Replace fixed sleeps in the child-kill tests with polling

**Completed:** v0.3.0

Both marker tests now poll for up to 5 seconds via a shared `marker_stays_absent` helper and
fail the moment the marker appears, instead of a single `sleep(1_200ms)` racing the fixture's
`sleep 1` with ~200 ms of margin.

### Record why the new host RPC tests skip `HostChainLock`

**Completed:** v0.3.0

Both call sites now state that the omission is deliberate and why it is safe: the tests only
read, and their assertions hold even against a tip observed mid-reorg.
