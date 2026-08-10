# What the peg simulates

Which half of Liquid's peg is real on regtest, which half this crate plays, and what that costs you
in the assertions you are allowed to write.

## The shape of the thing

Liquid's peg has two directions and they are not symmetric. Peg-in is a Bitcoin deposit to an address
the federation controls, followed by a claim on Liquid that carries a merkle proof of that deposit;
the sidechain validates the proof itself and mints. Peg-out is the reverse and it is not a proof at
all — the L-BTC is burned, a Bitcoin destination is written into the burning transaction, and then
*someone else* has to notice and pay. On liquidv1 that someone is the functionaries.

`liquidregtest` has no functionaries. So one of those two directions ports to a throwaway chain
intact and the other one stops halfway. That asymmetry is the whole subject of this page.

## Peg-in is real

Not approximated, not stubbed, not a transfer dressed up as a peg.

`Peg::peg_in_request` calls `getpeginaddress` on the Elements node and gets a genuine
federation-controlled address, tweaked by the claiming wallet's own keys. Funding it is an ordinary
Bitcoin transaction. `Peg::claim_peg_in` then fetches that transaction and a real `gettxoutproof`
merkle proof from the Bitcoin node and submits a real `claimpegin` to the Elements node, which
validates the proof against the `bitcoind` it was started pointing at — `-validatepegin=1`, with
`-mainchainrpc*` aimed at that node — before it mints anything.

Every step a consumer's own claim path would take is therefore exercised for real, and a mistake in
it fails here the way it would fail on liquidv1. The repository's own
`crates/nigiri-rs-testcontainers/tests/peg_pair.rs` drives the whole sequence end to end against four
real containers, in both the one-call form and the primitives.

## Peg-out is real up to the point where the federation would act

`Peg::send_to_mainchain` is a genuine Elements `sendtomainchain`. It genuinely burns L-BTC, and the
transaction it produces genuinely carries a peg-out output: an `OP_RETURN`, the parent chain's
genesis hash, and the destination's `scriptPubKey`. Nothing about that is simulated.

And then nothing happens. There is no federation watching, so the BTC never moves, and a test that
stopped there would be testing half a feature. `Peg::release_peg_out` plays the missing part: it
reads the transaction back, decodes the peg-out output, and pays the destination on Bitcoin.

The line between the two is worth being precise about, because it is where every misleading assertion
comes from. **Everything up to and including the burn is real. The release is this crate pretending.**

### Why the destination is decoded rather than passed in

`release_peg_out` takes one argument, the Liquid transaction ID. It could just as easily have taken
the destination — the caller knows it, having just supplied it to `send_to_mainchain` — and the
implementation would be shorter.

That would also make peg-out pointless to test. The interesting failure on liquidv1 is not "the
federation was slow"; it is "you encoded the destination wrongly and the BTC went nowhere". A release
that trusted an argument would pay the address the caller meant, while the real federation would pay
the address the caller actually wrote. The bug would pass.

So the destination comes out of the transaction and nothing the caller says can override it. Encode
it wrongly and no BTC arrives, which is exactly the outcome liquidv1 would give. That single decision
is what makes the simulated half worth having at all.

### Why there is no reserve, and what it breaks

On liquidv1 the released BTC comes from a pool the federation holds — the same coins that were pegged
in. Regtest has no such pool, so the release pays from the Bitcoin node's **own wallet**. It is not
the BTC anyone pegged in. It was mined by the fixture, and there is more of it available than was
ever locked.

The consequence is one-sided in a way that is easy to get wrong. The Liquid side is honest: the burn
was real, so L-BTC supply falls by exactly what was pegged out. The Bitcoin side is not: total BTC on
the mainchain grows with every release, because coins are being created out of the fixture's wallet
rather than moved out of a reserve.

**So no 1:1 invariant holds across the pair, and any assertion of conservation reads the wrong
number.** A test that sums both sides and expects them to agree is not detecting a bug when it fails
and not proving anything when it passes. Assert on what actually moved — the burn on Liquid, the
payout to the decoded destination on Bitcoin — and leave supply alone.

## Why the reported confirmation depth is not enough

`getsidechaininfo` reports a `pegin_confirmation_depth`, and the obvious reading is that a deposit at
that depth is claimable. It is not. The node rejects a claim at exactly the depth it reports and
accepts it a few blocks later, because the node's view of the mainchain lags the mainchain itself.

"A few" is the operative word, and it is why this is handled the way it is. Two independent
observations were recorded while this was built: one run had the claim accepted two blocks past the
reported depth, and the note on the crate's own retry bound records another rejected at 8 and accepted
at 11. **Different runs give different numbers.**

That is the argument against a fixed margin. A hardcoded `+3` would encode a number measured once, on
one machine, against one image, and would fail the first time a slower machine needed a fourth block.
`complete_peg_in` mines **one block at a time and resubmits**, up to twenty, which adapts to however
far behind the node happens to be on the day. And it retries only on the two error variants another
block could plausibly fix — a dead socket or a malformed reply is not a maturity problem, and
spending twenty blocks before reporting it would bury the real error.

The same reasoning applies to the depth itself: read `pegin_confirmation_depth()` rather than
hardcoding 8, so a chain configured with a lowered `peginconfirmationdepth` and a production-shaped
one both work.

## Why `initpegoutwallet` is not wrapped

The natural shape for the peg-out API is three methods — register a mainchain wallet, send, release —
and it is two.

`initpegoutwallet` is rejected on this chain unconditionally, with `PAK enforcement is not enabled on
this network`. PAK enforcement is off, so there is no PAK entry to register, and the call has nothing
to do. That was observed on two different Elements builds, in every descriptor form tried, so the
blocker is the chain's configuration rather than the image — and not something a caller can turn on
from outside.

Meanwhile `sendtomainchain` works without it. So a wrapper for `initpegoutwallet` would be a method
that cannot succeed, sitting next to one that does not need it — an API that exists only to be
explained away in its own documentation. A method that cannot succeed is worse than no method, so
there is none. A consumer working against a custom environment that *does* enable PAK can reach the
call through [`rpc()`](how-to-call-any-node-rpc.md).

## What `Peg::connect` proves, and what it does not

`connect` reads `getsidechaininfo` from the Liquid node and compares its reported parent block hash
against the Bitcoin node's `getblockhash 0`. The tempting reading is that this verifies the pair.

It does not, and the reason is that Bitcoin's regtest genesis is a hardcoded chain parameter. It is
the same value on every regtest node ever started, never generated per instance, and `liquidregtest`
carries that same value as its parent. Two nodes that have never heard of each other therefore agree
on it, and `connect` accepts them. That is measured rather than reasoned:
`crates/nigiri-rs-testcontainers/tests/peg_wiring.rs` starts two independent fixtures against a real
daemon and asserts that `connect` succeeds, precisely so this claim cannot quietly drift.

What the comparison does catch is a Liquid node built for a **different** parent chain — one carrying
testnet or mainnet parameters, pointed at a regtest `bitcoind`. That is a real mistake and it is
worth catching early. It is also the entire extent of the check. Note too that the mismatch direction
does not license the stronger reading either: a genuinely wired pair whose Elements node carries
different chain parameters will also mismatch.

Wiring is guaranteed by **construction**, not by verification. `PegPair` starts `elementsd` with
`-validatepegin=1` and `-mainchainrpc*` addressing its own `bitcoind` by container name, on a network
they share; that is what makes a claim validate, and it is true before `connect` is ever called. On a
`Peg` you assembled yourself out of two clients, the first real evidence of wiring is a `claimpegin`
that succeeds.

## What all of this buys

A consumer can exercise the code they actually own. The claim path is real, so a bug in it surfaces.
The peg-out encoding is real and the destination is read back out of it, so a bug in that surfaces
too. What cannot be exercised here is the federation's own behaviour — its rotation, its failure
modes, its reserve accounting — and none of that is code a consumer writes.

The cost is one class of assertion. Supply across the pair is fiction on the Bitcoin side, and that is
the one thing a reader can get wrong without noticing, which is why every page touching the peg
repeats it.

## Related

- [Tutorial: a round trip across Liquid's peg](tutorial-peg-round-trip.md) — the boundary described
  here, met one step at a time against real containers
- [How to peg in and peg out](how-to-peg.md) — the working flows in both directions
- [Client API reference](reference-client.md#peg) — every `Peg` method, and the peg records
- [Fixture API reference](reference-fixtures.md#pegpair) — the four-container pair the flows run
  against
- [Lifecycle ownership](explanation-lifecycle-ownership.md) — why the pair owns its containers and
  the client owns nothing
