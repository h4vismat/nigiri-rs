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
let info: bitcoin_rpc_types::v30::GetBlockchainInfo = client.rpc("getblockchaininfo", ()).await?;
```

Nigiri v0.5.16 and the pinned fixture image both run Bitcoin Core v30.0, so `v30` is the module to
use. A custom environment on another Core version needs the matching module.

There is no equivalent for Liquid: the `elements` crate supplies the native values, and callers own
their own RPC records.

## Scope limits

Nigiri v0.5.16's default Liquid regtest configuration does not enable PAK enforcement, so
`initpegoutwallet` and `sendtomainchain` fail there and are deliberately not wrapped. Peg-in and
peg-out behaviour is not simulated. Federation lifecycle, chain configuration, and cross-chain
orchestration stay with the host application.

## Related

- [How to point the client at services you run](how-to-point-at-your-own-services.md)
- [Errors](reference-errors.md)
- [Fixture API](reference-fixtures.md)
- [Typed networks](explanation-typed-networks.md)
