# What "ready" means

Why `Fixture::start()` waits for three services to agree on a block height before it returns, and
what that guarantee does not cover.

## The problem

A regtest stack is three moving parts:

```
   node  ──mines──▶  Electrs  ──serves──▶  Esplora HTTP
                        │
                        └───serves──────▶  Electrum TCP
```

The node mines. Electrs indexes what the node mined. Esplora and Electrum serve what Electrs indexed.
Each hop takes time.

A container that is *running* is not a service that is *ready*, and a naive fixture returns on the
first of these to be true:

- the container process started;
- the port accepts a TCP connection;
- the node answers `getblockcount`.

None of them means what a caller needs. A test that gets its client back at any of those moments and
immediately queries a funded address sees a wallet whose funds do not exist yet. The node has mined
101 blocks; the indexer has processed 4. The Esplora call returns an empty UTXO list, truthfully, and
the test fails.

That failure is the worst kind: **it is timing-dependent**. It passes on a fast machine, fails on a
loaded CI runner, and passes again when you add a `println!`. The natural fix — sleep a bit — is a
guess that is either too short (flaky) or too long (every test in the suite pays for it).

## The approach

A fixture polls all three services and returns only when they report the **same tip height**.

```rust,ignore
struct Heights {
    node: u64,      // getblockcount
    esplora: u64,   // GET /blocks/tip/height
    electrum: u64,  // blockchain.headers.subscribe
}

fn agree(&self) -> bool { /* all three equal */ }
```

Each round issues the three probes and compares. Not equal, or a probe failed? Wait 100 ms and go
again. That retry delay is a single shared constant, so polling cannot drift between the loops that
use it.

Two properties of the loop matter more than the comparison:

**A failing probe is not a failure.** A service that is not up yet returns an error, and the loop
treats that identically to a disagreement: retry. Only the budget expiring ends the loop with an
error. This is what makes the fixture robust to the ordering of container startup, which is not
something it controls.

**One shared deadline covers everything.** The 60-second budget is created once, before the node
container starts, and every operation and every pause is charged to it — container start, wallet
funding, indexer start, and this readiness wait. A slow phase spends budget the later phases then no
longer have.

The alternative, a fresh clock per phase, sounds fairer and is worse: four phases at 60 seconds each
is a four-minute fixture nobody budgeted for. A caller who sets `startup_timeout(Duration::from_secs(300))`
should get a fixture that takes at most 300 seconds, and does.

When the budget does expire, the error names which service was behind and attaches that container's
log:

```
fixture was not ready after 60s: node=101 esplora=4 electrum=4; <container log>
```

The service label on this variant is `fixture` rather than a single container name, because the
condition that failed is agreement between three of them. The height triple is what names the
laggard.

That is the difference between a diagnosable failure and a bare timeout. The last height reading tells
you the shape of the problem before you read a single log line.

### Funding differs; readiness does not

The two chains fund a wallet differently. Bitcoin has a block subsidy, so a Bitcoin fixture mines 101
blocks and waits for the coinbase to mature — 100 confirmations, plus the block itself. Liquid has no
subsidy, so mining funds nothing; a Liquid fixture connects the genesis outputs instead, and mines a
single block only because callers reasonably expect a nonzero tip.

Hence the arrival heights: **101 for Bitcoin, 1 for Liquid.**

Everything else is shared. Container, root RPC, `createwallet`, the wallet-scoped client, and this
readiness loop are identical; funding is the one thing the chain trait has to supply.

### A pair runs the whole thing twice

A [`PegPair`](reference-fixtures.md#pegpair) is two of these stacks on one Docker network, so it runs
this same agreement for each half — six probes, two triples — under one shared budget rather than two
budgets side by side. Nothing about the loop changes; there is simply twice as much of it, which is
why a pair's default budget is 120 seconds against a single fixture's 60.

A pair then does one thing more before `start` returns: it pairs the two clients through
`Peg::connect`, charged to the same clock. That check compares the Liquid node's parent chain against
the Bitcoin node's genesis, which is a weaker statement than it sounds — see
[what `Peg::connect` proves](reference-client.md#what-connect-proves-and-what-it-does-not).

## The guarantee, precisely

When `start()` returns:

- node, Esplora, and Electrum report the same tip;
- the wallet holds spendable funds;
- every endpoint on the client is the runtime-mapped one.

**That agreement is established once, at startup.** It is not maintained.

Blocks you mine afterwards reach the indexer on its own schedule. So this can fail:

```rust,ignore
client.generate_to_address(1, &address).await?;
let utxos = client.get_utxos(&address).await?;   // may not see it yet
```

The node has the block. Esplora may not. Poll rather than assume:

```rust,ignore
let txid = client.faucet(&address, None).await?;
client.wait_for_confirmation(&txid, Duration::from_secs(30)).await?;
```

`wait_for_confirmation` polls Esplora every 500 ms, which is the service most wallets read.

Note that `faucet` and `broadcast_tx` each mine a confirming block before returning, so the
transaction is confirmed **on the node** by the time you have the txid. `mint`, `faucet_asset`, and
raw `rpc()` calls do not mine at all.

## Trade-offs

**Startup costs seconds, not milliseconds.** About 3 seconds for a Bitcoin fixture on an idle machine
with images pulled, 1.5 for Liquid. A fixture that returned as soon as the node answered would be
faster and would hand you a chain you cannot query. The seconds buy a test that cannot be flaky for
this reason.

**Three probes per round, in sequence.** Each poll round pays the sum of three round trips rather
than the longest, though nothing in a round depends on another probe's result. Running them under
`tokio::join!` is [TODOS.md P4](../TODOS.md); the saving is unmeasured and plausibly single-digit
milliseconds on loopback, which is why it sits below other work. Readiness is the most load-bearing
code in the crate and is not worth touching for an unmeasured gain.

**The node and indexer start sequentially.** `start()` brings the node fully up — container, RPC,
wallet, the whole 101-block fund — before starting Electrs, though there is no compile-time
dependency between them. Overlapping them is [TODOS.md P3](../TODOS.md), and it has a real blocker:
the current ordering is load-bearing for diagnostics. An Electrs failure attaches the *node's*
container log, on the grounds that the node is running and holds the only account of what Electrs was
pointed at. Joining the two means an Electrs failure can arrive while the node does not yet exist, so
that pairing needs rethinking rather than deleting.

**One number, not a full consistency check.** Equal tip heights do not prove the three services agree
on the *contents* of that block. In a single-node regtest chain with one indexer there is no
mechanism to make them disagree, so the cheap check is the right one. It would not be on a network
with competing tips.

## Why not just sleep

Worth stating explicitly, because it is the obvious alternative and it fails in a specific way.

A sleep is a guess about the slowest machine that will ever run the test. Too short and it is flaky
on CI. Too long and every fixture in the suite pays the worst case — 18 fixture-starting tests times
five wasted seconds is a minute and a half per run, forever.

Polling costs the actual time, which on a warm machine is a few hundred milliseconds, and adapts to a
slow one without anyone tuning a constant. The budget is there to bound the pathological case, not to
pace the normal one.

## Related

- [Fixture API reference](reference-fixtures.md) — timings, guarantees, `startup_timeout`
- [How to run a throwaway regtest stack](how-to-run-a-fixture.md) — troubleshooting readiness
  timeouts
- [Errors](reference-errors.md) — `ReadinessTimeout`, `Probe`, `Bootstrap`
