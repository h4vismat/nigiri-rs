# How to work with Liquid assets

Mint an asset, send it, and read the results — including what confidential outputs do and do not tell
you.

Both methods below exist **only** on `NigiriClient<Liquid>`. Calling them on a Bitcoin client is a
compile error, enforced by `compile_fail` doctests.

## Prerequisites

A `NigiriClient<Liquid>`, from a fixture or from a running Elements node. Plus:

```toml
[dev-dependencies]
nigiri-rs = { version = "0.5", features = ["testcontainers"] }
bitcoin = "0.32"     # for Amount
elements = "0.25"    # for AssetId
```

The dependency family is aligned with LWK 0.18.1: `elements 0.25.3` and compatible `bitcoin 0.32.x`.
`nigiri-rs` does not depend on LWK itself.

## Mint an asset

```rust,ignore
use nigiri_rs::{Liquid, NigiriClient};

# async fn example(client: &NigiriClient<Liquid>) -> Result<(), Box<dyn std::error::Error>> {
let address = client.new_address().await?;

let minted = client
    .mint(&address.to_string(), 1_000, "NigiriRsTest", "NRT")
    .await?;

println!("asset:     {}", minted.asset);           // elements::AssetId
println!("transfer:  {}", minted.txid);            // the send to your address
println!("issuance:  {}:{}", minted.issuance_txin.txid, minted.issuance_txin.vin);
# Ok(())
# }
```

Signature: `mint(&self, address: &str, quantity: u64, name: &str, ticker: &str) -> Result<MintResponse, NigiriError>`.

`quantity` is a plain count, not an amount — the asset contract sets precision 0.

`MintResponse` carries three things: the `asset` ID, the `txid` of the transfer to your address, and
`issuance_txin`, the input of the **separate** transaction that issued the asset. Two transactions,
not one.

### The asset ID will not match Nigiri's

`mint` derives the asset ID from a JSON contract it builds itself — domain `nigiri-rs.invalid`,
precision 0, a zeroed issuer pubkey, plus your name and ticker. Nigiri's own `mint` command builds a
different contract, so identical inputs produce a **different** asset ID. That is intentional, not a
bug: the ID is a hash of the contract, and the contracts differ.

### It is not atomic

`mint` calls `issueasset` and then `sendtoaddress`. If the send fails after issuance, **the asset
still exists**. The error you get back is the send's.

Inspect node state before retrying — a blind retry issues a second asset with a different ID.

```rust,ignore
match client.mint(&address, 1_000, "Test", "TST").await {
    Ok(minted) => { /* ... */ }
    Err(error) => {
        // The asset may exist even though this failed. Check `listissuances`
        // before calling mint again.
        eprintln!("mint failed: {error}");
    }
}
```

### Verification

```rust,ignore
assert_eq!(minted.issuance_txin.txid.to_string().len(), 64);
```

## Send an existing asset

```rust,ignore
use bitcoin::Amount;

# async fn example(client: &nigiri_rs::NigiriClient<nigiri_rs::Liquid>, asset: &elements::AssetId, destination: &str) -> Result<(), Box<dyn std::error::Error>> {
let txid = client
    .faucet_asset(destination, Amount::ONE_BTC, asset)
    .await?;
# let _ = txid;
# Ok(())
# }
```

Signature: `faucet_asset(&self, address: &str, amount: bitcoin::Amount, asset: &elements::AssetId) -> Result<elements::Txid, NigiriError>`.

Note the type mismatch that is not a mistake: `mint` takes a `u64` count, `faucet_asset` takes a
`bitcoin::Amount`. `Amount` is the crate-wide monetary type, serialized as exact decimal BTC, never
through `f64`.

### `faucet_asset` does not mine

Unlike `faucet` and `broadcast_tx`, which each mine one confirming block, `faucet_asset` and `mint`
leave the transaction in the mempool. Confirm it yourself:

```rust,ignore
use std::time::Duration;

let txid = client.faucet_asset(&destination, Amount::ONE_BTC, &minted.asset).await?;
client.generate_to_address(1, &destination).await?;
client.wait_for_confirmation(&txid, Duration::from_secs(30)).await?;
```

## Send L-BTC

The policy asset needs no minting. `faucet` sends it, and mines a block:

```rust,ignore
use bitcoin::Amount;

let address = client.new_address().await?;
let txid = client
    .faucet(&address.to_string(), Some(Amount::from_sat(50_000)))
    .await?;
```

Its asset ID is available as a constant if you need to compare against it:

```rust,ignore
use nigiri_rs::LBTC_REGTEST_ASSET;

assert_eq!(*LBTC_REGTEST_ASSET, utxo.asset.unwrap());
```

Value `5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225`.

## Read confidential UTXOs

`LiquidUtxo` differs from `BitcoinUtxo` in the fields that matter here:

```rust,ignore
pub struct LiquidUtxo {
    pub txid: elements::Txid,
    pub vout: u32,
    pub value: Option<bitcoin::Amount>,     // None when confidential
    pub asset: Option<elements::AssetId>,   // None when confidential
    pub value_commitment: Option<PedersenCommitment>,
    pub asset_commitment: Option<Generator>,
    pub status: TxStatus<elements::BlockHash>,
}
```

Exactly one pair is populated per output: either the explicit `value`/`asset`, or the
`value_commitment`/`asset_commitment` pair. A confidential output publishes commitments; the indexer
cannot see through them, so it reports no amount.

```rust,ignore
for utxo in client.get_utxos(&address.to_string()).await? {
    match (utxo.value, utxo.asset) {
        (Some(value), Some(asset)) => println!("explicit: {value} of {asset}"),
        _ => println!("confidential: {:?}", utxo.value_commitment),
    }
}
```

**Do not assert on a confidential amount.** `utxo.value.unwrap()` panics on a blinded output. If you
need the amount, unblind it with the wallet that owns the output — that is LWK's job, not this
crate's.

Address statistics are affected the same way. `LiquidAddressStats` has `tx_count`,
`funded_txo_count`, and `spent_txo_count`, but **no sums**: the values are unknown to the indexer.
The Bitcoin `AddressStats` has `funded_txo_sum` and `spent_txo_sum`.

## Full worked example

```rust,ignore
use std::time::Duration;
use bitcoin::Amount;
use nigiri_rs::{Liquid, NigiriClient};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[nigiri_rs::test]
async fn mint_then_send(client: NigiriClient<Liquid>) -> Result<(), BoxError> {
    let destination = client.new_address().await?.to_string();

    // Mint. Two transactions: one issuance, one transfer.
    let minted = client.mint(&destination, 1_000, "NigiriRsTest", "NRT").await?;
    assert_eq!(minted.issuance_txin.txid.to_string().len(), 64);

    // Send some of it. Neither mint nor faucet_asset mines.
    let txid = client
        .faucet_asset(&destination, Amount::ONE_BTC, &minted.asset)
        .await?;
    client.generate_to_address(1, &destination).await?;
    client.wait_for_confirmation(&txid, Duration::from_secs(30)).await?;

    // The transfer is on chain.
    assert!(client.get_tx_status(&txid).await?.confirmed);
    Ok(())
}
```

## The peg lives on another page

Peg-in and the simulated peg-out release are not on `NigiriClient` at all — they need both chains at
once, so they live on [`Peg`](reference-client.md#peg). See
[How to peg in and peg out](how-to-peg.md) for both flows, and
[What the peg simulates](explanation-what-the-peg-simulates.md) for which half is real and what you
may assert about it.

## What is deliberately not here

`initpegoutwallet` remains unwrapped: PAK enforcement is off on this chain, so the node rejects the
call outright, and `sendtomainchain` does not need it.

The crate models only what the verified default Liquid network can actually execute. Federation
lifecycle, chain configuration, and cross-chain orchestration stay with your application.

If you need `initpegoutwallet` on a custom environment that does enable PAK, reach it through
[`rpc()`](how-to-call-any-node-rpc.md).

## Troubleshooting

**`Nigiri RPC issueasset failed with code -8: Invalid asset amount`** — `quantity` was zero or
otherwise rejected. Nothing was issued; `mint` returns before attempting the send.

**`Nigiri RPC sendtoaddress failed with code -6: Insufficient funds`** after a mint — the issuance
succeeded, the transfer did not. The asset exists. Check `listissuances` before retrying.

**`utxo.value` is `None`** — the output is confidential. Expected, not an error. Use the commitment
fields or unblind through a wallet.

**Zero L-BTC balance against a host-owned Nigiri** — Nigiri's Liquid node does not connect its genesis
outputs, so its wallet starts empty. A fixture does, and holds the full 21,000,000 L-BTC. Same chain,
same genesis; only the fixture is funded.

## Related

- [Client API reference](reference-client.md) — `MintResponse`, `LiquidUtxo`, `LiquidAddressStats`
- [How to call any node RPC](how-to-call-any-node-rpc.md) — anything not wrapped here
- [Typed networks](explanation-typed-networks.md) — why these two methods do not exist on Bitcoin
