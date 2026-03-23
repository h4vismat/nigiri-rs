# nigiri-rs

A Rust client library for [Nigiri](https://github.com/vulpemventures/nigiri), providing an async API to interact with a local Liquid regtest environment. Designed for end-to-end testing of applications built on the [Liquid Network](https://liquid.net).

## Prerequisites

- [Nigiri](https://github.com/vulpemventures/nigiri) installed and running with the `--liquid` flag:

```sh
nigiri start --liquid
```

## Installation

Add to your `Cargo.toml`:

```toml
[dev-dependencies]
nigiri-rs = { git = "https://github.com/h4vismat/nigiri-rs" }
```

## Quick Start

```rust
use nigiri_rs::NigiriClient;

#[tokio::main]
async fn main() -> Result<(), nigiri_rs::NigiriError> {
    let client = NigiriClient::new();

    // Wait for Nigiri to be ready
    client.wait_ready().await?;

    // Send L-BTC to an address
    let txid = client.faucet("el1qq...", Some(1.0)).await?;
    println!("Faucet txid: {txid}");

    // Mint a custom Liquid asset
    let mint = client.mint("el1qq...", 1000, "MyToken", "MTK").await?;
    println!("Asset: {}, txid: {}", mint.asset, mint.txid);

    // Query UTXOs
    let utxos = client.get_utxos("el1qq...").await?;
    println!("UTXOs: {}", utxos.len());

    Ok(())
}
```

## API

### Client

```rust
// Default local ports (Chopsticks: 3001, Electrs: 30001)
let client = NigiriClient::new();

// Custom URLs
let client = NigiriClient::with_urls("http://host:3001", "http://host:30001");
```

### Faucet & Mint

| Method | Description |
|--------|-------------|
| `faucet(address, amount)` | Send L-BTC to an address (auto-mines a block) |
| `cli_faucet_asset(address, amount, asset_id)` | Send a specific asset via CLI |
| `mint(address, quantity, name, ticker)` | Mint a new Liquid asset |
| `cli_mint(address, quantity, name, ticker)` | Mint via CLI, returns `(asset_id, txid)` |

### Queries

| Method | Description |
|--------|-------------|
| `get_utxos(address)` | List UTXOs for an address |
| `has_funds(address)` | Check if an address has any UTXOs |
| `get_address_info(address)` | Address stats (tx count, funded/spent totals) |
| `get_tx(txid)` | Transaction details |
| `get_tx_status(txid)` | Confirmation status |
| `block_height()` | Current block height |

### Broadcasting & Confirmation

| Method | Description |
|--------|-------------|
| `broadcast_tx(tx_hex)` | Broadcast a raw transaction (auto-mines a block) |
| `wait_for_confirmation(txid, timeout)` | Poll until a transaction is confirmed |

### Utilities

| Method / Constant | Description |
|-------------------|-------------|
| `esplora_url()` | Returns the Esplora API base URL (useful with `EsploraClientBuilder`) |
| `LBTC_REGTEST_ASSET` | Policy asset ID for L-BTC on regtest |

## Architecture

The client talks to two Nigiri services:

- **Chopsticks** (`localhost:3001`) — state-changing operations: faucet, mint, broadcast. Automatically mines a block after each operation.
- **Electrs** (`localhost:30001`) — read-only Esplora REST API: UTXOs, transactions, blocks.

## Running Tests

```sh
nigiri start --liquid
cargo test
```

Tests skip gracefully if Nigiri is not running.

## License

MIT
