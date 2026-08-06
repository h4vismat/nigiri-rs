# Client API reference

Everything `nigiri-rs-core` exports, re-exported in full by the `nigiri-rs` facade. Both import
paths work; the snippets here use the facade.

Errors are documented separately in [Errors](reference-errors.md). Every fallible method below
returns `Result<T, NigiriError>`.

## `NigiriClient<N>`

```rust
pub struct NigiriClient<N: NigiriNetwork> { /* private */ }
```

Derives `Debug` and `Clone`. Cloning copies the immutable configuration and shares the underlying
HTTP transport; it never implies ownership of an external process.

`N` is [`Bitcoin`](#bitcoin-and-liquid) or [`Liquid`](#bitcoin-and-liquid). There is no default type
parameter: `NigiriClient::new()` without naming a network does not compile.

### Construction

| Method | Signature | Notes |
| --- | --- | --- |
| `new` | `NigiriClient::<Bitcoin>::new() -> Self` | Bitcoin defaults. Panics only on an impossible static-config failure. |
| `new` | `NigiriClient::<Liquid>::new() -> Self` | Liquid defaults. |
| `default` | `Default::default() -> Self` | Implemented for both `NigiriClient<Bitcoin>` and `NigiriClient<Liquid>`; identical to `new()`. |
| `with_config` | `fn with_config(config: NigiriConfig) -> Result<Self, NigiriError>` | Validates and normalizes the config, then builds the HTTP client with `config.timeout` as its request timeout. |

```rust
use nigiri_rs::{Bitcoin, Liquid, NigiriClient};

let bitcoin = NigiriClient::<Bitcoin>::new();
let liquid = NigiriClient::<Liquid>::new();
```

`with_config` is the only constructor that can fail. It returns
`NigiriError::InvalidRequest` for a rejected configuration (see
[`validate_and_normalize`](#validation-rules)) and `NigiriError::HttpTransport` if the HTTP client
cannot be built.

### Endpoint accessors

| Method | Returns | Notes |
| --- | --- | --- |
| `esplora_url` | `&Url` | The normalized Esplora base URL, always with a trailing slash. |
| `electrum_endpoint` | `&ElectrumEndpoint` | Host and TCP port. A fixture reports its **runtime-mapped** port here, not the fixed container port. |

Read both from the client rather than assuming Nigiri's fixed ports. See
[How to point a wallet at a fixture](how-to-connect-a-wallet.md).

### Readiness and chain state

| Method | Signature |
| --- | --- |
| `wait_ready` | `async fn wait_ready(&self) -> Result<(), NigiriError>` |
| `block_height` | `async fn block_height(&self) -> Result<u64, NigiriError>` |
| `best_block_hash` | `async fn best_block_hash(&self) -> Result<N::BlockHash, NigiriError>` |
| `generate_to_address` | `async fn generate_to_address(&self, blocks: u64, address: &str) -> Result<Vec<N::BlockHash>, NigiriError>` |
| `invalidate_block` | `async fn invalidate_block(&self, hash: &N::BlockHash) -> Result<(), NigiriError>` |
| `reconsider_block` | `async fn reconsider_block(&self, hash: &N::BlockHash) -> Result<(), NigiriError>` |

`wait_ready` polls `block_height` every 100 ms and returns `Ok(())` on the first success. It gives up
after `config.timeout` with `NigiriError::Timeout { operation: "wait for readiness", .. }`. Failures
during the wait are swallowed, not returned — a service that is not up yet is retried.

`block_height` reads Esplora (`GET /blocks/tip/height`), not the node. `best_block_hash` reads the
node (`getbestblockhash`). During a sync lag the two disagree, which is exactly what
[fixture readiness](explanation-fixture-readiness.md) exists to rule out at startup.

`generate_to_address` rejects `blocks == 0` before any transport request, with
`NigiriError::InvalidRequest { detail: "block count must be greater than zero" }`.

### Addresses and funding

| Method | Signature |
| --- | --- |
| `new_address` | `async fn new_address(&self) -> Result<N::Address, NigiriError>` |
| `faucet` | `async fn faucet(&self, address: &str, amount: Option<bitcoin::Amount>) -> Result<N::Txid, NigiriError>` |

`new_address` calls `getnewaddress` on the node wallet and parses the result into the network's
native address type. A Bitcoin address is checked against `bitcoin::Network::Regtest`; a Liquid
address is checked against `elements::AddressParams::ELEMENTS`. An address for the wrong network is
rejected with `NigiriError::InvalidResponse`.

`faucet` sends through the node wallet (`sendtoaddress`), then mines **exactly one block** to confirm
it. `None` sends exactly 1 BTC (`bitcoin::Amount::ONE_BTC`); that default is chosen by this crate,
not by the service.

The two steps are not atomic. If the send commits and the mining then fails, `faucet` returns
`NigiriError::PostTransactionMiningFailed` carrying the committed transaction ID. Inspect node state
before retrying.

Amounts are serialized as exact decimal BTC through `Amount::to_string_in(Denomination::Bitcoin)` and
`serde_json::Number`; no value passes through `f64`.

### Queries

| Method | Signature |
| --- | --- |
| `get_utxos` | `async fn get_utxos(&self, address: &str) -> Result<Vec<N::Utxo>, NigiriError>` |
| `has_funds` | `async fn has_funds(&self, address: &str) -> Result<bool, NigiriError>` |
| `get_address_info` | `async fn get_address_info(&self, address: &str) -> Result<N::AddressInfo, NigiriError>` |
| `get_tx` | `async fn get_tx(&self, txid: &N::Txid) -> Result<N::TxInfo, NigiriError>` |
| `get_tx_status` | `async fn get_tx_status(&self, txid: &N::Txid) -> Result<TxStatus<N::BlockHash>, NigiriError>` |

All five read Esplora. Address arguments stay `&str` rather than a native address type, which avoids
a conversion for LWK confidential addresses that are already strings.

`has_funds` is `get_utxos(...).map(|utxos| !utxos.is_empty())` — it costs the same request.

### Broadcast and confirmation

| Method | Signature |
| --- | --- |
| `broadcast_tx` | `async fn broadcast_tx(&self, transaction_hex: &str) -> Result<N::Txid, NigiriError>` |
| `wait_for_confirmation` | `async fn wait_for_confirmation(&self, txid: &N::Txid, timeout: Duration) -> Result<(), NigiriError>` |

`broadcast_tx` sends `sendrawtransaction` through the node, then mines exactly one block, with the
same non-atomic caveat and the same `PostTransactionMiningFailed` behaviour as `faucet`.

`wait_for_confirmation` polls `get_tx_status` every 500 ms until `confirmed` is true. It takes its
own `timeout` rather than using `config.timeout`. Unlike `wait_ready`, it **propagates** a query
error immediately rather than retrying: a failed status lookup ends the wait with that error.

### Arbitrary node RPC

```rust
pub async fn rpc<R, P>(&self, method: &str, params: P) -> Result<R, NigiriError>
where
    R: DeserializeOwned,
    P: Serialize,
```

The caller selects the response type. See [How to call any node RPC](how-to-call-any-node-rpc.md)
for parameter shapes and worked examples.

Method names are validated before any request: 1 to 64 bytes of ASCII letters, digits, and
underscores. Anything else is `NigiriError::InvalidRequest`. The bound exists because a
runtime-computed name reaches `NigiriError` and therefore caller logs.

Requests use JSON-RPC 1.0 with a constant id (`"nigiri-rs"`), which is safe while each POST carries
one request whose response is fully read before the next call.

### Liquid-only methods

These exist on `NigiriClient<Liquid>` and do not compile on `NigiriClient<Bitcoin>`. The boundary is
enforced by `compile_fail` doctests.

| Method | Signature |
| --- | --- |
| `mint` | `async fn mint(&self, address: &str, quantity: u64, name: &str, ticker: &str) -> Result<MintResponse, NigiriError>` |
| `faucet_asset` | `async fn faucet_asset(&self, address: &str, amount: bitcoin::Amount, asset: &elements::AssetId) -> Result<elements::Txid, NigiriError>` |

`mint` derives the asset ID from a JSON contract it builds itself (domain `nigiri-rs.invalid`,
precision 0, zeroed issuer pubkey), so identical inputs produce a different asset ID than Nigiri's
own `mint` command. It calls `issueasset` then `sendtoaddress`, and **those are not atomic**: if the
send fails after issuance, the asset exists anyway. Retrying can create a second asset.

Neither method mines. Follow them with `generate_to_address` if you need confirmation. See
[How to work with Liquid assets](how-to-work-with-liquid-assets.md).

## `NigiriConfig`

```rust
pub struct NigiriConfig {
    pub esplora_url: Url,
    pub node_rpc_url: Url,
    pub electrum: ElectrumEndpoint,
    pub node_rpc_user: String,
    pub node_rpc_password: String,
    pub timeout: Duration,
    pub max_response_bytes: usize,
}
```

Derives `Debug`, `Clone`, `PartialEq`, `Eq`. Credentials are deliberately visible in `Debug` output:
they are Nigiri's published regtest defaults, not production secrets.

### Fields

| Field | Type | Bitcoin default | Liquid default |
| --- | --- | --- | --- |
| `esplora_url` | `Url` | `http://localhost:30000/` | `http://localhost:30001/` |
| `node_rpc_url` | `Url` | `http://localhost:18443/` | `http://localhost:18884/` |
| `electrum` | `ElectrumEndpoint` | `localhost:50000` | `localhost:50001` |
| `node_rpc_user` | `String` | `admin1` | `admin1` |
| `node_rpc_password` | `String` | `123` | `123` |
| `timeout` | `Duration` | 30 s | 30 s |
| `max_response_bytes` | `usize` | 64 KiB | 64 KiB |

`node_rpc_url`'s path is preserved exactly, with no trailing slash added, so you can target a
wallet-specific Bitcoin Core endpoint such as `http://host:18443/wallet/name`. `esplora_url` is a
base URL and **is** normalized to end in `/`.

### Constructors

| Constructor | Returns |
| --- | --- |
| `NigiriConfig::bitcoin()` | Bitcoin defaults. |
| `NigiriConfig::liquid()` | Liquid defaults. |
| `NigiriConfig::default()` | The **Bitcoin** defaults, for every `N`. |

`Default` is Bitcoin-shaped regardless of the client's network. Three fields are chain-dependent —
`esplora_url`, `node_rpc_url`, and `electrum` — and Rust evaluates `default()` before applying any
struct-update override, so a Liquid config written as

```rust
# use nigiri_rs::NigiriConfig;
let wrong = NigiriConfig {
    esplora_url: "http://localhost:30001".parse().unwrap(),
    node_rpc_url: "http://localhost:18884/".parse().unwrap(),
    ..Default::default()   // electrum silently stays on Bitcoin's port 50000
};
```

keeps Bitcoin's Electrum port. Start from `NigiriConfig::liquid()` instead. This trap is pinned by a
test named `naive_liquid_override_of_only_the_two_urls_keeps_bitcoins_electrum_port` in
`crates/nigiri-rs-core/src/config.rs`.

### Validation rules

`with_config` applies these and returns `NigiriError::InvalidRequest` on any failure:

- `esplora_url` and `node_rpc_url` must use `http` or `https` and support relative paths.
- Neither may carry a query string or a fragment.
- `timeout` must be greater than zero.
- `max_response_bytes` must be greater than zero and at most `MAX_RESPONSE_BYTES_LIMIT`. The limit
  itself is accepted.

### Constants

| Constant | Value | Meaning |
| --- | --- | --- |
| `DEFAULT_MAX_RESPONSE_BYTES` | `64 * 1024` (64 KiB) | Default per-response retention ceiling. |
| `MAX_RESPONSE_BYTES_LIMIT` | `16 * 1024 * 1024` (16 MiB) | Largest accepted `max_response_bytes`. |

One limit covers every response body, node JSON-RPC and Esplora alike. A body past the limit is
rejected with `NigiriError::InvalidResponse` rather than buffered. Raise it for methods with large
results such as `listunspent`, `listtransactions`, or `getblock <hash> 2`.

The 30-second default timeout has no public constant.

## `ElectrumEndpoint`

```rust
pub struct ElectrumEndpoint { /* private */ }

impl ElectrumEndpoint {
    pub fn new(host: impl Into<String>, port: u16) -> Result<Self, NigiriError>;
    pub fn host(&self) -> &str;
    pub fn port(&self) -> u16;
}
```

Derives `Clone`, `Debug`, `Eq`, `PartialEq`. Kept separate from the HTTP endpoints because Electrum
is a raw TCP protocol: there is no scheme to normalize and no path to preserve, and callers need the
two parts individually to build whatever connection string their Electrum client expects.

`new` rejects an empty host and a zero port, both with `NigiriError::InvalidRequest`.

## Network markers

### `Bitcoin` and `Liquid`

```rust
pub enum Bitcoin {}
pub enum Liquid {}
```

Uninhabited markers. They exist only as type parameters and are never constructed.

### `NigiriNetwork`

```rust
pub trait NigiriNetwork: private::Sealed {
    type Txid: Display;
    type BlockHash: Display + DeserializeOwned;
    type Address: Display;
    type Utxo;
    type TxInfo;
    type AddressInfo;
}
```

**Sealed.** You cannot implement it for your own type; the parsing contracts live in a private
supertrait.

| Associated type | `Bitcoin` | `Liquid` |
| --- | --- | --- |
| `Txid` | `bitcoin::Txid` | `elements::Txid` |
| `BlockHash` | `bitcoin::BlockHash` | `elements::BlockHash` |
| `Address` | `bitcoin::Address<NetworkChecked>` | `elements::Address` |
| `Utxo` | `BitcoinUtxo` | `LiquidUtxo` |
| `TxInfo` | `BitcoinTxInfo` | `LiquidTxInfo` |
| `AddressInfo` | `BitcoinAddressInfo` | `LiquidAddressInfo` |

Why a sealed trait rather than an enum: [Typed networks](explanation-typed-networks.md).

## `Peg`

```rust
pub struct Peg { /* private */ }
```

Derives `Debug` and `Clone`. Holds both clients **by value** — `NigiriClient` is `Clone` and cheap,
immutable configuration plus a shared transport — so no lifetime is threaded through the signatures
below. A `Peg` knows nothing about what started the two nodes: one assembled over services you run
and one taken from a [`PegPair`](reference-fixtures.md#pegpair) are the same type.

**Peg-in is real. Peg-out is half real.** `peg_in_request` returns a genuine
federation-controlled address and `claim_peg_in` submits a real `claimpegin` with a real merkle
proof, so your own claim path is exercised. `send_to_mainchain` is a genuine Elements call that burns
L-BTC — but nothing services it, because regtest has no federation, and `release_peg_out` plays that
part as a simulation. Read [Peg-out has no reserve](#peg-out-has-no-reserve) before asserting on
supply.

### Construction

| Method | Signature |
| --- | --- |
| `connect` | `async fn connect(bitcoin: NigiriClient<Bitcoin>, liquid: NigiriClient<Liquid>) -> Result<Self, NigiriError>` |

```rust
use nigiri_rs::{Bitcoin, Liquid, NigiriClient, Peg};

# async fn example() -> Result<(), nigiri_rs::NigiriError> {
let peg = Peg::connect(
    NigiriClient::<Bitcoin>::new(),
    NigiriClient::<Liquid>::new(),
)
.await?;
# let _ = peg;
# Ok(())
# }
```

There is deliberately no infallible constructor. `connect` reads the Liquid node's
`getsidechaininfo` and compares its reported `parent_blockhash` against the Bitcoin node's
`getblockhash 0`. A mismatch is `NigiriError::PegNotConfigured`, whose detail names both hashes. The
reported `pegin_confirmation_depth` is cached at the same time, which is why the accessor for it
below costs no round trip.

#### What `connect` proves, and what it does not

**A successful `connect` does not prove the two nodes can talk to each other.** Bitcoin's regtest
genesis is a hardcoded chain parameter — the same value on every node, never generated per instance —
and `liquidregtest` carries that same value as its parent. Two fixtures that have never heard of each
other therefore agree on the parent chain, and `connect` accepts them. That is measured, not
inferred: `crates/nigiri-rs-testcontainers/tests/peg_wiring.rs` starts two independent fixtures
against a real daemon and asserts `connect` succeeds.

What the comparison does catch is a Liquid node built for a **different** parent chain — one
carrying testnet or mainnet parameters, pointed at a regtest `bitcoind`. Nothing more.
[`PegPair`](reference-fixtures.md#pegpair) is what guarantees the pair is wired: it starts
`elementsd` with `-validatepegin=1` aimed at its own `bitcoind`. On a `Peg` you assembled yourself,
the first real evidence of wiring is a `claimpegin` that succeeds.

### Accessors

| Method | Signature | Returns |
| --- | --- | --- |
| `bitcoin` | `fn bitcoin(&self) -> &NigiriClient<Bitcoin>` | The Bitcoin side of the pair. |
| `liquid` | `fn liquid(&self) -> &NigiriClient<Liquid>` | The Liquid side. |
| `pegin_confirmation_depth` | `fn pegin_confirmation_depth(&self) -> u64` | Confirmations a deposit needs before it can be claimed, as the sidechain reported them to `connect`. |

All three are synchronous, `#[must_use]`, and free — no request is made. `pegin_confirmation_depth`
is 8 on both pinned Elements images; read it rather than hardcoding that, and see
[the mining note](#complete_peg_in-mines-and-how-many-blocks-is-not-fixed) for why reaching it is
necessary but not sufficient.

### Peg-in

| Method | Signature |
| --- | --- |
| `peg_in_request` | `async fn peg_in_request(&self) -> Result<PegInRequest, NigiriError>` |
| `claim_peg_in` | `async fn claim_peg_in(&self, mainchain_txid: &bitcoin::Txid) -> Result<elements::Txid, NigiriError>` |
| `complete_peg_in` | `async fn complete_peg_in(&self, amount: bitcoin::Amount) -> Result<PegIn, NigiriError>` |

`peg_in_request` calls `getpeginaddress`. It takes no destination: the address is derived from the
Liquid node's own wallet, and the eventual claim credits that wallet. Moving pegged funds anywhere
else is an ordinary transfer afterwards, not part of the peg. The returned Bitcoin address is
network-checked against regtest; anything else is `NigiriError::InvalidResponse`.

`claim_peg_in` fetches the deposit and its merkle proof from the Bitcoin node (`getrawtransaction`,
then `gettxoutproof`) and submits `claimpegin` to the Liquid node, returning the minting Liquid
transaction ID. The claim script is left out of the call: Elements infers it when the claiming wallet
issued the address, which it did. If the deposit has fewer confirmations than
`pegin_confirmation_depth`, nothing is submitted and you get
`NigiriError::PegInImmature { have, need }` — both numbers, rather than a node error string.

`complete_peg_in` runs the whole sequence and returns a `PegIn`: request an address, fund it from the
Bitcoin wallet, mine it to maturity, claim it. It takes no address for the same reason
`peg_in_request` does not.

#### `complete_peg_in` mines, and how many blocks is not fixed

A test that asserts on Bitcoin block height after a peg-in has to expect this, and cannot expect an
exact number.

1. `faucet` sends the deposit and mines **exactly one** confirming block.
2. `pegin_confirmation_depth() - 1` further blocks are mined, reaching the reported depth. The depth
   is read from the node, not hardcoded, so a chain with a lowered `peginconfirmationdepth` and a
   production-shaped one both work.
3. `claimpegin` is submitted. If it fails in a way another block could plausibly fix, **one more
   block is mined and it is submitted again — up to twenty extra blocks.**

Step 3 exists because the Liquid node's view of the mainchain lags the mainchain itself. Against a
real Elements node reporting a depth of 8, a claim was rejected at exactly 8 and only accepted at 11.
Retrying a block at a time adapts to however far behind the node is; a fixed margin would bake in a
number measured once, on one machine, against one image.

So one peg-in mines at least `pegin_confirmation_depth()` blocks and at most
`pegin_confirmation_depth() + 20`. Assert `>=`, never `==`.

Only `NigiriError::PegInImmature` and `NigiriError::RpcFailed` are treated as retryable; the node's
rejection of a premature claim arrives as the latter. Any other error returns immediately rather than
spending twenty blocks on something mining cannot fix. The decision is made on the error variant, not
on the node's message text, which carries no compatibility promise. If the twenty blocks run out, the
last retryable error is returned.

The deposit and its merkle proof are fetched once, before the loop: once mature, neither can change,
so every retry resubmits the same pair instead of asking the Bitcoin node again.

### Peg-out

| Method | Signature |
| --- | --- |
| `send_to_mainchain` | `async fn send_to_mainchain(&self, destination: &str, amount: bitcoin::Amount) -> Result<elements::Txid, NigiriError>` |
| `release_peg_out` | `async fn release_peg_out(&self, liquid_txid: &elements::Txid) -> Result<PegOut, NigiriError>` |

`send_to_mainchain` is a genuine Elements `sendtomainchain`: it burns L-BTC and records the Bitcoin
destination in an output of the resulting Liquid transaction. Nothing releases the BTC on its own —
follow it with `release_peg_out`.

`destination` is `&str` rather than `bitcoin::Address` so a caller can deliberately supply a
malformed one and exercise the failure path; every address argument in this crate is `&str` for that
reason. The destination and the amount are treated as sensitive arguments and redacted from error
bodies (see [Errors](reference-errors.md)).

**No peg-out wallet setup is needed, and none is possible.** `initpegoutwallet` is deliberately not
wrapped: this chain runs with PAK enforcement off, so the node rejects the call outright — there is
no PAK entry to register — and `sendtomainchain` does not require one.

`release_peg_out` reads the named Liquid transaction back with `getrawtransaction`, scans its outputs
for the peg-out, decodes the destination and value out of it, and pays that destination on Bitcoin
through `faucet`. **The destination comes out of the transaction, not from an argument**, so a
consumer that encodes it wrongly gets no payout, exactly as on liquidv1.

| Outcome | Result |
| --- | --- |
| A peg-out for this pair, decoded | `Ok(PegOut)` |
| Its destination script is not a standard address, or its value is missing or unreadable | `NigiriError::PegOutputMalformed` |
| Every peg-out-shaped output names a different parent chain | `NigiriError::PegOutputMalformed`, detail naming both chains |
| No peg-out-shaped output at all | `NigiriError::PegOutputNotFound` |

A wrong-chain output is skipped rather than failing the scan, since a genuine peg-out for this pair
may follow it; the mismatch is reported only if nothing better is found. A same-chain output that
cannot be read is reported immediately — moving past it would hide a real problem with a real
peg-out.

Like `faucet`, the release mines exactly one confirming block.

#### Peg-out has no reserve

**`release_peg_out` is a simulation.** The BTC it releases comes from the Bitcoin node's own wallet,
not from a locked reserve, and it is not the BTC anyone pegged in. Total BTC on the mainchain side
**grows with every release**, and **no 1:1 invariant holds across the pair**. Do not write a test
that asserts one.

The Liquid half stays honest — `sendtomainchain` genuinely burned the L-BTC, so the Liquid side moves
the way it would on liquidv1. It is the mainchain side that is fictional. What you can verify here is
your own encoding and your own claim handling, not the federation's behaviour.

### Peg records

All three derive `Debug`, `Clone`, `PartialEq`, `Eq`, and every field is public. Unlike the
[response records](#response-records) below, none of them is `Deserialize`: each is assembled by this
crate from more than one call rather than parsed out of one response.

```rust
pub struct PegInRequest {
    pub mainchain_address: bitcoin::Address,
    pub claim_script: String,
}

pub struct PegIn {
    pub mainchain_txid: bitcoin::Txid,
    pub claim_txid: elements::Txid,
    pub amount: bitcoin::Amount,
}

pub struct PegOut {
    pub liquid_txid: elements::Txid,
    pub destination: bitcoin::Address,
    pub amount: bitcoin::Amount,
    pub bitcoin_txid: bitcoin::Txid,
}
```

| Field | What it holds |
| --- | --- |
| `PegInRequest::mainchain_address` | The Bitcoin address to fund. Federation-controlled, tweaked by the Liquid wallet's keys. Network-checked, unlike `BitcoinAddressInfo::address`. |
| `PegInRequest::claim_script` | Hex claim script, retained for callers that submit their own claim. `claim_peg_in` does not pass it. |
| `PegIn::mainchain_txid` | The Bitcoin deposit that funded the peg-in address. |
| `PegIn::claim_txid` | The Liquid transaction that minted the L-BTC. |
| `PegIn::amount` | The amount deposited. **L-BTC minted equals this minus network fees**, so it is not the wallet's balance change. |
| `PegOut::liquid_txid` | The Liquid transaction that burned the L-BTC — the argument you passed in. |
| `PegOut::destination` | The destination decoded out of the peg-out output, not supplied by you. |
| `PegOut::amount` | The value decoded out of the peg-out output. |
| `PegOut::bitcoin_txid` | The Bitcoin transaction **this crate** sent to simulate the release. Not a federation transaction. |

## Response records

All derive `Debug`, `Clone`, `PartialEq`, `Eq`, `Deserialize`. Every field is public.

### `TxStatus<H>`

```rust
pub struct TxStatus<H> {
    pub confirmed: bool,
    pub block_height: Option<u64>,
    pub block_hash: Option<H>,
    pub block_time: Option<u64>,
}
```

`H` is the network's `BlockHash`. The three optional fields default to `None` when absent.

### `BitcoinUtxo` / `LiquidUtxo`

```rust
pub struct BitcoinUtxo {
    pub txid: bitcoin::Txid,
    pub vout: u32,
    pub value: bitcoin::Amount,
    pub status: TxStatus<bitcoin::BlockHash>,
}

pub struct LiquidUtxo {
    pub txid: elements::Txid,
    pub vout: u32,
    pub value: Option<bitcoin::Amount>,
    pub asset: Option<elements::AssetId>,
    pub value_commitment: Option<PedersenCommitment>,   // JSON: "valuecommitment"
    pub asset_commitment: Option<Generator>,            // JSON: "assetcommitment"
    pub status: TxStatus<elements::BlockHash>,
}
```

`BitcoinUtxo::value` is always present. `LiquidUtxo::value` and `asset` are `Option` because a
confidential output publishes commitments instead — exactly one pair is populated per output.

`value` fields deserialize from satoshi integers into `bitcoin::Amount`.

### `BitcoinTxInfo` / `LiquidTxInfo`

```rust
pub struct BitcoinTxInfo {
    pub txid: bitcoin::Txid,
    pub size: u64,
    pub weight: u64,
    pub fee: bitcoin::Amount,
    pub status: TxStatus<bitcoin::BlockHash>,
}
```

`LiquidTxInfo` is identical with `elements::Txid` and `elements::BlockHash`. Both are a typed
**subset** of the Esplora transaction response, not the whole thing.

### `BitcoinAddressInfo` / `LiquidAddressInfo`

```rust
pub struct BitcoinAddressInfo {
    pub address: bitcoin::Address<NetworkUnchecked>,
    pub chain_stats: AddressStats,
    pub mempool_stats: AddressStats,
}

pub struct LiquidAddressInfo {
    pub address: elements::Address,
    pub chain_stats: LiquidAddressStats,
    pub mempool_stats: LiquidAddressStats,
}
```

Note the Bitcoin address here is `NetworkUnchecked`, unlike `new_address`, which returns a checked
one.

### `AddressStats` and `LiquidAddressStats`

```rust
pub struct AddressStats {
    pub tx_count: u64,
    pub funded_txo_count: u64,
    pub funded_txo_sum: bitcoin::Amount,
    pub spent_txo_count: u64,
    pub spent_txo_sum: bitcoin::Amount,
}

pub struct LiquidAddressStats {
    pub tx_count: u64,
    pub funded_txo_count: u64,
    pub spent_txo_count: u64,
}
```

The Liquid form omits both sums: confidential values are unknown to the indexer.

**Known gap:** `LiquidAddressStats` is not re-exported from the crate root. You can read
`info.chain_stats.tx_count` through field access, but you cannot name the type in a signature or a
`let` binding. `AddressStats` is exported.

### `MintResponse` and `IssuanceTxIn`

```rust
pub struct MintResponse {
    pub asset: elements::AssetId,
    pub txid: elements::Txid,          // the transfer to your address
    pub issuance_txin: IssuanceTxIn,   // the separate issuing transaction
}

pub struct IssuanceTxIn {
    pub txid: elements::Txid,
    pub vin: u32,
}
```

`issuance_txin` is not optional: a successful issuance always has one.

## `LBTC_REGTEST_ASSET`

```rust
pub static LBTC_REGTEST_ASSET: LazyLock<elements::AssetId>;
```

The native Liquid regtest policy asset, `5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225`.

## `bitcoin_rpc_types` (feature `bitcoin-rpc-types`)

Re-exports the `corepc-types` 0.15 crate as `nigiri_rs::bitcoin_rpc_types`, giving you maintained
Bitcoin Core response records to deserialize `rpc()` results into:

```rust,ignore
let info: bitcoin_rpc_types::v31::GetBlockchainInfo = client.rpc("getblockchaininfo", ()).await?;
```

The module has to match the Core version serving the request. The pinned fixture image runs Bitcoin
Core v31.0, so `v31` is the module against a fixture; Nigiri v0.5.16 runs v30.0 and needs `v30`. A
custom environment on another Core version needs its own matching module.

There is no equivalent for Liquid: the `elements` crate supplies the native values, and callers own
their own RPC records.

## Scope limits

The peg is covered above under [`Peg`](#peg): peg-in is real, and peg-out's release is a simulation
that holds no reserve. `initpegoutwallet` is deliberately not wrapped — this chain rejects it, PAK
enforcement being off, and `sendtomainchain` does not need it. Federation lifecycle, chain
configuration, and cross-chain orchestration stay with the host application.

## Related

- [How to point the client at services you run](how-to-point-at-your-own-services.md)
- [Errors](reference-errors.md)
- [Fixture API](reference-fixtures.md)
- [Typed networks](explanation-typed-networks.md)
