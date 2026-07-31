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

**Priority:** P3

`kill_and_reap` in `src/rpc.rs` signals only the process the crate spawned. `nigiri` (a Go
binary) runs the `docker` CLI as its own child, verified by observing the tree during an RPC:

```
nigiri   pid 59328
docker   pid 59364, ppid 59328
```

SIGKILL to `nigiri` orphans `docker`, which reparents to init and keeps its inherited stdout
and stderr write ends open. Two consequences worth fixing:

- Every timeout leaks one `docker` process, each holding an exec session against dockerd.
- Pipe EOF requires every writer to close, so the orphan is why `capture_process_output`
  depends on the outer timeout to escape rather than on the streams actually ending.

Fix: `Command::process_group(0)` on Unix, then signal the group. Needs `libc` (or `nix`) as a
direct dependency for `killpg`, and a fixture method that backgrounds a grandchild so the gap
becomes testable.

**This does not make timeouts atomic.** Once `docker exec` has handed an RPC to the node, that
work completes inside the container; killing a client is not cancellation. An earlier revision
of this item claimed process-group kill would stop a timed-out mutating RPC from committing.
It will not, which is why this is P3 and not P1. Node-state recovery stays host-owned.

Related test-integrity problem, and the stronger reason to do this: both marker tests assert
that a killed child cannot perform a follow-up side effect, but the fixture is a single `sh`
with no children, so they pass by construction and cannot fail for the production shape they
are named after.

### Cap the error excerpt independently of the response limit

**Priority:** P3

`bounded_redacted` is handed the full `max_rpc_response_bytes` even though its only consumer
is a human-readable `NigiriError::RpcFailed { stderr }` message. Passing
`limit.min(ERROR_EXCERPT_BYTES)` (8-16 KiB) at the three call sites would cap the redaction
pass and its allocation regardless of how high a caller raises the response limit. A CLI
diagnostic never needs more than a few KiB.

The absolute upper bound shipped in 0.3.0; this is the remaining half.

### Share one redaction implementation between the CLI and HTTP paths

**Priority:** P3

`src/http.rs::bounded_error_text` still does a naive `String::replace` over the body, so it
cannot redact a secret split by the truncation cut and does not normalize ANSI before matching.
`src/rpc.rs::bounded_redacted` does both. Two redaction routines with materially different
guarantees is a trap: a reader cannot tell which applies where. Extract one (bytes in, bounded
redacted string out) and call it from both.

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
  reconciled in 0.3.0 and the test copy carries a comment saying why, but nothing enforces it.

## Completed

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
