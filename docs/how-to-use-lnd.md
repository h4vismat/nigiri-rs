# How to use a host-managed LND node

Connect to an LND daemon you operate without enabling Docker fixtures or depending on generated RPC
types.

## Enable the client

```toml
[dependencies]
nigiri-rs = { version = "0.5", features = ["lnd"] }
```

The `lnd` feature does not enable `fixtures`, Bollard, or lifecycle management. It exposes the
project-owned Lightning API from `nigiri-rs-lnd` through the facade. To provision a test topology,
enable `lightning-fixtures` for `nigiri_rs::fixtures::LndPair`; `fixtures` alone contains the
Bitcoin/Liquid fixtures. Direct users of `nigiri-rs-fixtures` enable its `lnd` feature.

## Load credentials and wait for readiness

```rust,no_run
use std::time::Duration;
use nigiri_rs::{LndClient, LndConfig};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let config = LndConfig::from_files(
    "https://127.0.0.1:10009",
    "tls.cert",
    "admin.macaroon",
    Duration::from_secs(30),
)
.await?;
let client = LndClient::with_config(config)?;
let info = client.wait_ready().await?;

println!("LND {} at height {}", info.alias(), info.block_height());
# Ok(())
# }
```

Construction validates synchronously and is lazy: it does not contact LND. `wait_ready()` performs
one bounded authenticated `GetInfo` and returns its `NodeInfo`; it does not poll for chain or graph
synchronization. Check `network()`, `synced_to_chain()`, and `synced_to_graph()` if your application
requires those stronger states.

`LndConfig::new` accepts credential bytes instead. Both constructors enforce an HTTPS endpoint with
a host and explicit port, no userinfo/query/fragment, a nonzero timeout, a certificate no larger
than `MAX_TLS_CERTIFICATE_BYTES` (1 MiB), and a macaroon no larger than `MAX_MACAROON_BYTES` (64 KiB).
An explicit `https://localhost:443` is accepted. For a new wallet, `initialize_wallet` accepts
`LndBootstrapConfig` with a parsed HTTPS `Url`, certificate bytes, and timeout. Bootstrap also
accepts port 443 after URL normalization removes the explicit port. It validates TLS settings
independently of authentication and uses the returned admin macaroon for the authenticated config.

## Understand the certificate pin

The configured PEM certificate is converted to one exact end-entity DER pin. The client still uses
rustls handshake-signature verification, but it does not treat that certificate as a general CA or
trust an alternative certificate for the same hostname. This matches LND's self-signed certificate
model and prevents ambient platform roots from widening trust. Rotate the config when LND rotates
its certificate.

The macaroon authorizes every operation this client performs. Treat it as a secret: restrict file
permissions, provision the narrowest macaroon permissions your application needs, never embed its
bytes in source or diagnostics, and do not send it to an endpoint whose certificate you did not
verify. `Debug` for configs, clients, errors, invoices, and payments deliberately omits credential
bytes, certificate bodies, payment preimages, and raw invoices, but that does not secure copies made
by your own code.

## Retry committed operations safely

Each request is bounded by `LndConfig::timeout`; `PaymentOptions` also supplies LND's payment timeout.
A timeout is not cancellation or rollback. Mutating calls such as `open_channel`, `create_invoice`,
and `pay_invoice` can return `LndError::OutcomeUnknown` when LND may have committed state;
`lookup_payment` can return it when the terminal payment state cannot be observed. The error
preserves a channel point or payment hash when known from the request or daemon.

For a payment, query by hash before retrying:

```rust,ignore
match client.pay_invoice(invoice, options).await {
    Ok(payment) => use_payment(payment),
    Err(nigiri_rs::LndError::OutcomeUnknown {
        identifier: Some(hash),
        ..
    }) => {
        // Parse the retained hash in application code, then call `lookup_payment`.
        // Retry the invoice only after the lookup proves no payment was committed.
        eprintln!("query payment {hash} before retrying");
    }
    Err(error) => return Err(error.into()),
}
```

`PaymentFailed` is different: LND reported a terminal failure and the error includes the payment
hash plus a bounded reason. `InvalidResponse` means the daemon returned data that could not satisfy
the public domain model. See [Error reference](reference-errors.md) for every variant.

## Classify errors and build test adapters

Match `LndError::Status { code, .. }` using the owned `LndStatusCode` enum, rather than parsing the
human-readable `detail`. The adapter translates transport codes at its boundary, so your retry
policy does not need a Tonic dependency. Select retries according to the operation as well as the
code; an uncertain committed mutation requires reconciliation first.

Implement `LightningNode` for application test doubles or alternative adapters. Its response
records have public validated constructors: `NodeInfo::try_new`, `WalletBalance::try_new`,
`Peer::try_new`, `Channel::try_new`, and `PaymentRecord::try_new` return `Result<_, LndError>`.
`InvoiceRecord::from_invoice(invoice, state)` derives the hash and amount from a parsed BOLT11
invoice, rejecting an invoice without an amount. Payment construction checks any supplied preimage
against the hash and requires a preimage for success. See the [response record reference](reference-client.md#response-records-and-states)
for constructor arguments and balance invariants.

## Related

- [Client API reference](reference-client.md#lightning-client-api)
- [Tutorial: settle a Lightning payment](tutorial-lightning-payment.md)
- [Lifecycle ownership](explanation-lifecycle-ownership.md)
