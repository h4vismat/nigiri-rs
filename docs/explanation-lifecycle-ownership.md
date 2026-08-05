# Lifecycle ownership

Why `nigiri-rs-core` never starts, stops, or deletes anything — and why the fixtures that do live in
a separate crate.

## The problem

A test-support library that talks to a blockchain node has an obvious temptation: start the node for
the caller. It knows the ports, it knows the credentials, it can shell out to `nigiri start`. Every
consumer would otherwise write that themselves.

The failure mode is what happens when the library is wrong about who owns the process.

Picture a client whose `Drop` stops the node it connected to. One test finishes and tears down the
regtest environment that four other tests are mid-way through using. Or a client that runs
`nigiri start` on construction: a developer with a Nigiri already running, holding state they care
about, gets it silently restarted. Or cleanup that deletes a data directory — on a machine where the
"regtest" data directory was, for one unlucky configuration, not the one the library assumed.

Every one of those is a library making a decision that belongs to the host, using knowledge it does
not have.

There is a second problem, smaller but real. Provisioning means Docker client libraries. A consumer
who talks to a regtest environment they already run has no use for those, but a single crate that
does both makes them pay for the dependency tree anyway.

## The approach

The core crate is **lifecycle-neutral**. It provides readiness checks but never:

- starts or stops services;
- invokes Docker or Testcontainers;
- provisions or deletes service data directories;
- removes containers or volumes;
- performs cleanup from `Drop`.

Cloning a client clones immutable configuration and a shared HTTP transport. It never implies
ownership of an external process, because the client has no concept of owning one.

That leaves a gap — someone has to start the services — and the gap is filled by a **separate crate**
that owns the lifecycle explicitly and completely:

```
nigiri-rs                    facade
├── nigiri-rs-core           the clients. Owns nothing.
├── nigiri-rs-testcontainers the fixtures. Owns containers, volumes, networks.
└── nigiri-rs-macros         #[nigiri_rs::test]
```

`nigiri-rs-core` does not depend on `nigiri-rs-testcontainers`. No Docker or Testcontainers
dependency reaches a consumer who only wants the client. The facade wires them together behind the
`testcontainers` feature, off by default.

So there are two paths, and they compose:

**You own the services.** Run Nigiri, or any compatible node plus Esplora indexer, and point a client
at it. The library reads and writes through the endpoints you gave it and touches nothing else.

**A fixture owns the services.** `Fixture::start()` creates containers, a network, and anonymous
volumes, all scoped to a per-fixture UUID. Dropping the fixture removes every one of them. Ownership
is total and it is visible in the type system: the containers live exactly as long as the `Fixture`
value.

Both can be used in the same test suite.

## Trade-offs

**A consumer has to start something.** The library will not do it for you, which is friction the first
time. The fixtures crate exists to make that one line rather than a README paragraph, but the line is
still yours to write.

**Ownership is now a type, so you can drop it too early.** `Fixture` owning its containers means a
fixture that goes out of scope takes the chain with it. `client()` returns a borrow so the common
mistake is caught at compile time — but `NigiriClient` is `Clone`, and a cloned client outliving its
fixture points at containers that no longer exist. That surfaces as connection-refused at runtime,
not as an error message about lifetimes.

**Two crates instead of one.** More manifests, a publish order to respect, and a facade to keep the
import paths stable. The facade re-exports `nigiri-rs-core` in full, so every path published at 0.2.0
still resolves.

**A fixture can't reuse a warm environment.** Each one pays its own startup: about 3 seconds for
Bitcoin, 1.5 for Liquid, on an idle machine with images pulled. A library that managed a shared node
could amortize that across a whole suite. The design trades those seconds for tests that cannot
interfere with each other.

## What this buys

The purchase is test isolation without a coordination protocol.

Because a fixture owns its chain, a test that mines, reorgs, or mutates a wallet cannot be observed
by any other test. No cross-process mutation lock, no serialized test execution, no "run this one
alone" annotation. Repository tests that once needed a `HostChainLock` to share a single Nigiri now
just run.

And because the core crate owns nothing, pointing it at a shared environment is safe by construction.
The worst a client can do to a node it did not start is send it requests.

## The one thing that stays yours

Mutating RPCs against a **shared** node still need coordination. `rpc()` will call
`invalidateblock` on a host-owned Nigiri that four other processes are using, and this crate will not
stop it. Lifecycle neutrality means the library does not manage that node — including not protecting
it from you.

Against a fixture the question does not arise. That asymmetry is the whole argument for fixtures.

## Related

- [Fixture API reference](reference-fixtures.md) — what ownership means concretely
- [How to point the client at services you run](how-to-point-at-your-own-services.md)
- [What "ready" means](explanation-fixture-readiness.md)
