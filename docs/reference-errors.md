# Error reference

Three `#[non_exhaustive]` error enums. `NigiriError` is the Bitcoin/Liquid boundary, `LndError` is
the Lightning boundary, and `FixtureError` is lifecycle/bootstrap and can retain either protocol
error as a source. None is closed, so a downstream match needs a wildcard arm.

All implement `std::error::Error` through `thiserror`. Where a variant has a
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
    AssetTransferFailed { asset: elements::AssetId, issuance_txin: IssuanceTxIn, source: Box<NigiriError> },
    PegOutputNotFound { liquid_txid: String },
    PegOutputMalformed { liquid_txid: String, detail: String },
    PegInImmature { have: u64, need: u64 },
    PegNotConfigured { detail: Cow<'static, str> },
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

A non-success Esplora status, or an unsuccessful RPC HTTP status without a valid RPC error
envelope. This includes a success-shaped RPC envelope delivered with HTTP 503. The retained
body is bounded and sensitive arguments are redacted. Malformed RPC envelopes may instead
produce `InvalidResponse`; no unsuccessful HTTP response becomes a successful RPC.

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
Both adapters use this classification for sending and reading response bodies. Polling budgets
also include in-flight requests and sleeps:

| `operation` | Bound by |
| --- | --- |
| an RPC method name or Esplora operation label | `config.timeout` |
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

- A successful HTTP response lacked a valid JSON-RPC envelope: missing fields, duplicate fields,
  a missing/mismatched ID, or simultaneous non-null result and error. An explicit `null` result
  remains valid for a unit-returning call.
- The result did not deserialize into the requested type.
- A txid, block hash, or address did not parse, or was for the wrong network.
- The body exceeded `max_response_bytes` (`detail: "response body exceeded the configured safety
  limit"`). Reading stops as soon as the limit is crossed.

**Response content is deliberately omitted** from deserialization failures, so a mismatched type does
not leak the payload into your logs.

### `PostTransactionMiningFailed`

> `{operation} committed transaction {txid}, but confirmation mining failed`

Reports a **partial success**. `faucet` and `broadcast_tx` each commit a
transaction and then mine one block; if the commit succeeds and the mining fails, this carries the
committed transaction ID and the underlying mining error as its source.

The transaction is on the node. It is simply not confirmed. Do not retry blindly — inspect node state
first, or the retry sends a second transaction.

### `AssetTransferFailed`

`mint` completed issuance, but the subsequent transfer failed. `asset` is the native
`elements::AssetId`; `issuance_txin` retains its `elements::Txid` and input index (`vin`).
`source` is the transfer error, available through `Error::source()`.

Use these identifiers to inspect and recover the existing issuance. Repeating `mint` issues
another asset. A transfer timeout can still mean the transfer committed, so inspect node state
before resending it. An `issueasset` failure returns its original error without this wrapper.

### `PegOutputNotFound`

> `no peg-out output in Liquid transaction {liquid_txid}`

`Peg::release_peg_out` scanned every output of the named Liquid transaction and found none shaped
like a peg-out at all.

### `PegOutputMalformed`

> `malformed peg-out output in Liquid transaction {liquid_txid}: {detail}`

A peg-out-shaped output was present but not usable for this pair — a wrong-chain output decodes
fine, it just names another pair, so this is not that case. `detail` names the specific problem: a
destination script that is not a standard address, an output with no explicit value, a value that
does not parse as an amount, a missing/wrong explicit asset, multiple same-parent peg-out outputs,
or (when every peg-out-shaped output named a different parent chain)
which chain it named instead.

### `PegInImmature`

> `peg-in deposit has {have} confirmations, needs {need}`

`Peg::claim_peg_in` checked the deposit's confirmation count against the sidechain's reported
`pegin_confirmation_depth` before submitting `claimpegin`, rather than letting the node reject the
claim. `Peg::complete_peg_in` also returns immediately if its initial deposit lookup is immature;
its submission retry loop only retries the pinned Elements maturity rejection described in
[the client reference](reference-client.md#complete_peg_in-mines-and-how-many-blocks-is-not-fixed).

### `PegNotConfigured`

> `peg is not configured: {detail}`

`Peg::connect` compared the Liquid node's `getsidechaininfo` parent block hash against the Bitcoin
node's genesis and they did not match, so the Elements node was built for a different parent chain
than this `bitcoind` serves.

It does **not** mean the two were never wired together. That comparison cannot tell a wired pair from
two unrelated nodes in either direction: regtest's genesis is a hardcoded chain parameter, so
independent nodes agree on it, and a genuinely wired pair still mismatches if its Elements node
carries other chain parameters. See
[What `connect` proves](reference-client.md#what-connect-proves-and-what-it-does-not).

## `LndError`

```rust
#[non_exhaustive]
pub enum LndError {
    InvalidRequest { detail: Cow<'static, str> },
    CredentialRead { path: PathBuf, source: io::Error },
    Transport { operation: Cow<'static, str>, detail: Cow<'static, str>, source: Box<dyn Error + Send + Sync> },
    Authentication { operation: Cow<'static, str>, detail: Cow<'static, str> },
    Status { operation: Cow<'static, str>, code: LndStatusCode, detail: Cow<'static, str> },
    Timeout { operation: Cow<'static, str>, duration: Duration },
    InvalidResponse { operation: Cow<'static, str>, detail: Cow<'static, str>, identifier: Option<String> },
    PaymentFailed { payment_hash: sha256::Hash, reason: Cow<'static, str> },
    OutcomeUnknown { operation: Cow<'static, str>, identifier: Option<String> },
}
```

`Debug` is manual and contains only the safe `Display` text. Operation labels and daemon text are
bounded; raw metadata, macaroons, certificate bodies, wallet passwords, seed words, invoices, and
payment preimages are never included.

### `InvalidRequest`

Caller input was rejected before an RPC: an invalid HTTPS endpoint, empty/oversized credential,
zero timeout, invalid peer address or channel/invoice/payment request, checked amount overflow,
sub-satoshi conversion, missing BOLT11 amount, non-whole-second duration, or value beyond LND's
signed request range. Public response-record constructors also return this variant for invalid
network identities, peer addresses, inconsistent balances, or invalid payment proofs.

### `CredentialRead`

A configured certificate or macaroon file could not be opened/read. The path is retained and the
`io::Error` is the source; file content is not retained. An over-limit file is `InvalidRequest`.

### `Transport`

TLS/gRPC connection or stream transport failed. The source chain is preserved with credential
metadata removed. The configured certificate is an exact end-entity pin, so a changed certificate
is expected to fail here. A transport-source status after a mutation is dispatched is instead
`OutcomeUnknown` because the server may already have committed it.

### `Authentication`

LND rejected the macaroon (`Unauthenticated` or `PermissionDenied`). It is separate from general
status failures so callers can stop rather than retry credentials blindly.

### `Status`

LND returned another gRPC status. The operation, owned `LndStatusCode`, and bounded diagnostic
`detail` are retained. Match `code` for policy decisions; `detail` is display text. Code conversion
occurs once at the adapter boundary and exposes no Tonic types.

`LndStatusCode` is a `Clone + Copy + Debug + Eq + PartialEq`, non-exhaustive enum with variants
`Ok`, `Cancelled`, `Unknown`, `InvalidArgument`, `DeadlineExceeded`, `NotFound`, `AlreadyExists`,
`PermissionDenied`, `ResourceExhausted`, `FailedPrecondition`, `Aborted`, `OutOfRange`,
`Unimplemented`, `Internal`, `Unavailable`, `DataLoss`, and `Unauthenticated`. Authentication codes
from the LND adapter normally become `Authentication` instead.

The fixture retries only `Unavailable`, `DeadlineExceeded`, `ResourceExhausted`, `Aborted`, and
`Unknown` status codes during readiness. Wallet initialization additionally requires the operation
to be `generate wallet seed`, so the committing `InitWallet` phase is never retried. After a mutation is
dispatched, `Cancelled`, `Unknown`, `DeadlineExceeded`, `ResourceExhausted`, `Internal`, and
`Unavailable` are classified as `OutcomeUnknown`; authentication, validation, and precondition
failures remain definitive.

### `Timeout`

The operation exceeded `LndConfig::timeout`. This means the response was not observed, not that LND
canceled or rolled back work. Operations that may have committed remap the error to
`OutcomeUnknown` when appropriate.

### `InvalidResponse`

LND returned a response that cannot satisfy the public domain model: malformed identifiers,
negative/inconsistent amounts, wrong invoice/payment hash, changed channel point, missing terminal
stream state, or contradictory terminal updates. `identifier` preserves a safe channel point,
transaction ID, or payment hash when available.

### `PaymentFailed`

LND reported a terminal failed payment. The payment hash and bounded normalized reason are present.
This is distinct from uncertainty. `LndPair` retries its reverse startup probe only when this reason
is exactly `no route` or `insufficient balance`, after revalidating the channel and with a fresh
invoice; no other application payment is retried by the client.

### `OutcomeUnknown`

LND may have committed the operation, but the final state was not observed. At the mutation boundary,
this includes a local timeout, a transport-source status, and `Cancelled`, `Unknown`,
`DeadlineExceeded`, `ResourceExhausted`, `Internal`, or `Unavailable`. Authentication, validation,
and precondition failures remain definitive. `identifier` carries a known channel point or payment
hash. For payments, call `lookup_payment` by hash before retrying. Invoice creation may be uncertain
without an identifier, and wallet initialization is uncertain after `InitWallet` because blindly
generating a different seed could conflict with the committed wallet.

## `FixtureError`

```rust
#[non_exhaustive]
pub enum FixtureError {
    InvalidConfiguration { detail: String },
    Runtime { operation: String, resource: String, diagnostics: String, source: Box<dyn Error + Send + Sync> },
    Bootstrap { chain: &'static str, operation: &'static str, diagnostics: String, source: ... },
    Probe { service: &'static str, operation: &'static str, diagnostics: String, source: ... },
    ReadinessTimeout { service: &'static str, duration: Duration, last_observation: String, diagnostics: String },
    Client(NigiriError),
    Lightning(LndError),
}
```

Service/resource labels include `bitcoind`, `elements`, `electrs`, `fixture`, `peg`, `lnd-alice`,
`lnd-bob`, `lightning-channel`, `LND pair`, and `fixture cleanup` according to the failed boundary.
`chain` is `Bitcoin`, `Liquid`, or `Lightning`.

`diagnostics` carries bounded container output where the failure happened inside a container — that
field is why a readiness failure is usually diagnosable from the error text alone.

### `InvalidConfiguration`

> `invalid fixture configuration: {detail}`

Rejected **before Docker is asked to start anything**: an empty image name/tag, malformed digest,
blank entrypoint, zero/unrepresentable startup timeout, or invalid LND allocation/range arithmetic.

### `Runtime`

> `container runtime {operation} failed for {resource}: {diagnostics}`

One runtime shape covers daemon connection, image, network, container, port, bounded file-read, log,
and cleanup failures. `operation` and `resource` locate the step; bounded redacted diagnostics and
the underlying source retain details. An unavailable Docker daemon is represented here, not by a
separate public variant.

### `Bootstrap`

> `{chain} wallet bootstrap failed during {operation}: {diagnostics}`

Wallet/bootstrap or composite protocol work failed. For Bitcoin/Liquid, operations include wallet
creation/funding. For LND, `chain` is `Lightning`, diagnostics are bounded and redacted, and the
source chain contains `FixtureError::Lightning(LndError)` so typed classification is preserved.
Only transient `GenSeed` failures retry; `InitWallet` and later committed operations do not.

### `Probe`

> `{service} {operation} probe failed: {diagnostics}`

A readiness probe failed in a way that is not a timeout, for example Electrum
`blockchain.headers.subscribe` or an LND/Bitcoin observation that is classified as permanent.

### `ReadinessTimeout`

> `{service} was not ready after {duration:?}: {last_observation}; {diagnostics}`

The shared startup budget expired at a readiness boundary. The `service`, `last_observation`, and
bounded diagnostics identify that boundary: Docker connection or creation, container startup,
RPC/bootstrap, Electrum, LND, funding, cleanup, or a composite check. The variant has **no source** —
nothing necessarily failed; the budget expired.

For the final single-chain agreement check, `last_observation` is the final height reading, formatted
`node=<n> esplora=<n> electrum=<n>`, which tells you *which* service was behind, and `service` is
`"fixture"`.

On a [`PegPair`](reference-fixtures.md#pegpair), the same variant also covers the budget running out
while verifying the pair, after all four containers are already up. There `service` is `"peg"` and
`last_observation` is prose rather than the height triplet — `verifying both chains report the same
parent` — since there is only the one check, not three services to compare.

Bumping `startup_timeout` is the fix when this happens on a first run that is still pulling images.

For `LndPair`, the duration is the original whole-call budget (180 seconds by default), and the last
observation names `lnd-alice`, `lnd-bob`, or `lightning-channel` state. The same deadline bounds
failure diagnostics and cleanup. Exhausted public cleanup can detach from its supervisor thread;
reverse-order cleanup continues there without extending the public call beyond the configured
deadline.

### `Client`

> transparent

A `NigiriError` from the fixture's own client, wrapped via `#[from]`. Its `Display` is the inner
error's, unchanged.

### `Lightning`

> `Lightning client failed: {source}`

An `LndError` retained as the source. Direct conversions use this tuple variant. During LND startup,
the outer `Bootstrap` adds the operation and redacted container diagnostics while preserving this
typed inner source.

## Related

- [Client API](reference-client.md)
- [Fixture API](reference-fixtures.md)
- [What "ready" means](explanation-fixture-readiness.md) — why `ReadinessTimeout` exists
