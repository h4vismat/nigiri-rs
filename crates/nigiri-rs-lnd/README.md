# nigiri-rs-lnd

Host-managed [LND](https://github.com/lightningnetwork/lnd) client primitives for
`nigiri-rs`. This crate connects to an LND process you manage; it does not start
Docker containers or own service lifecycle.

The public API covers wallet initialization and unlocking, readiness checks,
peer and channel management, invoices, payments, and payment or invoice lookup.
Use `nigiri-rs-fixtures` when a test should provision and own a complete local
two-node Lightning topology.

```toml
[dependencies]
nigiri-rs-lnd = "0.1"
```

TLS uses an exact pin of the supplied end-entity certificate and still verifies
rustls signatures. Treat the certificate, macaroon, wallet password, and seed as
secrets: load credentials from protected files or memory, never log them, and
discard seed material as soon as wallet initialization completes.

The checked-in private protobuf surface is pinned to LND `v0.21.1-beta`, commit
`2b87887`; provenance is recorded in `proto/PROVENANCE.md`.

- [Connect to host-managed LND](https://github.com/h4vismat/nigiri-rs/blob/master/docs/how-to-use-lnd.md)
- [Client API reference](https://github.com/h4vismat/nigiri-rs/blob/master/docs/reference-client.md)
- [Error reference](https://github.com/h4vismat/nigiri-rs/blob/master/docs/reference-errors.md)
