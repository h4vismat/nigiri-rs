# nigiri-rs-lnd

Host-managed [LND](https://github.com/lightningnetwork/lnd) client primitives for
`nigiri-rs`. This crate connects to an LND process you manage; it does not start
Docker containers or own service lifecycle.

The public API covers wallet initialization, readiness checks,
peer and channel management, invoices, payments, and payment or invoice lookup.
Use `nigiri-rs-fixtures` with its `lnd` feature when a test should provision and own a complete local
two-node Lightning topology. Through the `nigiri-rs` facade, enable `lightning-fixtures` for
`LndPair`; `fixtures` alone provides Bitcoin/Liquid fixtures without LND.

```toml
[dependencies]
nigiri-rs-lnd = "0.2"
```

TLS uses an exact pin of the supplied end-entity certificate and still verifies
rustls signatures. The certificate is public trust-anchor data: protect its integrity and avoid
logging the full PEM. The macaroon, wallet password, and seed are credentials: load them from
protected files or memory, never log them, and discard seed material as soon as wallet
initialization completes.

Wallet bootstrap validates TLS settings independently of authentication and reuses those settings
with LND's returned admin macaroon. `LndBootstrapConfig` accepts normalized HTTPS URLs using the
default port 443. `LndConfig::new` and `from_files` require an explicit port in their input string,
including when that port is 443.

`LightningNode` supports downstream test doubles and adapters without generated RPC types.
Construct response records with `NodeInfo::try_new`, `WalletBalance::try_new`, `Peer::try_new`,
`Channel::try_new`, `InvoiceRecord::from_invoice`, and `PaymentRecord::try_new`. These constructors
validate network, endpoint, balance, invoice, and payment-proof invariants.

`LndError::Status` includes an owned `LndStatusCode` in its `code` field. Match that code for retry
policies rather than parsing diagnostic text. `OutcomeUnknown` still requires reconciliation
before retrying a mutation that may have committed.

The checked-in private protobuf surface is pinned to LND `v0.21.1-beta`, commit
`2b87887`; provenance is recorded in [`proto/PROVENANCE.md`](proto/PROVENANCE.md).

- [Connect to host-managed LND](https://github.com/h4vismat/nigiri-rs/blob/master/docs/how-to-use-lnd.md)
- [Client API reference](https://github.com/h4vismat/nigiri-rs/blob/master/docs/reference-client.md)
- [Error reference](https://github.com/h4vismat/nigiri-rs/blob/master/docs/reference-errors.md)
