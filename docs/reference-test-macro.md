# `#[nigiri_rs::test]` reference

An attribute macro that provisions a regtest stack per parameter and hands the test body a ready
client. Requires the `testcontainers` feature and Docker.

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

1. Starts one `Fixture` per parameter.
2. Binds each parameter to a **clone** of that fixture's client.
3. Calls your original body, now an inner `async fn`, with those clients.

The fixture handles stay owned by the wrapper, which is what keeps the containers alive for the
test's duration and tears them down when it ends — including on the panic path.

Generated code reaches everything it needs through `nigiri_rs::__private`, so **your crate needs
only `nigiri-rs`**. You do not add `tokio` or `nigiri-rs-testcontainers` to make an expansion
compile. (`__private` is `#[doc(hidden)]` and not covered by semver. Do not reference it.)

## Arguments

Two, both optional.

| Argument | Type | Default | Effect |
| --- | --- | --- | --- |
| `startup_timeout` | integer, seconds | 60 | Passed to `FixtureBuilder::startup_timeout` for every fixture in the test. |
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
| Two or more clients, any mix | Yes | One fixture each, **started concurrently**. |
| Any return type | Yes | Preserved verbatim, including `Result<_, _>`. |
| Other attributes on the fn | Yes | Preserved and re-emitted below the runtime attribute. |

Parameter types may be written as `NigiriClient<Bitcoin>` or fully qualified as
`nigiri_rs::NigiriClient<Bitcoin>` — the chain is read from the last path segment, so either import
style works.

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
| Parameter is not `NigiriClient<_>` | ``#[nigiri_rs::test]` parameters must be `NigiriClient<Bitcoin>` or `NigiriClient<Liquid>`; the chain is taken from this type` |
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

The chain is named because concurrent starts mean more than one can fail, and "the fixture" would
not say which.

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
