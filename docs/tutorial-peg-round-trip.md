# Tutorial: a round trip across Liquid's peg

You'll write tests that move value across Liquid's peg in both directions: BTC into the sidechain as
L-BTC, and L-BTC back out as BTC. Everything runs against four containers that exist only for the
duration of the test.

By the end you'll know why a peg needs a *wired* pair of nodes rather than two chains that happen to
be running, which half of the peg is real and which half this crate is pretending, and why a peg-in
claim sometimes has to be retried.

This tutorial assumes you've done [your first fixture-backed test](tutorial-first-test.md), or at
least know what a fixture is. If you just want the steps for a task you already understand, the
[how-to](how-to-peg.md) is shorter.

## What you'll need

- **Rust 1.88 or newer** (edition 2024). Check with `rustc --version`.
- **Docker, running.** Check with `docker info`.
- About 15 minutes. A peg pair is four containers, so the first run pulls four images.

## Step 1: Make a crate and add the dependencies

```sh
cargo new --lib peg-demo
cd peg-demo
```

Open `Cargo.toml` and add:

```toml
[dev-dependencies]
nigiri-rs = { version = "0.5", features = ["testcontainers"] }
bitcoin = "0.32"
serde_json = "1"
```

`nigiri-rs` is the facade — the client, the fixtures, and the test attribute. You need `bitcoin` for
`Amount`, because this crate never expresses money as a float. `serde_json` is for step 4, where
you'll ask a node a question the curated API doesn't wrap.

## Step 2: Start a wired pair

Create `tests/peg.rs`:

```rust,ignore
use nigiri_rs::testcontainers::PegPair;

type BoxError = Box<dyn std::error::Error>;

#[nigiri_rs::test(startup_timeout = 180)]
async fn a_pair_arrives_wired(pair: PegPair) -> Result<(), BoxError> {
    println!("bitcoin tip: {}", pair.bitcoin().block_height().await?);
    println!("liquid tip:  {}", pair.liquid().block_height().await?);
    println!("confirmation depth: {}", pair.peg().pegin_confirmation_depth());
    Ok(())
}
```

```sh
cargo test --test peg -- --nocapture
```

The first run pulls four images and takes a while. After that:

```
running 1 test
bitcoin tip: 101
liquid tip:  1
confirmation depth: 8
test a_pair_arrives_wired ... ok
```

**That's a peg pair.** Four containers on one Docker network: `bitcoind` with its Electrs, and
`elementsd` with its Electrs. The Elements node was started with `-validatepegin=1` and pointed at
the `bitcoind` beside it, which is the whole difference between a peg pair and two unrelated chains.

The heights differ for the reason they always do: Bitcoin needs 101 blocks before a coinbase is
spendable, while Liquid has no block subsidy at all and takes its money from the genesis outputs.

`startup_timeout = 180` raises the default 120-second budget, which is worth doing on a first run
that also has to pull images.

> **If it failed instead:** a message ending `is Docker running?` means exactly that. If it timed out
> while pulling, raise `startup_timeout` and run again — the images stay cached.

## Step 3: Peg in

Add a second test:

```rust,ignore
use bitcoin::Amount;

#[nigiri_rs::test(startup_timeout = 180)]
async fn btc_becomes_lbtc(pair: PegPair) -> Result<(), BoxError> {
    let pegged = pair.peg().complete_peg_in(Amount::from_sat(100_000)).await?;

    println!("deposit: {}", pegged.mainchain_txid);
    println!("claim:   {}", pegged.claim_txid);
    Ok(())
}
```

```sh
cargo test --test peg -- --nocapture
```

```
deposit: 6f1c…            (a Bitcoin txid)
claim:   a83b…            (a Liquid txid)
test btc_becomes_lbtc ... ok
```

**Two chains, one operation.** `complete_peg_in` asked the Liquid node for a peg-in address, sent BTC
to it on the Bitcoin side, mined until the deposit was old enough, then submitted a real `claimpegin`
carrying a real merkle proof. Nothing about the peg-in half is simulated: it is the same mechanism a
wallet integrates against on liquidv1.

It mines, which matters if you assert on block height afterwards. It mines to the depth the Liquid
node reports — the 8 you printed in step 2 — and then keeps mining one block at a time until the
claim is accepted, up to 20 more.

That last part is not defensive coding. The node **rejects a claim at exactly the depth it itself
reports**, because its view of the mainchain lags the mainchain. Two runs of this crate's own test
suite disagreed on how far past: one was accepted two blocks later, another three. That disagreement
is why the code retries rather than adding a fixed margin.

## Step 4: Prove the money actually arrived

Here is the trap. Add this to `btc_becomes_lbtc` and run it:

```rust,ignore
    assert_eq!(pegged.amount, Amount::from_sat(100_000));
```

It passes. It would also pass if the peg-in minted nothing at all, because `complete_peg_in` returns
the `amount` you handed it, echoed back unchanged. The assertion compares an argument to itself.

Ask the Liquid node instead:

```rust,ignore
#[nigiri_rs::test(startup_timeout = 180)]
async fn the_claim_credits_the_liquid_wallet(pair: PegPair) -> Result<(), BoxError> {
    let pegged = pair.peg().complete_peg_in(Amount::from_sat(100_000)).await?;

    let received: serde_json::Value = pair
        .liquid()
        .rpc("gettransaction", (pegged.claim_txid.to_string(),))
        .await?;

    // Elements is multi-asset, so `amount` is keyed per asset rather than being one number.
    // L-BTC's label is "bitcoin".
    let serde_json::Value::Number(credited) = &received["amount"]["bitcoin"] else {
        panic!("a credited claim reports a numeric bitcoin amount: {received}");
    };
    let credited = Amount::from_str_in(&credited.to_string(), bitcoin::Denomination::Bitcoin)?;

    println!("credited: {credited}");
    assert!(credited > Amount::ZERO);
    Ok(())
}
```

```
credited: 0.00099971 BTC
test the_claim_credits_the_liquid_wallet ... ok
```

**Less than 100,000 sat, and that is correct.** The peg is 1:1 minus network fees — the claim
transaction paid a fee on the Liquid side. Assert a floor or a range, never an exact figure.

Two things worth keeping from this step. `rpc` is the escape hatch for anything the curated API
doesn't wrap, and it's typed — see [how to call any node RPC](how-to-call-any-node-rpc.md). And the
amount goes through `serde_json::Number` and `Amount::from_str_in` rather than `as_f64`, so the
value stays exact; the crate parses peg-out values the same way internally.

## Step 5: Peg out

```rust,ignore
#[nigiri_rs::test(startup_timeout = 180)]
async fn lbtc_becomes_btc(pair: PegPair) -> Result<(), BoxError> {
    let destination = pair.bitcoin().new_address().await?;

    let burn = pair
        .peg()
        .send_to_mainchain(&destination.to_string(), Amount::from_sat(10_000))
        .await?;
    println!("burned in: {burn}");

    let released = pair.peg().release_peg_out(&burn).await?;
    println!("released {} to {}", released.amount, released.destination);

    assert_eq!(released.destination, destination);
    assert_eq!(released.amount, Amount::from_sat(10_000));
    Ok(())
}
```

```
burned in: 2832…
released 0.00010000 BTC to bcrt1q6pkdnagrcc38swf7jrtukw0jynrn7w04lxhnhn
test lbtc_becomes_btc ... ok
```

Two calls, and they are not the same kind of thing.

`send_to_mainchain` is a genuine Elements RPC. It really burns the L-BTC and really encodes your
Bitcoin destination into the transaction. That half is as real as the peg-in.

`release_peg_out` is this crate playing federation. On liquidv1 a group of functionaries watches for
burns and releases the BTC from locked funds; regtest has no functionaries, so nothing would ever
happen. **The BTC it pays comes from the Bitcoin node's own wallet, not from a reserve.** Total BTC
on the mainchain side grows with every release, and no 1:1 invariant holds across the pair. If you
write a test asserting conservation, it will read the wrong number and you will chase it for an hour.

Notice what `release_peg_out` was *not* given: the destination. It read that out of the transaction.

## Step 6: Break it on purpose

That last point is the reason peg-out is worth testing at all, so make it fail. Add:

```rust,ignore
use nigiri_rs::NigiriError;

#[nigiri_rs::test(startup_timeout = 180)]
async fn an_ordinary_transfer_is_not_a_peg_out(pair: PegPair) -> Result<(), BoxError> {
    let elsewhere = pair.liquid().new_address().await?;
    let ordinary = pair
        .liquid()
        .faucet(&elsewhere.to_string(), Some(Amount::from_sat(50_000)))
        .await?;

    let error = pair
        .peg()
        .release_peg_out(&ordinary)
        .await
        .expect_err("an ordinary transfer carries no peg-out output");

    println!("{error}");
    assert!(matches!(error, NigiriError::PegOutputNotFound { .. }));
    Ok(())
}
```

```
no peg-out output in Liquid transaction abe282317dece75c…
test an_ordinary_transfer_is_not_a_peg_out ... ok
```

**This is the failure your wallet needs to survive.** A peg-out is an ordinary-looking Liquid
transaction with a specially shaped output. Build that output wrongly and there is nothing to decode,
so no BTC is released and the L-BTC is already burned. On liquidv1 that is how funds are lost
silently. Here it is a typed error in a test that takes eight seconds.

If the output is present but its destination is unreadable you get `PegOutputMalformed` instead,
carrying which transaction and what was wrong with it.

## Step 7: Know what you are trusting

You now have a suite exercising a real peg-in and a simulated peg-out. Before you build assertions on
top of it, the two limits that matter:

- **The release holds no reserve.** L-BTC supply falls honestly when you burn, but the BTC that comes
  back was freshly mined, not unlocked. Conservation across the pair is not a property you can test.
- **`Peg::connect` does not prove the nodes can talk.** It compares the Liquid node's parent chain
  against the Bitcoin node's genesis — and Bitcoin's regtest genesis is a hardcoded constant that
  `liquidregtest` carries as a chain parameter, so two fixtures that have never heard of each other
  agree on it too. It catches a Liquid node built for a *different* parent chain. `PegPair` is what
  guarantees wiring, because it does the wiring.

Both are covered properly in [what the peg simulates](explanation-what-the-peg-simulates.md).

## What you built

A test suite where a wallet's peg-in path is exercised against a real `claimpegin` — merkle proof,
confirmation depth, lagging node view and all — and its peg-out path is exercised against a
federation that behaves like the real one in the way that matters: it reads the destination out of
your transaction, so encoding it wrongly pays nobody.

Four containers per test, removed when the test ends. No shared node, no cleanup step.

Where to go next:

- [How to peg in and peg out](how-to-peg.md) — the same operations as task recipes, including driving
  the claim yourself with `peg_in_request` and `claim_peg_in`.
- [What the peg simulates](explanation-what-the-peg-simulates.md) — the full real-versus-pretend
  boundary, and why each line of it falls where it does.
- [Client API reference](reference-client.md) — every `Peg` method, with the records they return.
- [Fixture API reference](reference-fixtures.md) — `PegPair`, its builder, and what a started pair
  guarantees.
- [How to point a wallet at a fixture](how-to-connect-a-wallet.md) — pointing a real BDK or LWK
  wallet at either chain of the pair.
