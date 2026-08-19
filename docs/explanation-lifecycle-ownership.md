# Lifecycle ownership

Why the Bitcoin/Liquid and Lightning protocol crates never start, stop, or delete anything — and why
the fixtures that do live in a separate crate.

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

Both protocol crates are **lifecycle-neutral**. `nigiri-rs-core` owns only Bitcoin and Liquid;
`nigiri-rs-lnd` owns the host-facing Lightning boundary. They provide readiness checks but never:

- starts or stops services;
- invokes Docker or Testcontainers;
- provisions or deletes service data directories;
- removes containers or volumes;
- performs cleanup from `Drop`.

Cloning either client copies immutable configuration and shares a transport. It never implies
ownership of an external process, because neither client has a concept of owning one.

That leaves a gap — someone has to start the services — and the gap is filled by a **separate crate**
that owns the lifecycle explicitly and completely:

```
nigiri-rs                    facade
├── nigiri-rs-core           Bitcoin/Liquid protocol clients. Own nothing.
├── nigiri-rs-lnd            Lightning protocol client. Owns no daemon.
├── nigiri-rs-fixtures       fixtures. Own containers, volumes, networks.
└── nigiri-rs-macros         #[nigiri_rs::test]
```

Neither protocol crate depends on `nigiri-rs-fixtures`; the two protocol crates do not depend on one
another either. No Docker dependency reaches a consumer who only wants a client. The facade exposes
the host-managed Lightning surface behind `lnd` and lifecycle management behind `fixtures`, both off
by default. `fixtures` implies `lnd` because `LndPair` returns authenticated `LndClient` values.

So there are two paths, and they compose:

**You own the services.** Run Nigiri (or compatible Bitcoin/Elements services) and/or LND, then point
the corresponding client at them. The client reads and writes through the endpoints and credentials
you supplied and touches nothing else.

**A fixture owns the services.** `Fixture::start()` creates one chain stack. `PegPair` owns a wired
Bitcoin/Liquid pair. `LndPair` owns a backing Bitcoin stack plus Alice and Bob LND. Dropping the
owning handle requests best-effort cleanup; explicit `shutdown()` awaits cleanup and reports an
error. A hard process kill can still leave resources for manual removal. Ownership is visible in
the type system: the owning value defines the intended service lifetime.

For `LndPair`, dependency order is part of the contract. Bob is removed before Alice, then Electrs
before bitcoind, and only then is the shared network removed. Startup, failure diagnostics, and
cleanup all consume the same 180-second whole-call deadline. If cleanup cannot finish in the
remaining budget, the public start returns on time while the dedicated supervisor continues bounded
best-effort reverse-order cleanup rather than leaking ownership into the client handles.

Both can be used in the same test suite.

## Trade-offs

**A consumer has to start something.** The library will not do it for you, which is friction the first
time. The fixtures crate exists to make that one line rather than a README paragraph, but the line is
still yours to write.

**Ownership is now a type, so you can drop it too early.** `Fixture` owning its containers means a
fixture that goes out of scope takes the chain with it. `client()` returns a borrow so the common
mistake is caught at compile time — but `NigiriClient` is `Clone`, and a cloned client outliving its
fixture points at containers that no longer exist. `LndClient` is also cheaply cloneable, so the same
caveat applies to `alice()` and `bob()`. That surfaces as connection-refused at runtime.

**Five crates instead of one.** More manifests, a publish order to respect, and a facade to keep the
import paths stable. Publish the two protocol crates before fixtures, then publish the facade;
macros have no workspace dependency. The separation prevents generated LND/gRPC details and Docker
lifecycle from becoming part of the Bitcoin/Liquid core boundary.

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

And because the protocol crates own nothing, pointing them at shared environments cannot cause a
client drop to stop or delete the service. The clients can still send mutating requests, so callers
remain responsible for coordinating shared state.

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
