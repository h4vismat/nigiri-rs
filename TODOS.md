# TODOS

Open work for `nigiri-rs`, grouped by area then priority (P0 highest, P4 lowest).
Completed items move to the bottom with the version that shipped them.

## CI / Distribution

### Reconcile the install snippets with what is actually published

**Priority:** P2

Every install snippet in `README.md` and under `docs/` names `version = "0.5"`, and
`docs/README.md:78-79` states plainly that 0.5.0 is not installable from crates.io. Both are true and
they contradict each other: only the facade's 0.2.0 was ever published, so a reader who copies the
snippet gets a version Cargo cannot resolve.

This predates the 0.5.0 release rather than being caused by it — the snippets said `0.4` while 0.4.0
was equally unpublished. Marking the release simply made the mismatch explicit, since the page now
says out loud what the snippet was already getting wrong.

Two ways out, and they are alternatives rather than steps: publish the four crates, which makes every
existing snippet true and depends on the publish workflow below; or make the git-plus-pinned-rev form
the primary snippet everywhere and keep the version table as a record of what the workspace contains,
which is honest immediately and needs no release. Deliberately left as-is until that choice is made,
because rewriting the snippets twice is worse than rewriting them once.

Held to be low-severity on the grounds that the crate has no external consumers. That reasoning
expires the moment it acquires one.

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

## Diagnostics

### Attach the pair's container logs to a `Peg::connect` timeout

**Priority:** P4

`PegPairBuilder::start` runs `Peg::connect` under the shared deadline and propagates its expiry with
`?`. That expiry is a `FixtureError::ReadinessTimeout`, which is one of only two variants
`owned_start::attach_diagnostics` will enrich — so container logs *could* be attached there and are
not. It is the one remaining place a pair could explain its own failure better than it does.

Deliberately narrow. The neighbouring `Peg::connect` *failure* path is a different case and was
settled during the peg-fixtures review: it arrives as `FixtureError::Client`, which has no
diagnostics field, so the two `attach_inner_logs` calls that used to sit there read two container
logs and discarded both. Those were removed rather than made to work. This item is only about the
timeout path, where the variant can carry what is read.

Low value in practice: a `Peg::connect` timeout means a hung `getsidechaininfo` on a node that
already came up and passed its own readiness check, so the container log is less likely to be the
thing that explains it than it is for a node that never started.

## Documentation

### Make the container-cleanup one-liners safe when nothing matches

**Priority:** P4

`docs/reference-fixtures.md` and `docs/how-to-run-a-fixture.md` both give a hard-kill cleanup recipe
built on `docker rm -f -v $(docker ps -aq --filter ...)`. When the filter matches nothing, the
command substitution is empty and `docker rm` exits with a usage error rather than doing nothing —
noise at exactly the moment a reader is already confused about whether they leaked containers. The
same shape now appears in two pages, so it is worth fixing once in both.

Pre-existing style rather than something this work introduced; the peg-fixtures branch only copied it
to a second page while documenting how to clean up a four-container pair.

## Completed

### Give the peg API a presence in `docs/`

**Completed:** v0.5.0

The peg API had shipped fully documented in rustdoc and with no presence in `docs/` at all — the
index contained zero occurrences of "peg". It now has a `Peg` section in `docs/reference-client.md`
covering `connect`, the three accessors, both peg-in paths, both peg-out calls, and the three peg
records; `PegPair` and `PegPairBuilder` sections in `docs/reference-fixtures.md`; the macro's new
`PegPair` parameter in `docs/reference-test-macro.md`; the how-to `docs/how-to-peg.md`; the
explanation `docs/explanation-what-the-peg-simulates.md`; and index entries in `docs/README.md`
routing to each.

Deferred to the `PegPair` work on purpose, and that was the right call: the how-to opens by starting
a wired pair, which was not something a reader could do when this entry was written.

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
