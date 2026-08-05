# Error reference

Two `#[non_exhaustive]` error enums. `NigiriError` comes from the client; `FixtureError` comes from
starting a fixture and wraps the former. Neither enum is closed: each grows variants as the crate
grows, so a downstream match must carry a wildcard arm (`_ => ...`) or it will not compile.

Both derive `Debug` and implement `std::error::Error` through `thiserror`. Where a variant has a
`source`, `Error::source()` returns it — always check the chain, the `Display` text is deliberately
short.

## `NigiriError`

```rust
#[non_exhaustive]
pub enum NigiriError {
    HttpTransport { operation: Cow<'static, str>, source: reqwest::Error },
    HttpStatus { operation: Cow<'static, str>, status: StatusCode, body: String },
    RpcFailed { method: Cow<'static, str>, code: i32, message: String },
    Timeout { operation: Cow<'static, str>, duration: Duration },
    InvalidRequest { detail: Cow<'static, str> },
    InvalidResponse { operation: Cow<'static, str>, detail: String },
    PostTransactionMiningFailed { operation: Cow<'static, str>, txid: String, source: Box<NigiriError> },
}
```

Labels are `Cow<'static, str>` so a runtime-determined RPC method name is reported accurately while
crate-owned labels stay borrowed and allocate nothing.

### `HttpTransport`

> `HTTP transport failed during {operation}`

The request never completed: connection refused, DNS failure, TLS failure, a broken stream
mid-response. Also returned by `with_config` if the HTTP client itself cannot be built
(`operation: "build HTTP client"`).

The underlying `reqwest::Error` is the source, with its URL stripped.

**Usually means:** the service is not running, or the URL is wrong.

### `HttpStatus`

> `HTTP status {status} during {operation}: {body}`

A non-success status whose body was **not** a JSON-RPC envelope — a proxy error page, a gateway
failure. The body is included, with any sensitive argument redacted.

A node that returns HTTP 500 *with* a proper JSON-RPC error envelope produces `RpcFailed` instead,
not this.

### `RpcFailed`

> `Nigiri RPC {method} failed with code {code}: {message}`

The node returned a JSON-RPC error envelope. `code` and `message` are the node's own — for example
`-8 Block height out of range`, or `-6 Insufficient funds`.

The message is redacted for sensitive arguments (addresses and amounts passed to `faucet`, raw
transaction hex passed to `broadcast_tx`).

**This is the variant to match on** when you expect a specific node-level failure.

### `Timeout`

> `{operation} timed out after {duration:?}`

Either a single HTTP operation exceeded `config.timeout`, or a polling loop exhausted its budget.
Three operations produce it:

| `operation` | Bound by |
| --- | --- |
| an RPC method name | `config.timeout` |
| `wait for readiness` | `config.timeout` |
| `wait for confirmation` | the `timeout` argument you passed |

**A timeout is not a rollback.** A mutating request may still have committed on the node. Inspect
node state before retrying.

### `InvalidRequest`

> `invalid request: {detail}`

Caller input rejected **before any request was sent**. Distinct from `InvalidResponse`, which means a
service returned something unusable.

Produced by:

- Configuration validation: a non-HTTP(S) URL, a URL with a query or fragment, a zero timeout, a zero
  or over-limit `max_response_bytes`.
- `ElectrumEndpoint::new`: an empty host or a zero port.
- `rpc()`: a method name that is empty, over 64 bytes, or contains anything outside ASCII letters,
  digits, and underscores.
- `generate_to_address(0, ..)`.
- An amount that cannot be represented as a JSON number.

### `InvalidResponse`

> `invalid response during {operation}: {detail}`

A service responded, but with something unusable:

- The body was not a JSON-RPC envelope and the status was a success.
- The result did not deserialize into the requested type.
- A txid, block hash, or address did not parse, or was for the wrong network.
- The body exceeded `max_response_bytes` (`detail: "response body exceeded the configured safety
  limit"`).

**Response content is deliberately omitted** from deserialization failures, so a mismatched type does
not leak the payload into your logs.

### `PostTransactionMiningFailed`

> `{operation} committed transaction {txid}, but confirmation mining failed`

The one variant that reports a **partial success**. `faucet` and `broadcast_tx` each commit a
transaction and then mine one block; if the commit succeeds and the mining fails, this carries the
committed transaction ID and the underlying mining error as its source.

The transaction is on the node. It is simply not confirmed. Do not retry blindly — inspect node state
first, or the retry sends a second transaction.

## `FixtureError`

```rust
#[non_exhaustive]
pub enum FixtureError {
    InvalidConfiguration { detail: String },
    RuntimeUnavailable { source: Box<dyn Error + Send + Sync> },
    ContainerStart { service: &'static str, image: String, diagnostics: String, source: ... },
    PortDiscovery { service: &'static str, container_port: u16, diagnostics: String, source: ... },
    Bootstrap { chain: &'static str, operation: &'static str, diagnostics: String, source: ... },
    Probe { service: &'static str, operation: &'static str, diagnostics: String, source: ... },
    ReadinessTimeout { service: &'static str, duration: Duration, last_observation: String, diagnostics: String },
    Client(NigiriError),
}
```

`service` is `"bitcoind"`, `"elements"`, or `"electrs"` — plus `"fixture"` on the three-way readiness
wait, where no single container is the one at fault. `chain` is `"Bitcoin"` or `"Liquid"`.

`diagnostics` carries bounded container output where the failure happened inside a container — that
field is why a readiness failure is usually diagnosable from the error text alone.

### `InvalidConfiguration`

> `invalid fixture configuration: {detail}`

Rejected **before Docker is asked to start anything**: an empty image name or tag, a malformed
digest, a blank image entrypoint, a zero startup timeout.

### `RuntimeUnavailable`

> `container runtime is unavailable`

The Docker daemon could not be reached. The `Display` text is stable and says nothing else on
purpose; the real cause is the source.

**This is what you get when Docker is not running.**

### `ContainerStart`

> `failed to start {service} from {image}: {diagnostics}`

The container was created but did not come up. `image` is the full descriptor including digest, which
matters when a pinned image has been replaced.

### `PortDiscovery`

> `failed to discover mapped {container_port} port for {service}: {diagnostics}`

The container is running but Docker did not report a host-side mapping for the port. Rare; usually a
daemon-level problem.

### `Bootstrap`

> `{chain} wallet bootstrap failed during {operation}: {diagnostics}`

Funding the wallet failed. `operation` names the step — `getnewaddress`, `generatetoaddress`,
`rescanblockchain`. The chain is named because a two-chain test run otherwise could not say which
stack failed.

### `Probe`

> `{service} {operation} probe failed: {diagnostics}`

A readiness probe failed in a way that is not a timeout, for example the Electrum
`blockchain.headers.subscribe` call erroring outright.

### `ReadinessTimeout`

> `{service} was not ready after {duration:?}: {last_observation}; {diagnostics}`

The startup budget ran out with the three services still disagreeing. `last_observation` is the final
height reading, formatted `node=<n> esplora=<n> electrum=<n>`, which tells you *which* service was
behind. On this path `service` is `"fixture"`, since the failure is the disagreement rather than any
one container. The variant has **no source** — nothing failed, the budget simply expired.

Bumping `startup_timeout` is the fix when this happens on a first run that is still pulling images.

### `Client`

> transparent

A `NigiriError` from the fixture's own client, wrapped via `#[from]`. Its `Display` is the inner
error's, unchanged.

## Related

- [Client API](reference-client.md)
- [Fixture API](reference-fixtures.md)
- [What "ready" means](explanation-fixture-readiness.md) — why `ReadinessTimeout` exists
