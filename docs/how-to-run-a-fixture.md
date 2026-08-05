# How to run a throwaway regtest stack

Start a Bitcoin or Liquid regtest chain in Docker, use it, and have it removed when you're done.

Use this when you need the fixture handle itself. If you only need a ready client in a test,
[`#[nigiri_rs::test]`](reference-test-macro.md) does the same thing with less code.

## Prerequisites

- Docker running (`docker info` succeeds).
- The `testcontainers` feature enabled:

  ```toml
  [dev-dependencies]
  nigiri-rs = { version = "0.4", features = ["testcontainers"] }
  tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
  ```

## Start one

```rust,ignore
use nigiri_rs::testcontainers::{Bitcoin, Fixture};

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
use nigiri_rs::testcontainers::{Fixture, Liquid};

let fixture = Fixture::<Liquid>::start().await?;
assert_eq!(fixture.client().block_height().await?, 1);
```

Liquid arrives at height 1 rather than 101. It has no block subsidy, so the fixture connects the
genesis outputs to fund the wallet instead of mining for it.

### Verification

`block_height()` returning 101 (Bitcoin) or 1 (Liquid) means the whole stack is up: the node mined,
the indexer caught up, and all three services agree on the tip.

## Let it go

Teardown is `Drop`. There is no `close()` and no cleanup step to forget:

```rust,ignore
let fixture = Fixture::<Bitcoin>::start().await?;
// ... use it ...
drop(fixture);   // both containers, their volumes, and the network are gone
```

Dropping at the end of scope is the normal case; call `drop` explicitly only when you want teardown
to happen at a specific point.

**Keep the fixture alive for as long as you use the client.** `client()` returns a borrow, so the
compiler stops you holding it too long — but `NigiriClient` is `Clone`, and a cloned client that
outlives its fixture points at containers that no longer exist. You'll see connection-refused
errors, not a compile error.

## Give it a longer budget

The default is 60 seconds for the whole startup. That covers every warm start with room to spare.
The **first** run on a machine has to pull two images, which can blow through it:

```rust,ignore
use std::time::Duration;
use nigiri_rs::testcontainers::{Bitcoin, Fixture};

let fixture = Fixture::<Bitcoin>::builder()
    .startup_timeout(Duration::from_secs(300))
    .start()
    .await?;
```

The timeout bounds the **whole** startup, not each step. One shared deadline covers container start,
wallet funding, indexer start, and the readiness wait, so a slow phase spends budget the later phases
then no longer have.

Pre-pulling is the alternative, and it keeps your timeouts honest:

```sh
docker pull ghcr.io/getumbrel/docker-bitcoind:v30.0
docker pull ghcr.io/vulpemventures/electrs:latest
docker pull ghcr.io/vulpemventures/elements:latest
docker pull ghcr.io/vulpemventures/electrs-liquid:latest
```

## Run several at once

Fixtures share nothing, so this needs no coordination:

```rust,ignore
use nigiri_rs::testcontainers::{Bitcoin, Fixture, Liquid};

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
`nigiri-rs-bitcoind-<uuid>`, `nigiri-rs-elements-<uuid>`, `nigiri-rs-electrs-<uuid>`), so concurrent
fixtures cannot collide on a name.

Because each fixture owns its chain, your tests need no cross-process mutation lock. A reorg in one
is invisible to every other.

## Swap a container image

```rust,ignore
use nigiri_rs::testcontainers::{Bitcoin, ContainerImage, Fixture};

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

**An image this crate has not been tested against may not honour the same arguments.** The node and
indexer command lines are built for the pinned images; a different upstream can reject a flag and the
container will fail to start. The defaults are pinned by tag *and* digest for this reason.

## Troubleshooting

**`container runtime is unavailable`** — Docker isn't running, or your user can't reach the socket.
The `Display` text is deliberately short; the real cause is in `Error::source()`. Print the full
chain:

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

**`failed to start <service> from <image>: ...`** — the container was created but did not come up.
The `image` field carries the full descriptor including digest, which is what you want if a pinned
image has been replaced. Container logs are in `diagnostics`.

**`invalid fixture configuration: ...`** — an empty image name or tag, a malformed digest, or a zero
`startup_timeout`. Rejected before Docker is asked to do anything.

**Containers left behind after a hard kill.** Teardown runs on `Drop`, including while panicking, but
a `SIGKILL` skips it. Everything is prefixed and UUID-scoped, so:

```sh
docker ps -a --filter "name=nigiri-rs-" --format "{{.Names}}"
docker rm -f -v $(docker ps -aq --filter "name=nigiri-rs-")
```

The `-v` matters — without it the anonymous volumes stay.

## Related

- [Fixture API reference](reference-fixtures.md) — every method, every default
- [`#[nigiri_rs::test]`](reference-test-macro.md) — the same fixtures without the preamble
- [What "ready" means](explanation-fixture-readiness.md)
- [Errors](reference-errors.md)
