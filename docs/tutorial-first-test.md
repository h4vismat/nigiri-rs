# Tutorial: your first fixture-backed test

You'll write a Rust test that starts a private Bitcoin regtest chain in Docker, funds an address on
it, and checks the money arrived — then throws the whole chain away. No Nigiri installation, no
shared node, no cleanup code.

By the end you'll understand the two ways to reach a fixture, why ports must be read rather than
assumed, and how to do the same thing on Liquid.

## What you'll need

- **Rust 1.88 or newer** (edition 2024). Check with `rustc --version`.
- **Docker, running.** Check with `docker info`. Podman is untested.
- About 10 minutes, most of it the first image pull.

## Step 1: Make a crate

```sh
cargo new --lib faucet-demo
cd faucet-demo
```

## Step 2: Add the dependency

Open `Cargo.toml` and add:

```toml
[dev-dependencies]
nigiri-rs = { version = "0.4", features = ["testcontainers"] }
```

One dependency. `nigiri-rs` is a facade that re-exports the client, the fixtures, and the test
attribute, so you never name the three crates behind it.

`dev-dependencies` is the right section: fixtures are a testing tool, and this keeps the Docker
client libraries out of your release build.

The `testcontainers` feature is what turns on both the fixtures and the `#[nigiri_rs::test]`
attribute. Without it neither exists.

## Step 3: Write the test and run it

Create `tests/faucet.rs`:

```rust,ignore
use nigiri_rs::{Bitcoin, NigiriClient};

type BoxError = Box<dyn std::error::Error>;

#[nigiri_rs::test]
async fn a_fresh_chain_arrives_funded(client: NigiriClient<Bitcoin>) -> Result<(), BoxError> {
    let height = client.block_height().await?;
    println!("tip height: {height}");
    assert_eq!(height, 101);
    Ok(())
}
```

Run it:

```sh
cargo test --test faucet -- --nocapture
```

The first run pulls two container images and takes a while. After that:

```
running 1 test
tip height: 101
test a_fresh_chain_arrives_funded ... ok
```

**That's a working chain.** The attribute started a Bitcoin node and an Electrs indexer in Docker,
mined 101 blocks so the coinbase would mature, waited until node, Esplora, and Electrum all agreed on
the tip, and handed your function a client pointed at all of it. When the test ended, both
containers, their volumes, and their network were removed.

Height 101 is not arbitrary: Bitcoin's coinbase needs 100 confirmations to become spendable, so 101
blocks is the smallest chain with money you can actually move.

> **If it failed instead:** a message ending `is Docker running?` means exactly that — start Docker
> and re-run. Tests here are never `#[ignore]`d, so an unavailable Docker fails loudly rather than
> reporting green having verified nothing.

## Step 4: Send money and watch it arrive

Replace the test body:

```rust,ignore
#[nigiri_rs::test]
async fn a_faucet_payment_shows_up_as_a_utxo(client: NigiriClient<Bitcoin>) -> Result<(), BoxError> {
    let address = client.new_address().await?;
    let address = address.to_string();

    let txid = client.faucet(&address, None).await?;
    println!("funded {address} with {txid}");

    let utxos = client.get_utxos(&address).await?;
    assert_eq!(utxos.len(), 1);
    assert_eq!(utxos[0].value.to_sat(), 100_000_000);

    Ok(())
}
```

```sh
cargo test --test faucet -- --nocapture
```

`faucet` did two things: it sent through the node wallet, then mined exactly one block to confirm the
payment. That's why the UTXO is queryable immediately, with no waiting.

`None` means "send exactly 1 BTC" — 100,000,000 satoshis, which is what the assertion checks. Pass
`Some(Amount::from_sat(50_000))` for a specific amount; that needs `bitcoin = "0.32"` in your
dev-dependencies, since `Amount` is the `bitcoin` crate's type.

The value came back as a `bitcoin::Amount`, not a float. Nothing in this crate passes a monetary
value through `f64`.

## Step 5: Read the ports, don't assume them

Add a third test:

```rust,ignore
#[nigiri_rs::test]
async fn the_endpoints_are_runtime_mapped(client: NigiriClient<Bitcoin>) -> Result<(), BoxError> {
    let esplora = client.esplora_url();
    let electrum = client.electrum_endpoint();

    println!("esplora:  {esplora}");
    println!("electrum: {}:{}", electrum.host(), electrum.port());

    // Nigiri's fixed ports. A fixture never uses them on the host side.
    assert_ne!(electrum.port(), 50_000);
    assert_ne!(esplora.port(), Some(30_000));

    Ok(())
}
```

You'll see something like `electrum: localhost:32773` — a different port every run.

**This is the thing most likely to bite you.** Nigiri publishes fixed ports (30000 for Esplora, 50000
for Electrum on Bitcoin) and plenty of example code hardcodes them. A fixture cannot: it lets Docker
assign host ports so that two fixtures can run at once without colliding. Always read the endpoints
off the client.

Those two accessors are how you point a real BDK or LWK wallet at the chain. See
[How to point a wallet at a fixture](how-to-connect-a-wallet.md).

## Step 6: Two chains in one test

The chain comes from the parameter type, so asking for both is just a second parameter:

```rust,ignore
use nigiri_rs::{Bitcoin, Liquid, NigiriClient};

#[nigiri_rs::test]
async fn the_two_chains_are_independent(
    bitcoin: NigiriClient<Bitcoin>,
    liquid: NigiriClient<Liquid>,
) -> Result<(), BoxError> {
    assert_eq!(bitcoin.block_height().await?, 101);
    assert_eq!(liquid.block_height().await?, 1);

    assert_ne!(
        bitcoin.electrum_endpoint().port(),
        liquid.electrum_endpoint().port(),
    );
    Ok(())
}
```

Two separate stacks, four containers, two networks. They share nothing — a reorg on one is invisible
to the other.

The two fixtures **start concurrently**. Measured on this exact shape: 4.47 s together against 6.30 s
one after the other. There is no attribute argument for the chain, which means the attribute and the
signature can never disagree about it.

Liquid arrives at height 1, not 101. Liquid has no block subsidy, so mining funds nothing; the
fixture connects the genesis outputs instead, and mines a single block only because callers
reasonably expect a nonzero tip.

## Step 7: The same thing without the attribute

The attribute is a convenience over a plain API. When you need the fixture handle itself — to
override an image, or to control exactly when teardown happens — use it directly:

```rust,ignore
use nigiri_rs::testcontainers::{Bitcoin, Fixture};

#[tokio::test]
async fn manual_fixture() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::<Bitcoin>::start().await?;
    let client = fixture.client();

    assert_eq!(client.block_height().await?, 101);

    drop(fixture); // containers gone here
    Ok(())
}
```

This needs `tokio` in your dev-dependencies with the `macros` and `rt-multi-thread` features; the
attribute reached tokio through `nigiri-rs` for you.

**Keep the fixture alive as long as you use the client.** `client()` returns a borrow so the
compiler enforces it, but `NigiriClient` is `Clone` — a cloned client outliving its fixture points at
containers that no longer exist.

## What you built

A test suite where every test owns a private blockchain. No shared node to coordinate around, no
cleanup step to forget, no `#[ignore]` hiding a test that never ran. Tests can mine, reorg, and
mutate wallets in parallel, because none of them can see each other's chain.

Where to go next:

- [How to point a wallet at a fixture](how-to-connect-a-wallet.md) — the actual reason to want this.
- [How to call any node RPC](how-to-call-any-node-rpc.md) — for anything the curated API doesn't
  wrap.
- [`#[nigiri_rs::test]` reference](reference-test-macro.md) — `startup_timeout`, `flavor`, and every
  rejection.
- [Client API reference](reference-client.md) — the full method list.
- [What "ready" means](explanation-fixture-readiness.md) — why `start()` can hand you a chain that
  is already queryable through three different services.
