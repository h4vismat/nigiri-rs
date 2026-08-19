# How to run a throwaway regtest stack

Start and use a Bitcoin, Liquid, or ready-to-pay Lightning regtest topology in Docker. Drop requests
best-effort cleanup; use `shutdown().await` when cleanup errors matter, and remember that a hard kill
can leave resources.

Use this when you need the fixture handle itself. If you only need a ready client in a test,
[`#[nigiri_rs::test]`](reference-test-macro.md) does the same thing with less code.

## Prerequisites

- Docker running (`docker info` succeeds).
- The `fixtures` feature enabled:

  ```toml
  [dev-dependencies]
  nigiri-rs = { version = "0.5", features = ["fixtures"] }
  tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
  ```

## Start one

```rust,ignore
use nigiri_rs::fixtures::{Bitcoin, Fixture};

#[tokio::test]
async fn uses_a_chain() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::<Bitcoin>::start().await?;
    let client = fixture.client();

    assert_eq!(client.block_height().await?, 101);
    Ok(())
}
```

Swap the type parameter for Liquid — everything else is identical:

```rust,ignore
use nigiri_rs::fixtures::{Fixture, Liquid};

let fixture = Fixture::<Liquid>::start().await?;
assert_eq!(fixture.client().block_height().await?, 1);
```

Liquid arrives at height 1 rather than 101. It has no block subsidy, so the fixture connects the
genesis outputs to fund the wallet instead of mining for it.

For a ready-to-pay Lightning topology, start `LndPair` instead:

```rust,no_run
use nigiri_rs::fixtures::LndPair;

# async fn example() -> Result<(), nigiri_rs::fixtures::FixtureError> {
let pair = LndPair::start().await?;
let _alice = pair.alice();
let _bob = pair.bob();
let _funding_output = pair.channel_point();
# Ok(())
# }
```

That owns four containers: bitcoind, Electrs, LND Alice, and LND Bob. It returns only after the
channel funding output has six confirmations, both nodes see the same active channel with spendable
local balance, and public 1,000-msat invoices settle in both directions. The two startup invoices
and payments remain in history.

### Verification

`block_height()` returning 101 (Bitcoin) or 1 (Liquid) means the whole stack is up: the node mined,
the indexer caught up, and all three services agree on the tip.

## Let it go

Drop starts best-effort cleanup automatically. Use explicit shutdown when its result matters:

```rust,ignore
let fixture = Fixture::<Bitcoin>::start().await?;
// ... use it ...
fixture.shutdown().await?; // waits for both containers, their volumes, and the network
```

Dropping at the end of scope is the normal best-effort case. Use `shutdown().await` when the test
must observe cleanup errors. `LndPair::shutdown()` attempts both phases even if LND cleanup fails:
Bob, Alice, Electrs, bitcoind, then the network.

**Keep the fixture alive for as long as you use the client.** `client()` returns a borrow, so the
compiler stops you holding it too long — but `NigiriClient` is `Clone`, and a cloned client that
outlives its fixture may point at containers that cleanup has removed. You may see
connection-refused errors, not a compile error.

## Give it a longer budget

The default is 60 seconds for the whole startup. That covers every warm start with room to spare.
The **first** run on a machine has to pull two images, which can blow through it (a
[`PegPair`](reference-fixtures.md#pegpair) pulls four and defaults to 120 seconds for the same
reason):

```rust,ignore
use std::time::Duration;
use nigiri_rs::fixtures::{Bitcoin, Fixture};

let fixture = Fixture::<Bitcoin>::builder()
    .startup_timeout(Duration::from_secs(300))
    .start()
    .await?;
```

The timeout bounds the **whole** startup, not each step. One shared deadline covers container start,
wallet funding, indexer start, and the readiness wait, so a slow phase spends budget the later phases
then no longer have.

`LndPair` defaults to 180 seconds. Its deadline also covers both LND starts, transient `GenSeed`
retry, wallet initialization, funding, six confirmation blocks, post-channel graph readiness, two
payment probes, failure diagnostics, and a bounded wait for cleanup. If that wait exhausts the
remaining time, the public call returns while its dedicated supervisor continues reverse-order
cleanup under per-request Docker bounds.

Pre-pulling is the alternative, and it keeps your timeouts honest:

```sh
docker pull ghcr.io/getumbrel/docker-bitcoind:v31.0
docker pull mempool/electrs:v3.4.0-dev1
docker pull blockstream/elementsd:23.3.3
docker pull mempool/electrs-liquid:v3.4.0-dev1
docker pull lightninglabs/lnd:v0.21.1-beta
```

## Run several at once

Fixtures share nothing, so this needs no coordination:

```rust,ignore
use nigiri_rs::fixtures::{Bitcoin, Fixture, Liquid};

let (bitcoin, liquid) = tokio::join!(
    Fixture::<Bitcoin>::start(),
    Fixture::<Liquid>::start(),
);
let bitcoin = bitcoin?;
let liquid = liquid?;
```

Starting a stack is almost entirely waiting on Docker and on the indexer catching up, so overlapping
the waits is close to free: 4.47 s for this pair together against 6.30 s sequentially.

Every Docker resource is scoped to a per-fixture UUID (`nigiri-rs-fixture-<uuid>`,
`nigiri-rs-bitcoind-<uuid>`, `nigiri-rs-elements-<uuid>`, `nigiri-rs-electrs-<uuid>`,
`nigiri-rs-lnd-alice-<uuid>`, and `nigiri-rs-lnd-bob-<uuid>`), so concurrent fixtures cannot collide
on a name.

Because each fixture owns its chain, your tests need no cross-process mutation lock. A reorg in one
is invisible to every other.

## Swap a container image

```rust,ignore
use nigiri_rs::fixtures::{Bitcoin, ContainerImage, Fixture};

let fixture = Fixture::<Bitcoin>::builder()
    .node_image(
        ContainerImage::new("ghcr.io/getumbrel/docker-bitcoind", "v29.0")
            .with_digest("sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
    )
    .start()
    .await?;
```

The digest is optional — `ContainerImage::new(name, tag)` alone is valid. If you supply one it must
be `sha256:` plus exactly 64 lowercase hex characters, or the fixture is rejected before Docker is
touched.

**A [`PegPair`](reference-fixtures.md#pegpairbuilder) has four images, so it has four setters**:
`bitcoind_image`, `bitcoin_electrs_image`, `elements_image`, and `liquid_electrs_image`. All four are
validated before the first container starts, so a bad Elements image is rejected without leaving the
Bitcoin half running.

**An [`LndPair`](reference-fixtures.md#lndpairbuilder) also has four image setters**:
`bitcoind_image`, `bitcoin_electrs_image`, `alice_image`, and `bob_image`. The LND defaults use the
same `v0.21.1-beta` baseline as the checked-in protobufs and are pinned by digest. Its allocation
setters are `channel_capacity` (2,000,000 sats) and `push_amount` (1,000,000 sats). The push must be
nonzero and lower than capacity, and each nominal side must retain at least 100,000 sats; all
allocation arithmetic and image descriptors are validated before Docker starts.

**If your image does not start its daemon on its own, give it an entrypoint.** The fixture passes a
flag vector as the container command and otherwise leaves the entrypoint to the image, so an image
whose `ENTRYPOINT` is unset (or is a shell) needs one:

```rust,ignore
use nigiri_rs::fixtures::{ContainerImage, Fixture, Liquid};

let fixture = Fixture::<Liquid>::builder()
    .node_image(
        ContainerImage::new("blockstream/elementsd", "23.3.3").with_entrypoint("elementsd"),
    )
    .start()
    .await?;
```

Without it the container execs `-chain=liquidregtest` as a program name and never answers RPC.

**An image this crate has not been tested against may not honour the same arguments.** The node and
indexer command lines are built for the pinned images; a different upstream can reject a flag and the
container will fail to start. The defaults are pinned by tag *and* digest for this reason.

## Troubleshooting

**`container runtime connect to container engine failed for container engine: ...`** — Docker isn't
running, or your user can't reach the socket. A failed daemon ping uses `ping container engine` in
the operation field instead. The underlying cause is in `Error::source()`; print the full chain:

```rust,ignore
let mut source: Option<&dyn std::error::Error> = Some(&error);
while let Some(current) = source {
    eprintln!("{current}");
    source = current.source();
}
```

**`fixture was not ready after 60s: node=101 esplora=4 electrum=4; ...`** — the startup budget
expired with the three services still disagreeing. The height triple names the laggard, and bounded
container logs follow it. If this is a first run, it's the image pull: raise `startup_timeout` or
pre-pull.

**`container runtime <operation> failed for <resource>: ...`** — Docker image, network, container,
port discovery, bounded file read, log retrieval, or cleanup failed. The operation/resource fields,
bounded redacted diagnostics, and `Error::source()` distinguish an unavailable daemon from an image
that started incorrectly.

**`invalid fixture configuration: ...`** — an image descriptor, startup budget, or LND allocation
failed validation. Rejected before Docker is asked to do anything.

**Containers left behind after a hard kill.** Drop requests and joins best-effort teardown, including
while panicking, but cannot report cleanup errors; use `shutdown().await` when they matter. A
`SIGKILL` skips Drop and can leave resources. Everything is prefixed and UUID-scoped, so:

```sh
docker ps -a --filter "name=nigiri-rs-" --format "{{.Names}}"
docker ps -aq --filter "name=nigiri-rs-" | while IFS= read -r id; do
    [ -n "$id" ] && docker rm -f -v "$id"
done
docker network ls -q --filter "name=nigiri-rs-fixture-" | while IFS= read -r id; do
    [ -n "$id" ] && docker network rm "$id"
done
```

The `-v` matters — without it the anonymous volumes stay. And `docker rm` never removes a network, so
the last line is not optional: a killed run leaves its network behind even after every container of it
is gone.

## Related

- [Fixture API reference](reference-fixtures.md) — every method, every default
- [`#[nigiri_rs::test]`](reference-test-macro.md) — the same fixtures without the preamble
- [What "ready" means](explanation-fixture-readiness.md)
- [Errors](reference-errors.md)
