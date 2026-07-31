# Migrating nigiri-rs

## From 0.2.x to 0.3.0

Version 0.3.0 adds a public, type-directed `NigiriClient::rpc<R>()` escape hatch for Bitcoin and Liquid. This deliberately reverses the 0.2 policy that arbitrary RPC was unavailable.

Existing curated methods remain supported and source-compatible. Use them when their stronger native return contracts cover the required operation.

Enable `bitcoin-rpc-types` to use the optional `corepc-types` re-export. Nigiri v0.5.16 runs Bitcoin Core v30.0, so its verified response module is `nigiri_rs::bitcoin_rpc_types::v30`.

RPC arguments are separate CLI-style strings. The method never accepts one combined command string and never invokes a shell. The method name may be computed at runtime. Arbitrary methods can mutate wallet and chain state; lifecycle and test synchronization remain host-owned.

### `NigiriConfig` gained a required field

`NigiriConfig` now carries `max_rpc_response_bytes`, so struct-literal construction must supply it. Existing code that built the config by literal needs one added line:

```rust
use nigiri_rs::{DEFAULT_MAX_RPC_RESPONSE_BYTES, NigiriConfig};

let config = NigiriConfig {
    chopsticks_url: "http://localhost:3000".parse()?,
    esplora_url: "http://localhost:30000".parse()?,
    executable: "nigiri".into(),
    timeout: std::time::Duration::from_secs(30),
    max_rpc_response_bytes: DEFAULT_MAX_RPC_RESPONSE_BYTES,
};
```

`NigiriClient::new()` is unaffected. Raise the value above the 64 KiB default when calling `rpc()` with methods whose results are large, such as `listunspent`, `listtransactions`, or `getblock <hash> 2`; anything past the limit is rejected rather than buffered.

### `NigiriError` labels are now `Cow<'static, str>`

`operation` and `method` on every `NigiriError` variant changed from `&'static str` to `Cow<'static, str>` so that a runtime-determined RPC method name is reported accurately. Struct patterns matching a label against a literal need a guard:

```rust
// Before
matches!(error, NigiriError::InvalidResponse { operation: "configuration", .. })
// After
matches!(error, NigiriError::InvalidResponse { ref operation, .. } if operation.as_ref() == "configuration")
```

Crate-owned labels stay borrowed, so this allocates nothing for the curated methods.

### `NigiriError::InvalidRequest` replaces two synthetic labels

Input rejected before any Nigiri process is spawned now has its own variant instead of
masquerading as an unusable response. Two cases moved:

| Was | Now |
| --- | --- |
| `InvalidResponse { operation: "configuration", detail }` | `InvalidRequest { detail }` |
| `InvalidResponse { operation: "RPC method validation", detail }` | `InvalidRequest { detail }` |

Nothing produced a response in either case, and callers previously had to string-match a
pseudo-operation to tell them apart from a genuine node failure:

```rust
// Before
matches!(error, NigiriError::InvalidResponse { ref operation, .. } if operation.as_ref() == "configuration")
// After
matches!(error, NigiriError::InvalidRequest { .. })
```

### `max_rpc_response_bytes` is bounded from above

Values above `MAX_RPC_RESPONSE_BYTES_LIMIT` (16 MiB) are now rejected by
`NigiriClient::with_config`. Formatting a failed RPC costs a multiple of the retention
ceiling, so an unbounded value read from a config file or environment variable could turn one
RPC failure into an out-of-memory abort. 16 MiB is far above any Bitcoin Core or Elements
regtest response.

### Process boundary hardening

The CLI executor drains stdout and stderr concurrently under the configured timeout, so a
child that fills one pipe can no longer deadlock the read. Breaching the retention limit or
the timeout kills and reaps the spawned child, subject to the process-group limit noted
below. Retained stderr is bounded, has ANSI escape sequences stripped, and preserves UTF-8
through lossy decoding.

Caller arguments are redacted from retained stderr on every code path. Each invocation
builder declares where its caller-supplied values begin, so the non-RPC subcommands
(`mint`, `faucet_asset`) redact their address, quantity, and asset arguments the same way
`rpc()` redacts its own. Redaction anchors on a 16-byte prefix and extends over however
much of the argument the CLI actually echoed, so an elided value (`Invalid descriptor
"wpkh(cQr..."`) is covered too. It remains textual matching: a CLI that re-encodes an
argument, or echoes only its tail, can still surface that form.

Killing a timed-out or over-limit child signals that one process only. `nigiri` runs the
`docker` CLI as its own child, which survives as an orphan. Independently of that, an RPC
already dispatched to the node completes inside the container: killing a client does not
cancel work the node has accepted. A mutating RPC that times out may therefore still commit,
and recovery remains host-owned.

### RPC failure detection changed

An `error code:` marker is now recognized only at the start of a line. A successful response
whose content happens to contain that phrase mid-line is no longer misreported as a failure.
Stderr containing only whitespace no longer fails a void result.

A method that exits zero, writes nothing to stdout, and writes real content to stderr is
still reported as `NigiriError::RpcFailed`, because that is how the node CLIs surface some
errors. Keep the host `nigiri` wrapper's stderr free of unrelated noise, or void RPCs will
report spurious failures.

## From 0.1.x to 0.2.0

Version 0.2.0 intentionally breaks the Liquid-only, stringly typed API.

## Select a network explicitly

```rust
use nigiri_rs::{Bitcoin, Liquid, NigiriClient};

let bitcoin = NigiriClient::<Bitcoin>::new();
let liquid = NigiriClient::<Liquid>::new();
```

The non-generic `NigiriClient::new()` constructor was removed and there is no default network parameter.

## Replace `with_urls`

`with_urls` was removed. Construct a complete `NigiriConfig` containing typed URLs, the Nigiri executable path, and the operation timeout, then call `NigiriClient::<Network>::with_config(config)`.

Unlike `new()`, `with_config` returns `Result` because custom values are validated and normalized.

## Replace strings and floating-point amounts

- Funding and broadcast methods return `bitcoin::Txid` or `elements::Txid`.
- Mining and reorg methods use native `bitcoin::BlockHash` or `elements::BlockHash`.
- `new_address` returns a checked Bitcoin regtest address or an Elements regtest address.
- Faucet amounts are `Option<bitcoin::Amount>`, not `Option<f64>`.
- Esplora methods return typed records rather than `serde_json::Value` or unchecked strings.

Transaction lookups now take references to native transaction identifiers:

```rust,no_run
# use nigiri_rs::{Bitcoin, NigiriClient};
# async fn example(txid: bitcoin::Txid) -> Result<(), nigiri_rs::NigiriError> {
let client = NigiriClient::<Bitcoin>::new();
let status = client.get_tx_status(&txid).await?;
# let _ = status;
# Ok(())
# }
```

## Liquid-only methods

The former synchronous `cli_mint` and `cli_faucet_asset` helpers were removed. Use asynchronous `mint` and `faucet_asset` on `NigiriClient<Liquid>`.

## RPC and lifecycle boundaries

In version 0.2.0, there was no public raw RPC method. Public wrappers parsed their exact expected response into native types.

The host must start and stop Nigiri. Version 0.2.0 does not start containers, detect-and-skip a missing host, delete datadirs, or clean up Nigiri from `Drop`.
