# How to peg in and peg out

Move value from Bitcoin into Liquid and back, against a pair of nodes wired for Liquid's peg.

Peg-in here is genuine: a real federation-controlled address, a real `claimpegin` with a real merkle
proof. The peg-out **release** is not — regtest has no functionaries, so this crate plays that part.
Read [What the peg simulates](explanation-what-the-peg-simulates.md) before you assert anything about
supply; it changes what the numbers mean.

## Prerequisites

```toml
[dev-dependencies]
nigiri-rs = { version = "0.5", features = ["testcontainers"] }
bitcoin = "0.32"     # for Amount
elements = "0.25"    # for Txid
serde_json = "1"     # for the raw-RPC assertions below
```

A running Docker daemon. A pair is four containers, and the first run on a machine pulls four pinned
images.

## Get a wired pair

`PegPair` starts `bitcoind` with its Electrs and `elementsd` with its Electrs on **one** Docker
network, gives the Elements node `-validatepegin=1` plus `-mainchainrpc*` pointed at that `bitcoind`
by container name, and hands back a `Peg` across the two:

```rust,ignore
use nigiri_rs::testcontainers::PegPair;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let pair = PegPair::start().await?;

let peg = pair.peg();          // &Peg — every pegging method lives here
let bitcoin = pair.bitcoin();  // &NigiriClient<Bitcoin>
let liquid = pair.liquid();    // &NigiriClient<Liquid>
# let _ = (peg, bitcoin, liquid);
# Ok(())
# }
```

Signature: `PegPair::start() -> Result<PegPair, FixtureError>`. `PegPair::builder()` overrides the
four images and the 120-second startup budget; see the
[fixture reference](reference-fixtures.md#pegpairbuilder).

Dropping the pair removes all four containers, their anonymous volumes, and the shared network. Keep
the `PegPair` alive, not the borrows — `bitcoin()`, `liquid()`, and `peg()` all borrow from it.

Or let the macro own it. A `PegPair` parameter binds the pair itself, not a clone of a client:

```rust,ignore
use bitcoin::Amount;
use nigiri_rs::testcontainers::PegPair;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[nigiri_rs::test(startup_timeout = 180)]
async fn a_deposit_becomes_lbtc(pair: PegPair) -> Result<(), BoxError> {
    let pegged = pair.peg().complete_peg_in(Amount::from_sat(100_000)).await?;
    println!("minted by {}", pegged.claim_txid);

    // `>= 101` is a floor a pair that pegged nothing already clears — 101 is also the arrival
    // height, so this is not evidence that `complete_peg_in` mined anything.
    assert!(pair.bitcoin().block_height().await? >= 101);
    assert_eq!(pair.liquid().block_height().await?, 1);
    Ok(())
}
```

Without a `startup_timeout` argument the parameter gets `PegPair::start()`'s own 120 seconds. See
[`#[nigiri_rs::test]`](reference-test-macro.md#a-pegpair-parameter).

### Two separate fixtures are not a substitute

A `Fixture::<Bitcoin>` and a `Fixture::<Liquid>` started side by side sit on different networks, and
the standalone Liquid chain runs `-validatepegin=0`. Nothing tells that Elements node where a
mainchain is, so a claim against it cannot validate.

The trap is that **`Peg::connect` accepts the two anyway.** Bitcoin's regtest genesis is a hardcoded
chain parameter — the same value on every node, never generated per instance — and `liquidregtest`
carries that same hash as its parent, so the comparison `connect` makes agrees for two nodes that
have never heard of each other. That is measured, not assumed:
`crates/nigiri-rs-testcontainers/tests/peg_wiring.rs` starts two independent fixtures against a real
daemon and asserts `connect` succeeds.

So a successful `connect` tells you the Liquid node was built for a regtest parent chain, and nothing
about wiring. `PegPair` is what guarantees wiring, because it passes `bitcoind`'s container name to
`elementsd` itself. On a `Peg` you assembled by hand, the first real evidence of wiring is a
`claimpegin` that succeeds.

## Peg in, the short way

`complete_peg_in` runs the whole sequence — address, deposit, maturity, claim — and mines what it
needs:

```rust,ignore
use bitcoin::Amount;

# async fn example(pair: &nigiri_rs::testcontainers::PegPair) -> Result<(), Box<dyn std::error::Error>> {
let pegged = pair.peg().complete_peg_in(Amount::from_sat(100_000)).await?;

println!("deposit: {}", pegged.mainchain_txid);  // bitcoin::Txid
println!("claim:   {}", pegged.claim_txid);      // elements::Txid, the transaction that minted
println!("amount:  {}", pegged.amount);          // the argument, echoed back
# Ok(())
# }
```

Signature: `complete_peg_in(&self, amount: bitcoin::Amount) -> Result<PegIn, NigiriError>`.

It takes no address. `getpeginaddress` derives one from the Liquid node's own wallet, and the claim
credits that wallet; moving the funds anywhere else afterwards is an ordinary transfer, not part of
the peg.

### It mines, and the block count is not fixed

Three stages, and only the first two are predictable:

1. `faucet` sends the deposit and mines **exactly one** confirming block.
2. `pegin_confirmation_depth() - 1` further blocks reach the depth the sidechain reports.
3. `claimpegin` is submitted. If it fails in a way another block could plausibly fix, one more block
   is mined and it is resubmitted — **up to twenty extra blocks.**

So one peg-in mines at least `pegin_confirmation_depth()` Bitcoin blocks and at most that plus
twenty. **Assert `>=` on Bitcoin height, never `==`.** And read the depth from
`peg.pegin_confirmation_depth()` rather than hardcoding it: the accessor is cached at `connect` and
costs no round trip.

### `pegged.amount` proves nothing on its own

It is the argument you passed, returned unchanged, so comparing the two cannot fail no matter what
was really pegged in. Ask the Liquid wallet instead:

```rust,ignore
# async fn example(pair: &nigiri_rs::testcontainers::PegPair, pegged: &nigiri_rs::PegIn) -> Result<(), Box<dyn std::error::Error>> {
let received: serde_json::Value = pair
    .liquid()
    .rpc("gettransaction", (pegged.claim_txid.to_string(),))
    .await?;

// Elements is multi-asset, so `amount` is keyed per asset label rather than being one number.
// L-BTC's label is "bitcoin".
let serde_json::Value::Number(credited) = &received["amount"]["bitcoin"] else {
    panic!("a credited claim reports a numeric bitcoin amount: {received}");
};
let credited = bitcoin::Amount::from_str_in(&credited.to_string(), bitcoin::Denomination::Bitcoin)?;
assert!(credited > bitcoin::Amount::ZERO);
# Ok(())
# }
```

Parse through `serde_json::Number` and `from_str_in`, never `f64` — that is how the crate reads
amounts internally, and it stays exact.

## Peg in, driving it yourself

The primitives, for a wallet team that owns its own claim path or needs to intervene between steps.

First the address and the deposit:

```rust,ignore
use bitcoin::Amount;
use nigiri_rs::NigiriError;

# async fn example(pair: &nigiri_rs::testcontainers::PegPair) -> Result<(), Box<dyn std::error::Error>> {
let peg = pair.peg();

let request = peg.peg_in_request().await?;
// request.mainchain_address: bitcoin::Address, federation-controlled, tweaked by the Liquid
//   wallet's keys and network-checked against regtest.
// request.claim_script: String, hex — retained for a caller that submits its own claim.

// `faucet` mines exactly one confirming block, so the deposit sits at depth 1 after this.
let deposit = pair
    .bitcoin()
    .faucet(
        &request.mainchain_address.to_string(),
        Some(Amount::from_sat(150_000)),
    )
    .await?;

// Too early. `claim_peg_in` checks the count itself rather than letting the node reject the claim,
// so you get both numbers.
let error = peg
    .claim_peg_in(&deposit)
    .await
    .expect_err("a deposit one block deep is not claimable");
assert!(matches!(error, NigiriError::PegInImmature { have: 1, .. }));
# Ok(())
# }
```

Then maturity, and the retry that the lagging node makes necessary:

```rust,ignore
# async fn example(pair: &nigiri_rs::testcontainers::PegPair, deposit: &bitcoin::Txid) -> Result<(), Box<dyn std::error::Error>> {
# use nigiri_rs::NigiriError;
let peg = pair.peg();
let need = peg.pegin_confirmation_depth();
let mining_address = pair.bitcoin().new_address().await?.to_string();

// `faucet` already mined one. `generate_to_address` rejects a zero count, hence the guard.
let remaining = need.saturating_sub(1);
if remaining > 0 {
    pair.bitcoin()
        .generate_to_address(remaining, &mining_address)
        .await?;
}

// Reaching the reported depth is necessary but not sufficient: the Liquid node's view of the
// mainchain lags the mainchain itself. Mine one more block per rejection.
let mut claim = None;
for attempt in 0..=20 {
    if attempt > 0 {
        pair.bitcoin()
            .generate_to_address(1, &mining_address)
            .await?;
    }
    match peg.claim_peg_in(deposit).await {
        Ok(txid) => {
            claim = Some(txid);
            break;
        }
        // Only these two are worth another block. A dead socket or a malformed request is not —
        // stop rather than spending twenty blocks and then reporting a maturity problem that was
        // never real. The node's rejection of a premature claim arrives as `RpcFailed`.
        Err(NigiriError::PegInImmature { .. } | NigiriError::RpcFailed { .. }) => continue,
        Err(other) => return Err(other.into()),
    }
}
let claim = claim.expect("a matured deposit is claimable within twenty blocks");
# let _ = claim;
# Ok(())
# }
```

That loop applies the same retry **policy** `complete_peg_in` runs, but not the same loop.
`complete_peg_in` fetches the deposit and its merkle proof once, before retrying, and resubmits only
the claim; this snippet calls `claim_peg_in` on every attempt, which re-fetches both. It also checks
maturity once and fails fast on a genuinely immature deposit, where this snippet's loop retries
through that instead. It also keeps the last retryable error so an exhausted budget reports something
useful. If you do not need to intervene between the steps, call `complete_peg_in` and skip all of it.

Signatures:

- `peg_in_request(&self) -> Result<PegInRequest, NigiriError>`
- `claim_peg_in(&self, mainchain_txid: &bitcoin::Txid) -> Result<elements::Txid, NigiriError>`
- `pegin_confirmation_depth(&self) -> u64`

`claim_peg_in` omits the claim script from the `claimpegin` call: Elements infers it when the claiming
wallet issued the address, which it did. `request.claim_script` is there for callers submitting a
claim through their own code path.

## Peg out

Two calls, and the second one is the simulated federation:

```rust,ignore
use bitcoin::Amount;

# async fn example(pair: &nigiri_rs::testcontainers::PegPair) -> Result<(), Box<dyn std::error::Error>> {
let destination = pair.bitcoin().new_address().await?;

// A genuine Elements `sendtomainchain`. It burns L-BTC and encodes the Bitcoin destination in an
// output of the resulting Liquid transaction. Nothing services it: regtest has no federation.
let burn = pair
    .peg()
    .send_to_mainchain(&destination.to_string(), Amount::from_sat(10_000))
    .await?;

// Plays the missing part: decodes that output and pays it from the Bitcoin node's own wallet.
let released = pair.peg().release_peg_out(&burn).await?;

assert_eq!(released.liquid_txid, burn);
assert_eq!(released.destination, destination);
assert_eq!(released.amount, Amount::from_sat(10_000));
# Ok(())
# }
```

Signatures:

- `send_to_mainchain(&self, destination: &str, amount: bitcoin::Amount) -> Result<elements::Txid, NigiriError>`
- `release_peg_out(&self, liquid_txid: &elements::Txid) -> Result<PegOut, NigiriError>`

`destination` is `&str` rather than `bitcoin::Address` so you can deliberately supply a malformed one
and exercise the failure path. Every address argument in this crate is `&str` for that reason.

**There is no third call.** `initpegoutwallet` is deliberately not wrapped: this chain runs with PAK
enforcement off, so the node rejects it outright — there is no PAK entry to register — and
`sendtomainchain` does not require one.

### The destination is decoded, not passed in

`release_peg_out` takes only the Liquid txid. It reads the transaction back with `getrawtransaction`,
scans the outputs for the peg-out, and pulls the destination and the value out of that output.
Nothing you pass can override what the transaction says.

That is the point, and it is why peg-out is worth testing at all: **encode the destination wrongly
and no BTC arrives, exactly as on liquidv1.** That failure is what a consumer is here to catch, and
an API that took the destination as an argument could not reproduce it.

| What the scan found | Result |
| --- | --- |
| A peg-out for this pair | `Ok(PegOut)` |
| Its destination script is not a standard address, or its value is missing or unreadable | `NigiriError::PegOutputMalformed` |
| Every peg-out-shaped output names a different parent chain | `NigiriError::PegOutputMalformed`, detail naming both chains |
| No peg-out-shaped output at all | `NigiriError::PegOutputNotFound` |

An ordinary Liquid transfer is the last row:

```rust,ignore
# async fn example(pair: &nigiri_rs::testcontainers::PegPair) -> Result<(), Box<dyn std::error::Error>> {
# use bitcoin::Amount;
# use nigiri_rs::NigiriError;
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
assert!(matches!(error, NigiriError::PegOutputNotFound { .. }));
# Ok(())
# }
```

## Assert on the result

**Confirm through the node's own `getrawtransaction`, not through Esplora**, when the test mined the
block itself. The indexer catches up on its own schedule after startup, so routing an assertion about
a block you just mined through it introduces a race that has nothing to do with the peg.

The release mines its own confirming block, inherited from `faucet`:

```rust,ignore
# async fn example(pair: &nigiri_rs::testcontainers::PegPair, released: &nigiri_rs::PegOut) -> Result<(), Box<dyn std::error::Error>> {
let paid: serde_json::Value = pair
    .bitcoin()
    .rpc(
        "getrawtransaction",
        (released.bitcoin_txid.to_string(), 1_u64),
    )
    .await?;
assert_eq!(paid["confirmations"].as_u64(), Some(1));
# Ok(())
# }
```

A correct `PegOut` struct proves only that decoding worked. To prove the BTC arrived, find the output
that pays the decoded destination. **Do not assume an index:** the release pays change as well, so
there is more than one output and the payout is not at a fixed position.

```rust,ignore
# async fn example(pair: &nigiri_rs::testcontainers::PegPair, released: &nigiri_rs::PegOut, paid: &serde_json::Value) -> Result<(), Box<dyn std::error::Error>> {
// `scriptPubKey.address` first, since that is what the pinned Bitcoin version reports; the hex
// compared against the destination's own script is the fallback for a version that omits it.
let destination_hex = format!("{:x}", released.destination.script_pubkey());
let outputs = paid["vout"].as_array().expect("a verbose getrawtransaction reports its outputs");
let output = outputs
    .iter()
    .find(|output| match output["scriptPubKey"]["address"].as_str() {
        Some(address) => address == released.destination.to_string(),
        None => output["scriptPubKey"]["hex"].as_str() == Some(destination_hex.as_str()),
    })
    .expect("an output pays the decoded destination");

let serde_json::Value::Number(value) = &output["value"] else {
    panic!("a matching output reports a numeric value: {output}");
};
let value = bitcoin::Amount::from_str_in(&value.to_string(), bitcoin::Denomination::Bitcoin)?;
assert_eq!(value, released.amount);
# Ok(())
# }
```

On the Liquid side, nothing in the peg mines a Liquid block. A claim sits in the Liquid mempool until
you confirm it yourself:

```rust,ignore
# async fn example(pair: &nigiri_rs::testcontainers::PegPair, claim_txid: &elements::Txid) -> Result<(), Box<dyn std::error::Error>> {
let address = pair.liquid().new_address().await?;
pair.liquid()
    .generate_to_address(1, &address.to_string())
    .await?;

let confirmed: serde_json::Value = pair
    .liquid()
    .rpc("getrawtransaction", (claim_txid.to_string(), 1_u64))
    .await?;
assert_eq!(confirmed["confirmations"].as_u64(), Some(1));
# Ok(())
# }
```

## What not to assert

**No 1:1 invariant across the pair.** `release_peg_out` pays from the Bitcoin node's own wallet, not
from a locked reserve, because regtest has no functionaries to hold one. The Liquid side stays
honest — `sendtomainchain` genuinely burned — so L-BTC supply falls for real, while total BTC on the
mainchain side **grows with every release**. Any assertion of conservation across the pair reads the
wrong number. [What the peg simulates](explanation-what-the-peg-simulates.md) is the full argument.

Three smaller ones, all covered above: do not hardcode the confirmation depth, do not assert `==` on
Bitcoin height after a peg-in, and do not treat `PegIn::amount` as evidence.

## Troubleshooting

**`peg-in deposit has 1 confirmations, needs 8`** — `NigiriError::PegInImmature`. The deposit is not
mature; mine to `pegin_confirmation_depth()` and then retry a block at a time.

**A claim rejected at exactly the reported depth** — expected, not a bug. The node's view of the
mainchain lags the mainchain, and different runs need a different number of extra blocks. Mine one
more and resubmit. `complete_peg_in` already does, up to twenty times.

**`no peg-out output in Liquid transaction ...`** — `NigiriError::PegOutputNotFound`. The transaction
you named is not a `sendtomainchain`. An ordinary transfer gives exactly this.

**`peg is not configured: the Liquid node's parent chain is ... but the Bitcoin node's genesis is
...`** — `NigiriError::PegNotConfigured`. The Elements node was built for a different parent chain,
carrying testnet or mainnet parameters against a regtest `bitcoind`. It does **not** mean the two are
unwired; that comparison cannot see wiring either way.

**`This action cannot be completed during initial sync or reindexing.`** from `getpeginaddress` —
`NigiriError::RpcFailed` with code -4, and only on a pair you wired yourself. `liquidregtest`'s
genesis carries a fixed 2011 timestamp, so a node still at height 0 considers itself in initial block
download, and with `-validatepegin=1` it refuses outright. Mine one Liquid block. A `PegPair` mines it
during startup, which is why the Liquid half arrives at height 1; see
`crates/nigiri-rs-core/examples/peg_smoke.rs` for the whole hand-wired recipe.

**Containers left behind after a hard kill** — `Drop` runs even while panicking, but `SIGKILL` skips
it, and a pair's two halves carry **independent** UUIDs, so no single suffix matches all four. The
shared network name is the only thing they have in common; see
[Resource naming](reference-fixtures.md#resource-naming) for the recipe.

## Related

- [Tutorial: a round trip across Liquid's peg](tutorial-peg-round-trip.md) — the same operations from
  an empty crate, if you have not pegged anything before
- [What the peg simulates](explanation-what-the-peg-simulates.md) — which half is real, and what that
  costs you in assertions
- [Client API reference](reference-client.md#peg) — every `Peg` method and the three peg records
- [Fixture API reference](reference-fixtures.md#pegpair) — `PegPair`, `PegPairBuilder`, teardown
- [`#[nigiri_rs::test]`](reference-test-macro.md#a-pegpair-parameter) — the pair without the preamble
- [Errors](reference-errors.md) — the four peg variants, with what triggers each
- [How to call any node RPC](how-to-call-any-node-rpc.md) — `getsidechaininfo` and anything else
  unwrapped
