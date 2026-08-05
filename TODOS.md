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

Publish order, worked out for CI dependency resolution. `nigiri-rs-macros` has no workspace
dependency at all — only `proc-macro2`, `quote`, and `syn` — because it emits
`::nigiri_rs::__private::…` as tokens and never compiles against the types, so it can publish at
any point. The constraint is `nigiri-rs-core` first, then `nigiri-rs-testcontainers`, which does
depend on it, then `nigiri-rs`, the facade, which depends on all three.

### Overlap the node and Electrs container starts

**Priority:** P3

`FixtureBuilder::start` brings the node fully up — container start, RPC readiness, `createwallet`,
and the whole 101-block fund — before it calls `electrs::start_electrs`. There is no compile-time
dependency between them: `start_electrs` takes the node's *name* as a string, computed by
`topology_names` before either container exists. Both could run under `tokio::try_join!`, which
would take Electrs' own container bring-up off the critical path of every one of the 18
fixture-starting tests.

Two things to settle before doing it, neither answerable without container runs. Electrs must
tolerate a node whose RPC is not yet listening; its `--jsonrpc-import` daemon is expected to
retry, but that is unverified here. And the current ordering is load-bearing for diagnostics: the
error branch attaches the node's container log to an Electrs failure, on the stated grounds that
"the node is running and holds the only account of what Electrs was pointed at." Joining them
means an Electrs failure can arrive while the node does not yet exist, so that pairing needs
rethinking rather than deleting. Measure the saving before committing to it.

### Probe the three services concurrently in `observe_heights`

**Priority:** P4

`readiness::observe_heights` issues its node `getblockcount`, Esplora `block_height`, and Electrum
`tip_height` probes strictly in sequence each poll round, though nothing in a round depends on
another probe's result — only the combined `Heights` is compared afterwards. Every retry round
pays the sum of three round trips instead of the longest. Each probe is already bounded by the
shared `Deadline`, so `tokio::join!` would not change timeout semantics.

The saving is unmeasured and plausibly single-digit milliseconds per round on a loopback Docker
port, which is why this sits below the start-overlap item. Worth measuring before touching
readiness logic, which is the most load-bearing code in the crate.

## Container images

### Move both indexer pins to a stable Mempool release

**Priority:** P3

`ContainerImage::electrs_default` and `electrs_liquid_default` both pin
`mempool/electrs*:v3.4.0-dev1`, a pre-release build. The Bitcoin pin had a choice — `v3.3.0` is
stable and available — and is held at `dev1` only so both chains run the same indexer build, because
`mempool/electrs-liquid` publishes nothing but `latest` and that dev tag. A version skew between the
two would surface as a chain difference in a suite that asserts the same behaviour against both.

Blocked on upstream, not on us: when Mempool publishes `v3.4.0` stable for the Liquid variant, move
both pins to it and refresh both digests. Nothing else has to change — the flag vectors both chains
build are already flag-compatible with this fork, verified against `--help` on both images.

## Test suite hygiene

### Deduplicate test scaffolding

**Priority:** P4

Surfaced by the 0.3.0 specialist review:

- `LiquidBlockchainInfo` is declared in both `crates/nigiri-rs-core/tests/rpc_api.rs` and
  `crates/nigiri-rs/tests/macro_smoke.rs` with the same three fields but `blocks: i64` in one and
  `u64` in the other. The divergence is arbitrary. It moved out of the fixtures crate when the two
  Liquid tests that used it were rewritten to `#[nigiri_rs::test]`, so the duplication crossed a
  crate boundary rather than going away.

## Completed

### Exercise all feature combinations in CI

**Completed:** current main

`.github/workflows/ci.yml` runs formatting, Clippy with warnings denied, default- and
all-feature target tests on Rust 1.88 and stable, all-feature doctests, and packaging checks.
Exactly one matrix cell (stable, all-features) runs the full workspace, including the
Docker-backed `nigiri-rs-testcontainers` suite; the other three scope to `nigiri-rs-core` and
`nigiri-rs-macros`, neither of which needs Docker. The macro crate is in the narrow cells
deliberately: its `trybuild` suite is pure compile-fail checking, and macro diagnostics are worth
checking against the 1.88 floor as well as stable.

### Bound `max_response_bytes` from above

**Completed:** v0.3.0

`validate_and_normalize` rejected only zero, leaving the sole ceiling on process-output
buffering unbounded above. Added `MAX_RESPONSE_BYTES_LIMIT` (16 MiB) and a rejection with
a boundary test.

Both were named `max_rpc_response_bytes` and `MAX_RPC_RESPONSE_BYTES_LIMIT` when this shipped.
The `rpc` dropped out once the limit also bounded Esplora responses, so the entry uses the names
that exist today rather than the ones it was written with.

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
