# TODOS

Open work for `nigiri-rs`, grouped by area then priority (P0 highest, P4 lowest).
Completed items move to the bottom with the version that shipped them.

## CI / Distribution

### Add a CI workflow that exercises all feature combinations

**Priority:** P1

There is no `.github/workflows/` directory. Plain `cargo test` covers neither
`tests/rpc_types_feature.rs` (entirely `#![cfg(feature = "bitcoin-rpc-types")]`) nor the
gated test in `tests/rpc_api.rs`, so the optional feature can break without any signal.
The host suites are `#[ignore]` and never run unattended.

Wanted:
- `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets` (default features) **and** `--all-features`
- `cargo test --doc --all-features`
- Pinned `rust-version = "1.85"` from `Cargo.toml`, plus stable
- Optionally a job that boots Nigiri and runs the `#[ignore]` host suites

### Add a crates.io publish workflow

**Priority:** P2

`Cargo.toml` carries `license`, `description`, and `repository`, so the crate is shaped for
publication, but nothing automates `cargo publish` on a tag. Releases are manual and
unverified. Make it idempotent so re-running a failed release does not error.

## RPC executor

### Shrink the redaction delta map to cut error-path memory

**Priority:** P3

`bounded_redacted` in `src/rpc.rs` allocates `vec![0_i32; stderr.len() + 1]` to track
overlapping redaction ranges — 4 bytes per retained stderr byte. Combined with the lossy
UTF-8 copy, formatting a failed RPC costs several times `NigiriConfig::max_rpc_response_bytes`.
That knob is caller-controlled and unbounded, so a gigabyte-range limit turns an RPC failure
into an out-of-memory abort. The cost is documented on the field as of 0.3.0; this item is the
actual fix.

Nesting depth is bounded by the number of caller arguments (small), so a narrower integer
type, or a two-pass merge of sorted ranges, would drop this to ~1x with no behavior change.

### Consider making the Esplora HTTP body limit configurable too

**Priority:** P4

`src/http.rs` keeps its own fixed `MAX_BODY_BYTES = 64 * 1024` for Esplora responses, while
the CLI path is now configurable via `NigiriConfig::max_rpc_response_bytes`. The field name
scopes the promise correctly, so this is not a bug, but a caller raising one limit may expect
the other to follow.

## Completed

_(nothing yet)_
