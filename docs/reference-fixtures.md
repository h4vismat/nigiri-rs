# Fixture API reference

`nigiri-rs-testcontainers`, reached through the facade as `nigiri_rs::testcontainers` with the
`testcontainers` feature enabled.

A fixture is one throwaway regtest stack: a node with a funded wallet, an Electrs indexer following
it, and a `NigiriClient` pointed at both. Nothing is shared between fixtures, so tests can run in
parallel and mine or reorg freely without coordinating.

Requires a running Docker daemon. No Nigiri installation. Podman is untested.

## `Fixture<C>`

```rust
pub struct Fixture<C: FixtureChain> { /* private */ }
```

Implements `Debug` by hand (chain name and Electrum endpoint only — the held client's config carries
the RPC password). Not `Clone`.

| Method | Signature |
| --- | --- |
| `start` | `async fn start() -> Result<Fixture<C>, FixtureError>` |
| `builder` | `fn builder() -> FixtureBuilder<C>` |
| `client` | `fn client(&self) -> &NigiriClient<C>` |
| `electrum_endpoint` | `fn electrum_endpoint(&self) -> &ElectrumEndpoint` |

```rust
use nigiri_rs::testcontainers::{Bitcoin, Fixture};

# async fn example() -> Result<(), nigiri_rs::testcontainers::FixtureError> {
let fixture = Fixture::<Bitcoin>::start().await?;
let client = fixture.client();
# let _ = client;
# Ok(())
# }
```

`start()` is `builder().start()` with the pinned defaults. `electrum_endpoint()` delegates to the
client; it is a convenience, not a second source of truth.

### Lifetime and teardown

**The fixture owns the containers.** Dropping it removes both containers, their anonymous volumes,
and the network. Nothing survives the test.

How many anonymous volumes exist depends on the images, not on the fixture: Docker creates one per
`VOLUME` an image declares, and of the pinned four only `docker-bitcoind` declares any. A Bitcoin
fixture therefore owns one and a Liquid fixture owns none — either way a fixture never mounts
storage Docker did not create for that container alone.

Keep the `Fixture` alive for as long as you use the client. `client()` returns a borrow, so the
compiler enforces this — but note that `NigiriClient` is `Clone`, and a cloned client outliving its
fixture points at containers that no longer exist.

Field order inside the struct is deliberate: Electrs is dropped before the node it indexes, so the
indexer is never briefly pointed at a container that has already gone. A test in `fixture.rs` pins
that order.

Teardown also runs on the panic path. A failed start inside `#[nigiri_rs::test]` unwinds with the
other fixtures' handles dropping normally.

### What a started fixture guarantees

When `start()` returns:

- The node, Esplora, and Electrum all report the **same tip height**.
- The node wallet holds spendable funds.
- Every port is the runtime-mapped one, readable from the client.

Tip heights on arrival:

| Chain | Height | Why |
| --- | --- | --- |
| Bitcoin | 101 | Mines 101 blocks so the coinbase matures. |
| Liquid | 1 | No block subsidy; funds come from connecting the genesis outputs. The single block exists only because callers expect a nonzero tip. |

That three-way agreement is established **once, at startup**. Blocks mined afterwards reach the
indexer on its own schedule — poll for them. See
[What "ready" means](explanation-fixture-readiness.md).

### A Liquid fixture is not a running Nigiri Liquid node

A Liquid fixture connects its genesis outputs, so its wallet holds the full 21,000,000 L-BTC of free
coins. Nigiri does not, and its wallet reports a zero L-BTC balance. The chain is identical — same
genesis, same dynamic-federation parameters — but a fixture is funded and Nigiri's node is not.

### Startup cost

Measured on an idle machine with the images already pulled:

| Scenario | Time |
| --- | --- |
| One Bitcoin fixture | ~3 s |
| One Liquid fixture | ~1.5 s |
| Two Bitcoin fixtures at once | ~4.5 s |
| Two of each at once | ~5 s |

The default 60-second budget covers all of these with room to spare. **The first run on a machine is
the slow one**: it pulls two pinned images per chain. Raise `startup_timeout` for that.

## `FixtureBuilder<C>`

```rust
pub struct FixtureBuilder<C: FixtureChain> { /* private */ }
```

Derives `Clone` and `Debug`. Every setter takes and returns `self`.

| Method | Signature | Default |
| --- | --- | --- |
| `startup_timeout` | `fn startup_timeout(self, timeout: Duration) -> Self` | 60 s |
| `node_image` | `fn node_image(self, image: ContainerImage) -> Self` | `C::node_image_default()` |
| `electrs_image` | `fn electrs_image(self, image: ContainerImage) -> Self` | `C::electrs_image_default()` |
| `start` | `async fn start(self) -> Result<Fixture<C>, FixtureError>` | — |

```rust
use std::time::Duration;
use nigiri_rs::testcontainers::{Bitcoin, Fixture};

# async fn example() -> Result<(), nigiri_rs::testcontainers::FixtureError> {
let fixture = Fixture::<Bitcoin>::builder()
    .startup_timeout(Duration::from_secs(180))
    .start()
    .await?;
# let _ = fixture;
# Ok(())
# }
```

`startup_timeout` bounds **the whole startup**, not any single step within it. One shared deadline
covers everything after validation, so a slow phase spends budget the later phases then no longer
have. This is intentional: a per-phase clock would let a fixture take an unbounded total.

### What `start` does, in order

1. Validates both image descriptors. Invalid input is rejected **before Docker is asked to start
   anything**.
2. Generates UUID-scoped resource names (see [Resource naming](#resource-naming)).
3. Creates the shared deadline. A zero timeout is rejected here.
4. Starts the node container, waits for its RPC, creates the wallet, and funds it.
5. Starts Electrs pointed at the node container by name.
6. Applies the Esplora URL and Electrum endpoint Electrs just published to a copy of the
   wallet-scoped client config, and builds the client.
7. Waits until node, Esplora, and Electrum agree on the tip.

Steps 5 and 7 attach the relevant container's log to the error on failure, which is what makes a
readiness timeout diagnosable rather than a bare timeout.

### Resource naming

Every Docker resource of one fixture is scoped to a single UUID, so concurrent fixtures cannot
collide and a leaked resource is traceable to the fixture that made it.

| Resource | Name |
| --- | --- |
| Network | `nigiri-rs-fixture-<uuid>` |
| Bitcoin node | `nigiri-rs-bitcoind-<uuid>` |
| Liquid node | `nigiri-rs-elements-<uuid>` |
| Indexer | `nigiri-rs-electrs-<uuid>` |

The UUID suffix is shared across all four. If you ever need to clean up after a hard kill:
`docker rm -f -v` on anything matching those prefixes — the `-v` matters, it takes the anonymous
volumes with it.

## `ContainerImage`

```rust
pub struct ContainerImage { /* private */ }

impl ContainerImage {
    pub fn new(name: impl Into<String>, tag: impl Into<String>) -> Self;
    pub fn with_digest(self, digest: impl Into<String>) -> Self;
    pub fn with_entrypoint(self, entrypoint: impl Into<String>) -> Self;
    pub fn name(&self) -> &str;
    pub fn tag(&self) -> &str;
    pub fn digest(&self) -> Option<&str>;
    pub fn entrypoint(&self) -> Option<&str>;
}
```

Derives `Clone`, `Debug`, `Eq`, `PartialEq`. Both the digest and the entrypoint are optional; `new`
alone is valid.

Without `with_entrypoint`, the image's own `ENTRYPOINT` is used, which is what most images want:
they already start their daemon, and overriding it would exec that daemon twice. Set one only for an
image that does not — `blockstream/elementsd` declares no entrypoint and defaults to `bash`, so
without `with_entrypoint("elementsd")` the flag vector a chain builds would be execed as a program
name.

Validation, applied at `start()`:

- Name must not be empty.
- Tag must not be empty.
- A digest, if present, must be `sha256:` followed by exactly 64 lowercase hex characters.
- An entrypoint, if present, must not be blank.

Each failure is `FixtureError::InvalidConfiguration`.

### Pinned defaults

Pinned by both tag and digest.

| Role | Image | Tag | Entrypoint |
| --- | --- | --- | --- |
| Bitcoin node | `ghcr.io/getumbrel/docker-bitcoind` | `v31.0` | the image's own |
| Liquid node | `blockstream/elementsd` | `23.3.3` | `elementsd` |
| Bitcoin indexer | `mempool/electrs` | `v3.4.0-dev1` | the image's own |
| Liquid indexer | `mempool/electrs-liquid` | `v3.4.0-dev1` | the image's own |

The indexers are Mempool's Esplora-Electrs fork, not the one Nigiri runs, which has not been rebuilt
since 2022. Both are pinned to the same `v3.4.0-dev1` build: the Liquid variant publishes no stable
tag, and a version skew between the two would show up as a chain difference in a suite that asserts
the same behaviour against both.

The Liquid node is Blockstream's own image, built from the verified `ElementsProject/elements`
release binaries. That is a provenance choice rather than a version bump: Nigiri's image runs Elements
Core v23.3.3 as well, so the `liquidregtest` chain is unchanged, which the Liquid suite asserts
against a genesis hash read from a real Nigiri stack.

The Bitcoin node is one major version ahead of the Nigiri CLI, which runs Core v30.0.

The constructors for these are crate-private; you reach them by not overriding the builder. You can
replace any of them with `ContainerImage::new(...)`, but **an image this crate has not been tested
against may not honour the same arguments** — the node and indexer command lines are built for
these specific images.

## `FixtureChain`

```rust
pub trait FixtureChain: NigiriNetwork + Sized + private::Sealed + 'static {
    const NODE_SERVICE: &'static str;
    const CHAIN_NAME: &'static str;
    const NODE_RPC_PORT: u16;
    const ELECTRS_HTTP_PORT: u16;
    const ELECTRS_ELECTRUM_PORT: u16;
    const NODE_NAME_PREFIX: &'static str;

    fn node_image_default() -> ContainerImage;
    fn electrs_image_default() -> ContainerImage;
    fn node_cmd() -> Vec<String>;
    fn electrs_cmd(node_container: &str) -> Vec<String>;
    // plus one sealed funding method
}
```

**Sealed.** Implemented for `Bitcoin` and `Liquid` only. A downstream implementation could not uphold
the container lifecycle, diagnostics, and teardown guarantees this crate makes, so it is not allowed
to try.

| Constant | `Bitcoin` | `Liquid` |
| --- | --- | --- |
| `NODE_SERVICE` | `bitcoind` | `elements` |
| `CHAIN_NAME` | `Bitcoin` | `Liquid` |
| `NODE_RPC_PORT` | 18443 | 18884 |
| `ELECTRS_HTTP_PORT` | 30000 | 30001 |
| `ELECTRS_ELECTRUM_PORT` | 50000 | 50001 |
| `NODE_NAME_PREFIX` | `nigiri-rs-bitcoind` | `nigiri-rs-elements` |

Every port above is a *container* port. The host-side ports are assigned by the runtime and differ
every run, which is why you must read the endpoint from the client rather than hardcoding these.

The funding step is the one thing that genuinely differs between chains. Everything before it —
container, root RPC, `createwallet`, the wallet-scoped client — is shared. Bitcoin mines a coinbase
and waits for maturity; Liquid has no block subsidy and reaches the genesis outputs instead.

## Re-exports and constants

```rust
pub use nigiri_rs_core::{Bitcoin, ElectrumEndpoint, Liquid};
pub const REPRODUCED_NIGIRI_VERSION: &str = "nigiri-v0.5.16";
```

`REPRODUCED_NIGIRI_VERSION` records the Nigiri release whose topology the pinned images reproduce, so
a future divergence can be traced to a version rather than guessed at. It is documentation, not a
runtime check — nothing in this crate talks to Nigiri.

Note the crate does **not** re-export `NigiriClient`. Import it from `nigiri_rs` or
`nigiri_rs_core`.

## Errors

Every fallible method returns `FixtureError`. See [Errors](reference-errors.md) for the full variant
list and what triggers each.

## Related

- [How to run a throwaway regtest stack](how-to-run-a-fixture.md)
- [`#[nigiri_rs::test]`](reference-test-macro.md) — the same fixtures without the preamble
- [What "ready" means](explanation-fixture-readiness.md)
- [Lifecycle ownership](explanation-lifecycle-ownership.md)
