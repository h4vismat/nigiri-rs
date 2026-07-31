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

### Kill the whole process group, not just the direct child

**Priority:** P1

`kill_and_reap` in `src/rpc.rs` signals only the child the crate spawned. Real `nigiri` is a
shell wrapper that execs `docker`, so SIGKILL to the wrapper leaves the `docker exec` and the
RPC it already dispatched running to completion. A mutating RPC that times out can therefore
still commit on the node, which is exactly what the timeout is supposed to prevent.

The fixture is a single `sh` with no children, so both marker tests pass while the host case is
unprotected. Fix: `Command::process_group(0)` on Unix and signal the group. Needs a fixture
method that backgrounds a grandchild so the gap is testable. Documented as a known limitation
in README.md and MIGRATION.md as of 0.3.0.

### Bound `max_rpc_response_bytes` from above

**Priority:** P2

`validate_and_normalize` only rejects zero. The field is the sole ceiling on process-output
buffering, and formatting one failed RPC allocates several times its value (a 4-byte-per-byte
redaction map plus a lossy UTF-8 copy that can expand threefold). A value read from a config
file or environment variable in the gigabyte range turns a single RPC failure into an abort.
The doc comment warns; nothing enforces.

Related: the error excerpt does not need the same bound as the response. Passing
`limit.min(ERROR_EXCERPT_BYTES)` (8-16 KiB) into `bounded_redacted` would cap the redaction
pass regardless of how high a caller raises the response limit.

### Share one redaction implementation between the CLI and HTTP paths

**Priority:** P3

`src/http.rs::bounded_error_text` still does a naive `String::replace` over the body, so it
cannot redact a secret split by the truncation cut and does not normalize ANSI before matching.
`src/rpc.rs::bounded_redacted` does both. Two redaction routines with materially different
guarantees is a trap: a reader cannot tell which applies where. Extract one (bytes in, bounded
redacted string out) and call it from both.

### Replace fixed sleeps in the child-kill tests with polling

**Priority:** P3

`stream_limit_breaches_kill_the_child_before_follow_up_side_effects` and
`timeout_kills_the_child_before_it_can_write_its_marker` both prove the kill via a single
`sleep(1_200ms)` against the fixture's `sleep 1` — about 200 ms of margin. On a loaded machine
a surviving child writes its marker after the assertion has run, so a broken `kill_and_reap`
yields a silent pass. Poll for the marker over a generous budget and assert it never appears,
or assert on the reaped status instead of a filesystem side effect.

### Consider making the Esplora HTTP body limit configurable too

**Priority:** P4

`src/http.rs` keeps its own fixed `MAX_BODY_BYTES = 64 * 1024` for Esplora responses, while
the CLI path is now configurable via `NigiriConfig::max_rpc_response_bytes`. The field name
scopes the promise correctly, so this is not a bug, but a caller raising one limit may expect
the other to follow.

## Test suite hygiene

### Deduplicate test scaffolding

**Priority:** P4

Surfaced by the 0.3.0 specialist review:

- The five-field `NigiriConfig` literal with `http://127.0.0.1:1` for both URLs appears seven
  times across `src/rpc.rs`, `src/liquid.rs`, and `tests/rpc_api.rs`, four of them where
  `fake_client_with_limit` already exists and only the `executable` differs. Adding the next
  required config field means editing seven sites again.
- `LiquidBlockchainInfo` is declared in both `tests/rpc_api.rs` and `tests/host_liquid.rs` with
  the same three fields but `blocks: i64` in one and `u64` in the other. The divergence is
  arbitrary.
- The fixture's payload sizes (`70000`, `60000`) are duplicated as bare Rust literals whose
  only significance is straddling `DEFAULT_MAX_RPC_RESPONSE_BYTES`. Nothing names the relation,
  so changing the default breaks the tests with no indication which side is wrong.
- `strip_ansi` exists in both `src/rpc.rs` and `tests/support/mod.rs`. The copies were
  reconciled in 0.3.0, but nothing stops them diverging again.

### Record why the new host RPC tests skip `HostChainLock`

**Priority:** P4

`bitcoin_public_rpc_deserializes_native_and_core_v30_types` and
`liquid_public_rpc_deserializes_native_elements_types` are the only host tests that do not
acquire the lock. README calls this deliberate for read-only tests, but they sit beside the
reorg tests that invalidate the tip, so an unlocked read can observe a mid-reorg chain unless
every invocation remembers `--test-threads=1`. Either take the lock (cheap) or comment the
omission at each call site.

## API surface

### Use a distinct error variant for pre-spawn input rejection

**Priority:** P4

`validate_rpc_method` reports an invalid caller-supplied method name as
`NigiriError::InvalidResponse` with a synthetic `"RPC method validation"` operation label. No
process was ever spawned, so nothing produced an invalid response, and callers can only
distinguish the case by string-matching that pseudo-label. The same sentinel-label pattern is
used for configuration errors. A dedicated `InvalidRequest { detail }` variant would say what
actually happened.

## Completed

_(nothing yet)_
