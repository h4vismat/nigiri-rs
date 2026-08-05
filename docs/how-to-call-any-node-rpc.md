# How to call any node RPC

Reach node methods the curated API does not wrap, with a response type you choose.

```rust
pub async fn rpc<R, P>(&self, method: &str, params: P) -> Result<R, NigiriError>
where
    R: DeserializeOwned,
    P: Serialize,
```

Available on both `NigiriClient<Bitcoin>` and `NigiriClient<Liquid>`.

**Prefer a curated method when one exists.** `new_address`, `best_block_hash`,
`generate_to_address`, `invalidate_block`, and `reconsider_block` all guarantee method-specific
native contracts — the right address network, the right hash type — that `rpc()` cannot.

## Prerequisites

A working client. See [How to point the client at services you run](how-to-point-at-your-own-services.md)
or [How to run a throwaway regtest stack](how-to-run-a-fixture.md).

## Basic calls

```rust,no_run
use nigiri_rs::{Bitcoin, NigiriClient};

# async fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::new();

let height: u64 = client.rpc("getblockcount", ()).await?;
let hash: bitcoin::BlockHash = client.rpc("getblockhash", (100_u64,)).await?;
# let _ = (height, hash);
# Ok(())
# }
```

The turbofish is rarely needed — the binding's type annotation drives `R`.

## Parameter shapes

| Rust value | JSON sent |
| --- | --- |
| `()` | `[]` |
| `(a,)` | `[a]` |
| `(a, b, c)` | `[a, b, c]` |
| `[a, b]` | `[a, b]` |
| any `Serialize` struct | a JSON object (named parameters) |

Note the trailing comma in a one-element tuple: `(100_u64,)` is a tuple, `(100_u64)` is just a
number.

Named parameters via a struct:

```rust,no_run
use nigiri_rs::{Bitcoin, NigiriClient};
use serde::Serialize;

#[derive(Serialize)]
struct BlockTemplateOptions {
    rules: [&'static str; 1],
}

# async fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::new();
let _: serde_json::Value = client
    .rpc("getblocktemplate", BlockTemplateOptions { rules: ["segwit"] })
    .await?;
# Ok(())
# }
```

Mixed positional parameters are ordinary tuples:

```rust,no_run
# use nigiri_rs::{Bitcoin, NigiriClient};
# async fn example() -> Result<(), nigiri_rs::NigiriError> {
# let client = NigiriClient::<Bitcoin>::new();
let _: serde_json::Value = client
    .rpc("listunspent", (1_u64, 9_999_999_u64, ["bcrt1qexample"]))
    .await?;
# Ok(())
# }
```

### Types are not coerced

JSON-RPC sends what you give it. `"100"` is a JSON string and is **not** a substitute for the JSON
number `100`:

```rust,ignore
client.rpc::<u64, _>("getblockhash", ("100",)).await   // node rejects it
client.rpc::<u64, _>("getblockhash", (100_u64,)).await // correct
```

The former CLI transport coerced strings according to each method's argument schema. This one does
not.

## Choosing a response type

Pick a Rust shape that matches the method's JSON result.

**Native types** where the result is a single value:

```rust,no_run
# use nigiri_rs::{Bitcoin, NigiriClient};
# async fn example() -> Result<(), nigiri_rs::NigiriError> {
# let client = NigiriClient::<Bitcoin>::new();
let hash: bitcoin::BlockHash = client.rpc("getbestblockhash", ()).await?;
let count: u64 = client.rpc("getconnectioncount", ()).await?;
# let _ = (hash, count);
# Ok(())
# }
```

**Your own record** for a structured result — deserialize only the fields you need:

```rust,no_run
use nigiri_rs::{Liquid, NigiriClient};
use serde::Deserialize;

#[derive(Deserialize)]
struct BlockchainInfo {
    chain: String,
    blocks: u64,
    bestblockhash: elements::BlockHash,
}

# async fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Liquid>::new();
let info: BlockchainInfo = client.rpc("getblockchaininfo", ()).await?;
assert_eq!(info.chain, "liquidregtest");
# Ok(())
# }
```

**`serde_json::Value`** when the shape is unknown or you only want one field. Deliberate, not lazy —
it's the right call when exploring:

```rust,no_run
# use nigiri_rs::{Bitcoin, NigiriClient};
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
# let client = NigiriClient::<Bitcoin>::new();
let funded: serde_json::Value = client.rpc("fundrawtransaction", (raw_hex(),)).await?;
let hex = funded["hex"].as_str().ok_or("fundrawtransaction returned no hex")?;
# let _ = hex;
# Ok(())
# }
# fn raw_hex() -> String { String::new() }
```

**`()`** for methods that return null:

```rust,ignore
let _: () = client.rpc("invalidateblock", (hash.to_string(),)).await?;
```

**Maintained Bitcoin Core records** with the `bitcoin-rpc-types` feature:

```toml
nigiri-rs = { git = "...", features = ["bitcoin-rpc-types"] }
```

```rust,ignore
use nigiri_rs::bitcoin_rpc_types;

let info: bitcoin_rpc_types::v30::GetBlockchainInfo =
    client.rpc("getblockchaininfo", ()).await?;
```

Nigiri v0.5.16 and the pinned fixture image both run Bitcoin Core v30.0, so `v30` is the module. A
different Core version needs the matching module. There is no Liquid equivalent — the `elements`
crate supplies native values and you own the record.

## Method name rules

Validated before any transport request: 1 to 64 bytes of ASCII letters, digits, and underscores.
Names may be computed at runtime. Anything else is `NigiriError::InvalidRequest`.

The length bound exists because a runtime name is carried into `NigiriError` and therefore into your
logs.

## Raise the response limit for large results

Every response body is capped at `max_response_bytes`, 64 KiB by default, and a body past it is
rejected rather than buffered:

```rust
use nigiri_rs::{Bitcoin, DEFAULT_MAX_RESPONSE_BYTES, NigiriClient, NigiriConfig};

# fn example() -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
    max_response_bytes: 16 * DEFAULT_MAX_RESPONSE_BYTES,   // 1 MiB
    ..Default::default()
})?;
# let _ = client;
# Ok(())
# }
```

`listunspent`, `listtransactions`, and `getblock <hash> 2` are the usual offenders. Ceiling is 16 MiB.

## Mutating methods

`rpc()` will happily call methods that mutate wallets and chain state.

- **Against a fixture:** safe. Each one owns its chain; no other test can observe your reorg.
- **Against a shared node** (host-owned Nigiri, a CI host): you own the coordination and the
  restoration. Nothing in this crate serializes access.

Unlike `faucet` and `broadcast_tx`, `rpc()` does **not** mine a confirming block. Follow a mutating
call with `generate_to_address` if you need confirmation.

## Worked example: a signed Liquid transaction

Four `rpc()` calls where the curated API stops. The blinding step is the Liquid difference: a
confidential output cannot be signed until its value and asset are blinded.

```rust,ignore
use nigiri_rs::{Liquid, NigiriClient};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

async fn signed_wallet_transaction(
    client: &NigiriClient<Liquid>,
    destination: &str,
) -> Result<String, BoxError> {
    // Liquid's createrawtransaction takes an ARRAY of output objects.
    // Bitcoin's takes a single object.
    let outputs = serde_json::json!([{ destination: "0.00010000" }]);
    let raw: String = client
        .rpc("createrawtransaction", (serde_json::json!([]), outputs))
        .await?;

    let funded: serde_json::Value = client.rpc("fundrawtransaction", (raw,)).await?;
    let funded_hex = funded["hex"].as_str().ok_or("no hex")?;

    let blinded: String = client.rpc("blindrawtransaction", (funded_hex,)).await?;

    let signed: serde_json::Value =
        client.rpc("signrawtransactionwithwallet", (blinded,)).await?;
    if signed["complete"] != serde_json::Value::Bool(true) {
        return Err("wallet did not completely sign".into());
    }
    Ok(signed["hex"].as_str().ok_or("no hex")?.to_owned())
}
```

Broadcast it with the curated method, which mines a confirming block for you:

```rust,ignore
let txid = client.broadcast_tx(&signed).await?;
```

## Troubleshooting

**`Nigiri RPC <method> failed with code -8: ...`** — the node rejected it. `code` and `message` are
the node's own; check its documentation for that method.

**`invalid response during <method>: result did not match the requested type`** — `R` is wrong. The
response body is deliberately **omitted** from this error so a mismatch cannot leak the payload into
your logs. Re-run with `serde_json::Value` and print it to see the real shape.

**`invalid response during <method>: response body exceeded the configured safety limit`** — raise
`max_response_bytes`.

**`invalid request: RPC method must be 1 to 64 bytes of ASCII letters, digits, and underscores`** —
a hyphen, a space, or a newline in the method name.

**`invalid response during <method>: expected a JSON-RPC response envelope`** — a 2xx response whose
body was not a JSON-RPC envelope. Usually the URL points at something that isn't the node's RPC port.

## Related

- [Client API reference](reference-client.md) — the curated methods to prefer
- [How to work with Liquid assets](how-to-work-with-liquid-assets.md)
- [Errors](reference-errors.md)
