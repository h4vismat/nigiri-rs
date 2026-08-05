# Typed networks

Why `Bitcoin` and `Liquid` are type parameters rather than an enum, and what that catches at compile
time.

## The problem

Bitcoin and Liquid are close enough to share an API and different enough that sharing one carelessly
produces bugs that compile.

A transaction ID is a `bitcoin::Txid` on one chain and an `elements::Txid` on the other. They are
different types with the same shape — 32 bytes, hex-encoded — so a value of one will happily parse as
the other and be wrong. Same for block hashes. Addresses differ more visibly (`bcrt1q…` against
`ert1q…`) but both are strings by the time they reach an RPC parameter.

Then there are methods that exist on one chain only. `mint` and `faucet_asset` are Elements
operations; Bitcoin has no asset issuance. A single client type has to do something about that, and
the usual answers are bad:

- Return `Err(UnsupportedOnThisNetwork)` at runtime. The mistake compiles, ships, and fails in a
  test run.
- Panic. Worse.
- Have the caller check a `network` field first. Now correctness depends on discipline.

The version before 0.2.0 had a third problem underneath these: responses came back as loosely-typed
records. A caller who wanted a real `bitcoin::Txid` parsed a string themselves, and every call site
repeated the parse and its error handling.

## The approach

`NigiriClient` is generic over a sealed network marker:

```rust
pub enum Bitcoin {}   // uninhabited: a type, never a value
pub enum Liquid {}

pub struct NigiriClient<N: NigiriNetwork> { /* ... */ }
```

`NigiriNetwork` maps each marker to the native types that chain actually uses:

| Associated type | `Bitcoin` | `Liquid` |
| --- | --- | --- |
| `Txid` | `bitcoin::Txid` | `elements::Txid` |
| `BlockHash` | `bitcoin::BlockHash` | `elements::BlockHash` |
| `Address` | `bitcoin::Address<NetworkChecked>` | `elements::Address` |
| `Utxo` | `BitcoinUtxo` | `LiquidUtxo` |
| `TxInfo` | `BitcoinTxInfo` | `LiquidTxInfo` |
| `AddressInfo` | `BitcoinAddressInfo` | `LiquidAddressInfo` |

Shared methods are written once, against the associated types:

```rust,ignore
pub async fn get_tx(&self, txid: &N::Txid) -> Result<N::TxInfo, NigiriError>
```

One implementation, two contracts. A `bitcoin::Txid` cannot be passed to a `NigiriClient<Liquid>`,
because `N::Txid` is `elements::Txid` there. The mistake is a type error, not a test failure.

Chain-only methods are implemented on the concrete type:

```rust,ignore
impl NigiriClient<Liquid> {
    pub async fn mint(...) -> Result<MintResponse, NigiriError>;
    pub async fn faucet_asset(...) -> Result<elements::Txid, NigiriError>;
}
```

`bitcoin_client.mint(...)` does not resolve. There is no runtime error to return because there is no
runtime. `compile_fail` doctests in `nigiri-rs-core/src/lib.rs` pin exactly this.

The trait is **sealed**: the parsing contracts live in a private supertrait, so no downstream crate
can add a network. That is deliberate. Parsing a txid, validating an address against the right
network parameters, and building the positional RPC vector for `sendtoaddress` are all
network-specific invariants this crate has to guarantee. An outside implementation could silently
break them.

Parsing happens once, at the boundary. `new_address` on Bitcoin returns an address already checked
against `bitcoin::Network::Regtest`; on Liquid, one checked against
`elements::AddressParams::ELEMENTS`. An address for the wrong network is rejected there, not carried
around as a string that might be fine.

### Where the types stop

Address **inputs** stay `&str`:

```rust,ignore
pub async fn get_utxos(&self, address: &str) -> Result<Vec<N::Utxo>, NigiriError>
```

That looks like an inconsistency. It is a concession to LWK, whose confidential addresses are already
strings by the time a consumer holds one; forcing a parse-then-stringify round trip at every call site
buys nothing when the value goes straight into an HTTP path or a JSON parameter.

Monetary values do not get the same concession. Everything is `bitcoin::Amount`, on both chains,
serialized as exact decimal BTC through `serde_json::Number`. No amount in this crate passes through
`f64`.

## Trade-offs

**Every signature carries a type parameter.** `NigiriClient<Bitcoin>` is noisier than `NigiriClient`,
and a function generic over both networks has to be written generic over `N: NigiriNetwork` with the
associated-type bounds spelled out. That is a real cost in a codebase that handles both chains
uniformly.

**No runtime chain selection.** You cannot read a config file and produce a
`NigiriClient<whatever it said>`. The type is fixed at compile time. An application that genuinely
needs to switch chains at runtime has to hold both clients, or box behind its own trait. For a
regtest testing library this is the right default — a test knows which chain it is testing — but it
is not free.

**No default type parameter.** `NigiriClient::new()` without naming a network does not compile. A
default of `Bitcoin` was rejected: it would make the Liquid case the one that has to be spelled out,
and silent Bitcoin behaviour in Liquid code is the exact failure mode this design exists to prevent.

**The trait is closed.** A fork wanting to add a third chain edits this crate. Sealing is what makes
the parsing guarantees hold, and reopening it would trade a compile-time property for an extension
point nobody has asked for.

**Sealing does not extend to `Default`.** Which is where the design has one genuinely sharp edge.

## The edge: `NigiriConfig::default()` is Bitcoin-shaped

`NigiriConfig` is **not** generic over the network. It is one struct with concrete fields, and its
`Default` returns the Bitcoin configuration for every network.

Three fields are chain-dependent: `esplora_url`, `node_rpc_url`, and `electrum`. So this compiles,
runs, and is wrong:

```rust,ignore
let config = NigiriConfig {
    esplora_url: "http://localhost:30001".parse()?,
    node_rpc_url: "http://localhost:18884/".parse()?,
    ..Default::default()          // electrum silently keeps Bitcoin's port 50000
};
let client = NigiriClient::<Liquid>::with_config(config)?;
```

The type parameter cannot help here. Rust's struct-update syntax evaluates `default()` *before* any
override and before anything knows what `N` will be, so `Default` can never be made chain-aware. The
compile-time guarantee stops at the client and does not reach its configuration.

The mitigation is `NigiriConfig::bitcoin()` and `NigiriConfig::liquid()`, public constructors that
exist for exactly this reason rather than being documented advice. A test named
`naive_liquid_override_of_only_the_two_urls_keeps_bitcoins_electrum_port` pins the trap in
`crates/nigiri-rs-core/src/config.rs`.

Worth stating plainly, because it has bitten this project: **before adding a chain-dependent field to
`NigiriConfig`, check every `..Default::default()` site.** The last time one was added, a Liquid test
asserted a port the code never held and could not fail.

## What this buys

Three classes of bug become compile errors:

1. Passing a Bitcoin txid, block hash, or UTXO to a Liquid client, or the reverse.
2. Calling `mint` or `faucet_asset` on Bitcoin.
3. Using an address from the wrong network — caught at parse time, one boundary in, rather than
   surfacing as an opaque node RPC failure.

And one class of boilerplate disappears: responses arrive as native `bitcoin`/`elements` values, so a
consumer building on BDK or LWK hands them straight over instead of reparsing strings.

## Related

- [Client API reference](reference-client.md) — the full associated-type table and every method
- [How to point the client at services you run](how-to-point-at-your-own-services.md) — the
  `Default` trap in practice
- [How to work with Liquid assets](how-to-work-with-liquid-assets.md) — the chain-only methods
