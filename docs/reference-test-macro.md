# `#[nigiri_rs::test]` reference

An attribute macro that provisions a regtest stack per parameter and hands the test body a ready
client or owning pair. Requires the `fixtures` feature and Docker.

```rust,ignore
use nigiri_rs::{Bitcoin, NigiriClient};

#[nigiri_rs::test]
async fn my_wallet_sees_its_funding(client: NigiriClient<Bitcoin>) -> Result<(), Box<dyn std::error::Error>> {
    let address = client.new_address().await?;
    client.faucet(&address.to_string(), None).await?;
    Ok(())
}
```

It replaces the `#[tokio::test]` attribute — do not write both.

## What it generates

The macro rewrites your function into a parameterless `#[tokio::test]` wrapper that:

1. Starts one fixture per parameter — a `Fixture` for a client parameter, or the requested
   `PegPair`/`LndPair` composite.
2. Binds each client parameter to a **clone** of that fixture's client. Pair parameters are bound to
   their owning handle, by move.
3. Calls your original body, now an inner `async fn`, with those bindings.

Fixture handles stay owned by the wrapper, which keeps containers alive for the test and tears them
down on normal return or panic. `PegPair` and `LndPair` already are their four-container handles, so
moving either into the body keeps its topology alive for exactly as long.

Generated code reaches everything it needs through `nigiri_rs::__private`, so **your crate needs
only `nigiri-rs`**. You do not add `tokio` or `nigiri-rs-fixtures` to make an expansion
compile. (`__private` is `#[doc(hidden)]` and not covered by semver. Do not reference it.)

## Arguments

Two, both optional.

| Argument | Type | Default | Effect |
| --- | --- | --- | --- |
| `startup_timeout` | integer, seconds | 60 (`Fixture`), 120 (`PegPair`), 180 (`LndPair`) | Passed to the corresponding builder for every fixture in the test. Omitted, each keeps its own default. |
| `flavor` | string | current-thread | Forwarded to `#[tokio::test(flavor = ...)]`. |

```rust,ignore
#[nigiri_rs::test(startup_timeout = 120)]
async fn slow_first_pull(client: NigiriClient<Bitcoin>) { /* ... */ }

#[nigiri_rs::test(flavor = "multi_thread")]
async fn needs_real_threads(client: NigiriClient<Bitcoin>) { /* ... */ }
```

Both may be combined: `#[nigiri_rs::test(startup_timeout = 120, flavor = "multi_thread")]`.

`flavor` is passed through to tokio verbatim and is not validated by this macro — tokio reports an
unknown flavor.

**There is deliberately no chain argument.** The chain comes from the parameter type, so the
attribute and the signature cannot disagree.

## Accepted signatures

The function must be `async`. Beyond that:

| Shape | Accepted | Result |
| --- | --- | --- |
| No parameters | Yes | Degrades to a plain async test. No containers started. |
| One `NigiriClient<Bitcoin>` | Yes | One Bitcoin fixture. |
| One `NigiriClient<Liquid>` | Yes | One Liquid fixture. |
| One `PegPair` | Yes | One wired Bitcoin and Liquid pair — four containers. |
| One `LndPair` | Yes | One Bitcoin/Electrs stack plus two ready-to-pay LND nodes — four containers. |
| Two or more parameters, any mix | Yes | One fixture or pair each, **started concurrently**. |
| Any return type | Yes | Preserved verbatim, including `Result<_, _>`. |
| Other attributes on the fn | Yes | Preserved and re-emitted below the runtime attribute. |

Parameter types may be written as `NigiriClient<Bitcoin>` or fully qualified as
`nigiri_rs::NigiriClient<Bitcoin>` — the chain is read from the last path segment, so either import
style works. `PegPair`/`LndPair` and their `nigiri_rs::fixtures::*` paths are matched the same way.
Pair types take no generic arguments, so `LndPair<Bitcoin>` and `PegPair<Bitcoin>` are rejected.

### A `PegPair` parameter

A `PegPair` names no chain, because it is both. The parameter is bound to the
[`PegPair`](reference-fixtures.md#pegpair) itself rather than to a clone of a client: the pair *is*
the handle that owns the four containers, so it moves into your body instead of being cloned out of
something the wrapper keeps.

```rust,ignore
use bitcoin::Amount;
use nigiri_rs::fixtures::PegPair;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[nigiri_rs::test(startup_timeout = 180)]
async fn a_peg_pair_parameter_starts_a_wired_stack(peg: PegPair) -> Result<(), BoxError> {
    let pegged = peg.peg().complete_peg_in(Amount::from_sat(100_000)).await?;

    // `pegged.amount` is `complete_peg_in`'s own argument echoed back, so comparing it against the
    // amount just passed in cannot fail no matter what was really pegged in. Ask the Liquid node
    // about the claim instead: `-txindex=1` makes even a mempool transaction retrievable, so this
    // needs no mined block to prove the node genuinely knows it.
    let claim: serde_json::Value = peg
        .liquid()
        .rpc("getrawtransaction", (pegged.claim_txid.to_string(), 1_u64))
        .await?;
    assert!(
        claim["vout"].as_array().is_some_and(|vout| !vout.is_empty()),
        "the Liquid node must know the claim transaction and report its outputs: {claim}"
    );

    // Both halves are reachable through the pair, which is what distinguishes it from two
    // independent stacks. `>= 101` is a floor a pair that pegged nothing already clears — 101 is
    // also the arrival height, so this is not evidence that `complete_peg_in` mined anything.
    // (Not sampled before and after: `block_height` is Esplora-backed and the blocks
    // `complete_peg_in` just mined reach the indexer on its own schedule, so a before/after
    // comparison would be flaky.)
    assert_eq!(peg.liquid().block_height().await?, 1);
    assert!(peg.bitcoin().block_height().await? >= 101);
    Ok(())
}
```

That is `a_peg_pair_parameter_starts_a_wired_stack` from
`crates/nigiri-rs/tests/macro_smoke.rs`, copied as it runs there. Note the `>=` on the Bitcoin height:
`complete_peg_in` mines a number of blocks that is not fixed. See
[`complete_peg_in` mines](reference-client.md#complete_peg_in-mines-and-how-many-blocks-is-not-fixed).

**A `PegPair` may be mixed with client parameters, and those still produce independent stacks.** A
test taking `(PegPair, NigiriClient<Bitcoin>)` gets six containers: the pair's four on their shared
network, plus a two-container Bitcoin fixture on a network of its own. That extra client is *not* the
pair's Bitcoin half — reach that through `peg.bitcoin()`.

Without a `startup_timeout` argument a `PegPair` parameter gets `PegPair::start()`, and therefore the
pair's own 120-second default rather than the 60 seconds a `Fixture` defaults to. With the argument,
the value applies to every fixture in the test, pair included.

### An `LndPair` parameter

`LndPair` also moves into the body. It returns Alice, Bob, the backing Bitcoin client, and the
confirmed channel point from one owner. Its default is 180 seconds. Startup has already settled two
1,000-msat public readiness invoices in opposite directions, so macro tests must identify their own
payment and invoice records by hash rather than assume empty histories.

```rust,ignore
use std::time::Duration;
use nigiri_rs::fixtures::LndPair;
use nigiri_rs::{CreateInvoiceRequest, InvoiceState, Millisats, PaymentOptions, PaymentState};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[nigiri_rs::test(startup_timeout = 240)]
async fn settles_a_real_payment(pair: LndPair) -> Result<(), BoxError> {
    let invoice = pair.bob().create_invoice(CreateInvoiceRequest::new(
        Millisats::new(25_000),
        "macro payment",
        Duration::from_secs(60),
    )?).await?;
    let hash = invoice.payment_hash();
    let payment = pair.alice().pay_invoice(
        invoice.invoice(),
        PaymentOptions::new(Millisats::new(10_000), Duration::from_secs(30))?,
    ).await?;
    assert_eq!(payment.payment_hash(), hash);
    assert_eq!(payment.state(), PaymentState::Succeeded);
    let settled = pair.bob().lookup_invoice(hash).await?;
    assert_eq!(settled.payment_hash(), hash);
    assert_eq!(settled.state(), InvoiceState::Settled);
    Ok(())
}
```

### Concurrency

With two or more parameters the fixtures start under `tokio::join!` rather than one after another.
Starting a stack is almost entirely waiting on Docker and on the indexer reaching the node's tip, so
overlapping that wait is close to free.

Measured on the two-chain test, five runs each on 2026-08-05 with a warm-up discarded and
non-overlapping ranges: **4.47 s concurrent against 6.30 s sequential**. Bitcoin mines 101 blocks
while Liquid mines one, so overlapping is worth roughly the whole Liquid startup.

A single fixture is emitted sequentially — joining one future buys nothing — and zero fixtures emit
no startup code at all.

If one start fails, the others still finish; the wrapper then panics on the first failure and the
remaining handles drop as the panic unwinds, running the same teardown a successful test would.

## Rejections

All of these are compile errors with the message shown. Six are pinned by `trybuild` cases in
`crates/nigiri-rs-macros/tests/ui/`: `not_async.rs`, `generic_fn.rs`, `reserved_parameter_name.rs`,
`unsupported_parameter.rs`, `unknown_argument.rs`, and `bad_timeout.rs`.

| Cause | Message |
| --- | --- |
| Not `async` | ``#[nigiri_rs::test]` requires an `async fn`: starting a fixture awaits Docker` |
| Generic parameters | ``#[nigiri_rs::test]` cannot be applied to a generic function: the test harness has no way to choose the type arguments` |
| `where` clause | ``#[nigiri_rs::test]` cannot be applied to a function with a `where` clause: the test harness has no way to satisfy it` |
| Takes `self` | ``#[nigiri_rs::test]` cannot be applied to a method taking `self`` |
| Non-identifier pattern in a parameter | `each parameter must be a plain name, so the generated wrapper can bind it` |
| Parameter named `__nigiri_rs_*` | `parameter names beginning `__nigiri_rs_` are reserved for the code `#[nigiri_rs::test]` generates; rename this parameter` |
| Parameter is not an accepted fixture type | ``#[nigiri_rs::test]` parameters must be `NigiriClient<Bitcoin>`, `NigiriClient<Liquid>`, `PegPair`, or `LndPair`; the chain is taken from this type` |
| Unknown attribute argument | ``unknown argument `x`; `#[nigiri_rs::test]` accepts `startup_timeout` and `flavor`. The chain is taken from the parameter type, not from an argument.`` |
| `startup_timeout` not an integer | ``startup_timeout` takes a number of seconds, e.g. `#[nigiri_rs::test(startup_timeout = 120)]`` |
| `flavor` not a string | ``flavor` takes a string, e.g. `flavor = "multi_thread"`` |

The generic and reserved-prefix rejections exist to replace worse errors. A still-generic inner
function would make rustc report "type annotations needed" against the attribute rather than the
signature; a parameter named `__nigiri_rs_inner` would shadow the generated inner function and
produce "expected function, found struct NigiriClient" against code the author never wrote.

## Tests are never `#[ignore]`d

Deliberate. An ignored Docker test reports green having verified nothing, and this project shipped
that exact failure mode twice — once as a CI filter that matched zero tests and exited 0, once as a
test that had never run in any CI job.

If Docker is unavailable, a generated test **fails loudly** with:

```
nigiri-rs: the Bitcoin fixture could not start; is Docker running?
```

The chain is named because concurrent starts mean more than one can fail. A pair names itself:
`the PegPair fixture could not start` or `the LndPair fixture could not start`.

## Testing the macro itself

`trybuild` can only ever check the rejections above. Generated code names
`::nigiri_rs::__private::…`, and `nigiri-rs-macros` cannot depend on the facade that re-exports it —
that would be a dependency cycle. **Successful expansion is proven only by**
`crates/nigiri-rs/tests/macro_smoke.rs`, which runs against real containers. Do not try to add a
passing-expansion `trybuild` case to the macro crate.

## Related

- [Tutorial: your first fixture-backed test](tutorial-first-test.md)
- [Fixture API](reference-fixtures.md) — what the macro starts for you
- [Client API](reference-client.md) — what the injected client can do
