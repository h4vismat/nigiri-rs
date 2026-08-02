# TODOS

Open work for `nigiri-rs`, grouped by area then priority (P0 highest, P4 lowest).
Completed items move to the bottom with the version that shipped them.

## CI / Distribution

### Add a crates.io publish workflow

**Priority:** P2

`Cargo.toml` carries `license`, `description`, and `repository`, so the crate is shaped for
publication, but nothing automates `cargo publish` on a tag. Releases are manual and
unverified. Make it idempotent so re-running a failed release does not error.

## Test suite hygiene

### Deduplicate test scaffolding

**Priority:** P4

Surfaced by the 0.3.0 specialist review:

- `LiquidBlockchainInfo` is declared in both `tests/rpc_api.rs` and `tests/host_liquid.rs` with
  the same three fields but `blocks: i64` in one and `u64` in the other. The divergence is
  arbitrary.
- The fixture's payload sizes (`70000`, `60000`) are duplicated as bare Rust literals whose
  only significance is straddling `DEFAULT_MAX_RPC_RESPONSE_BYTES`. Nothing names the relation,
  so changing the default breaks the tests with no indication which side is wrong.
## Completed

### Exercise all feature combinations in CI

**Completed:** current main

`.github/workflows/ci.yml` runs formatting, Clippy with warnings denied, default- and
all-feature target tests on Rust 1.85 and stable, all-feature doctests, packaging checks,
and separately gated host suites.

### Apply one configurable response limit to every transport

**Completed:** v0.4.0

Renamed the public setting to `max_response_bytes` and now apply it to node JSON-RPC,
Chopsticks, and Esplora response bodies. The fixed Esplora-only `MAX_BODY_BYTES` limit is gone.

### Bound `max_rpc_response_bytes` from above

**Completed:** v0.3.0

`validate_and_normalize` rejected only zero, leaving the sole ceiling on process-output
buffering unbounded above. Added `MAX_RPC_RESPONSE_BYTES_LIMIT` (16 MiB) and a rejection with
a boundary test. The remaining error-excerpt half is tracked separately as P3.

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
