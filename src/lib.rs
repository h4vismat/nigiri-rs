//! Nigiri test helper — provides a client for interacting with a local
//! Nigiri Liquid regtest environment.
//!
//! # Prerequisites
//!
//! Nigiri must be running with the `--liquid` flag before executing E2E tests:
//!
//! ```bash
//! nigiri start --liquid
//! ```
//!
//! # Port layout
//!
//! | Service          | Port  | Used for                              |
//! |------------------|-------|---------------------------------------|
//! | Chopsticks       | 3001  | `/faucet`, `/mint` (Liquid extensions) |
//! | Electrs (API)    | 30001 | Esplora REST API (blocks, txs, UTXOs) |
//! | Esplora (UI)     | 5001  | Block explorer web UI (not used here) |
//!
//! Chopsticks also proxies the Esplora API at its root, but for wallet
//! sync we point `EsploraClientBuilder` directly at electrs on port 30001.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Chopsticks endpoint — faucet and mint only.
const DEFAULT_CHOPSTICKS_URL: &str = "http://localhost:3001";

/// Electrs Liquid API — Esplora REST queries (no `/api` prefix).
const DEFAULT_ESPLORA_URL: &str = "http://localhost:30001";

/// Policy asset for Liquid regtest (L-BTC).
pub const LBTC_REGTEST_ASSET: &str =
    "5ac9f65c0efcc4775e0baec4ec03abdde22473cd3cf33c0419ca290e0751b225";

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// HTTP client for a local Nigiri Liquid regtest environment.
///
/// Uses two endpoints:
/// - **Chopsticks** (`localhost:3001`) for `/faucet` and `/mint`
/// - **Electrs** (`localhost:30001`) for Esplora REST queries
#[derive(Clone)]
pub struct NigiriClient {
    /// Chopsticks URL (faucet, mint).
    chopsticks_url: String,
    /// Electrs Esplora API URL (blocks, txs, UTXOs).
    esplora_url: String,
    http: Client,
}

impl NigiriClient {
    /// Creates a new client with default local ports.
    pub fn new() -> Self {
        Self::with_urls(DEFAULT_CHOPSTICKS_URL, DEFAULT_ESPLORA_URL)
    }

    /// Creates a new client with custom URLs.
    pub fn with_urls(chopsticks_url: &str, esplora_url: &str) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            chopsticks_url: chopsticks_url.trim_end_matches('/').to_string(),
            esplora_url: esplora_url.trim_end_matches('/').to_string(),
            http,
        }
    }

    /// Returns the Esplora API URL for use with `EsploraClientBuilder`.
    ///
    /// Points to electrs on port 30001 (no `/api` prefix).
    pub fn esplora_url(&self) -> &str {
        &self.esplora_url
    }

    // -----------------------------------------------------------------------
    // Health / readiness
    // -----------------------------------------------------------------------

    /// Polls the block tip endpoint until Nigiri is responsive (up to 30s).
    pub async fn wait_ready(&self) -> Result<(), NigiriError> {
        let url = format!("{}/blocks/tip/height", self.esplora_url);
        for attempt in 0..30 {
            match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                _ => {
                    if attempt == 29 {
                        return Err(NigiriError::NotReady(
                            "Nigiri not responding after 30s — is `nigiri start --liquid` running?"
                                .into(),
                        ));
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        unreachable!()
    }

    /// Returns the current block height.
    pub async fn block_height(&self) -> Result<u64, NigiriError> {
        let url = format!("{}/blocks/tip/height", self.esplora_url);
        let resp = self.http.get(&url).send().await?.error_for_status()?;
        let height: u64 = resp
            .text()
            .await?
            .trim()
            .parse()
            .map_err(|e| NigiriError::Api(format!("Failed to parse block height: {}", e)))?;
        Ok(height)
    }

    // -----------------------------------------------------------------------
    // Faucet (Chopsticks)
    // -----------------------------------------------------------------------

    /// Sends L-BTC to the given address via Chopsticks faucet.
    ///
    /// Auto-mines a block. Returns the funding txid.
    pub async fn faucet(&self, address: &str, amount: Option<f64>) -> Result<String, NigiriError> {
        let url = format!("{}/faucet", self.chopsticks_url);
        let mut body = serde_json::json!({ "address": address });
        if let Some(amt) = amount {
            body["amount"] = serde_json::json!(amt);
        }
        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(NigiriError::Api(format!(
                "Faucet failed ({}): {}",
                status, text
            )));
        }
        // Chopsticks returns JSON like {"txId":"..."}
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let txid = parsed["txId"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| text.trim().trim_matches('"').to_string());
        Ok(txid)
    }

    // -----------------------------------------------------------------------
    // Mint (Chopsticks, Liquid-only)
    // -----------------------------------------------------------------------

    /// Mints a new Liquid asset and sends `quantity` units to `address`.
    pub async fn mint(
        &self,
        address: &str,
        quantity: u64,
        name: &str,
        ticker: &str,
    ) -> Result<MintResponse, NigiriError> {
        let url = format!("{}/mint", self.chopsticks_url);
        let body = serde_json::json!({
            "address": address,
            "quantity": quantity,
            "name": name,
            "ticker": ticker,
        });
        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(NigiriError::Api(format!(
                "Mint failed ({}): {}",
                status, text
            )));
        }
        let parsed: MintResponse = serde_json::from_str(&text).map_err(|e| {
            NigiriError::Api(format!(
                "Failed to parse mint response: {} — body: {}",
                e, text
            ))
        })?;
        Ok(parsed)
    }

    // -----------------------------------------------------------------------
    // UTXO / balance queries (Electrs Esplora API)
    // -----------------------------------------------------------------------

    /// Returns the list of UTXOs for an address.
    ///
    /// Note: On Liquid, confidential outputs have `valuecommitment`/`assetcommitment`
    /// instead of `value`/`asset`. Only unblinded outputs will have numeric values.
    pub async fn get_utxos(&self, address: &str) -> Result<Vec<Utxo>, NigiriError> {
        let url = format!("{}/address/{}/utxo", self.esplora_url, address);
        let resp = self.http.get(&url).send().await?.error_for_status()?;
        let utxos: Vec<Utxo> = resp.json().await?;
        Ok(utxos)
    }

    /// Returns true if the address has any UTXOs (confirmed or unconfirmed).
    pub async fn has_funds(&self, address: &str) -> Result<bool, NigiriError> {
        let utxos = self.get_utxos(address).await?;
        Ok(!utxos.is_empty())
    }

    /// Returns address stats (tx_count, funded/spent counts).
    pub async fn get_address_info(&self, address: &str) -> Result<serde_json::Value, NigiriError> {
        let url = format!("{}/address/{}", self.esplora_url, address);
        let resp = self.http.get(&url).send().await?.error_for_status()?;
        let info: serde_json::Value = resp.json().await?;
        Ok(info)
    }

    // -----------------------------------------------------------------------
    // Transaction queries (Electrs Esplora API)
    // -----------------------------------------------------------------------

    /// Returns transaction details by txid.
    pub async fn get_tx(&self, txid: &str) -> Result<TxInfo, NigiriError> {
        let url = format!("{}/tx/{}", self.esplora_url, txid);
        let resp = self.http.get(&url).send().await?.error_for_status()?;
        let info: TxInfo = resp.json().await?;
        Ok(info)
    }

    /// Returns the confirmation status of a transaction.
    pub async fn get_tx_status(&self, txid: &str) -> Result<TxStatus, NigiriError> {
        let url = format!("{}/tx/{}/status", self.esplora_url, txid);
        let resp = self.http.get(&url).send().await?.error_for_status()?;
        let status: TxStatus = resp.json().await?;
        Ok(status)
    }

    /// Broadcasts a raw transaction hex via Chopsticks (auto-mines a block).
    pub async fn broadcast_tx(&self, tx_hex: &str) -> Result<String, NigiriError> {
        let url = format!("{}/tx", self.chopsticks_url);
        let resp = self.http.post(&url).body(tx_hex.to_string()).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(NigiriError::Api(format!(
                "Broadcast failed ({}): {}",
                status, text
            )));
        }
        Ok(text.trim().trim_matches('"').to_string())
    }

    // -----------------------------------------------------------------------
    // Block helpers
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // CLI-based operations (for asset fauceting)
    // -----------------------------------------------------------------------

    /// Mints a new asset via the `nigiri mint` CLI command.
    /// Returns (asset_id, txid).
    pub fn cli_mint(
        address: &str,
        quantity: u64,
        name: &str,
        ticker: &str,
    ) -> Result<(String, String), NigiriError> {
        let output = std::process::Command::new("nigiri")
            .args(["mint", address, &quantity.to_string(), name, ticker])
            .output()
            .map_err(|e| NigiriError::Api(format!("Failed to run nigiri mint: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NigiriError::Api(format!("nigiri mint failed: {}", stderr)));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Parse output: look for "asset: <hex>" and "txId: <hex>"
        let asset = stdout
            .lines()
            .find(|l| l.starts_with("asset:"))
            .and_then(|l| l.strip_prefix("asset:"))
            .map(|s| s.trim().to_string())
            .ok_or_else(|| NigiriError::Api(format!("No asset in mint output: {}", stdout)))?;

        let txid = stdout
            .lines()
            .find(|l| l.starts_with("txId:"))
            .and_then(|l| l.strip_prefix("txId:"))
            .map(|s| s.trim().to_string())
            .ok_or_else(|| NigiriError::Api(format!("No txId in mint output: {}", stdout)))?;

        Ok((asset, txid))
    }

    /// Sends a specific Liquid asset to an address via `nigiri faucet --liquid`.
    /// Returns the txid.
    pub fn cli_faucet_asset(
        address: &str,
        amount: f64,
        asset_id: &str,
    ) -> Result<String, NigiriError> {
        let output = std::process::Command::new("nigiri")
            .args(["faucet", "--liquid", address, &amount.to_string(), asset_id])
            .output()
            .map_err(|e| NigiriError::Api(format!("Failed to run nigiri faucet: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NigiriError::Api(format!(
                "nigiri faucet failed: {}",
                stderr
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let txid = stdout
            .lines()
            .find(|l| l.starts_with("txId:"))
            .and_then(|l| l.strip_prefix("txId:"))
            .map(|s| s.trim().to_string())
            .ok_or_else(|| NigiriError::Api(format!("No txId in faucet output: {}", stdout)))?;

        Ok(txid)
    }

    /// Waits until a transaction is confirmed (up to `timeout`).
    pub async fn wait_for_confirmation(
        &self,
        txid: &str,
        timeout: Duration,
    ) -> Result<(), NigiriError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let status = self.get_tx_status(txid).await?;
            if status.confirmed {
                return Ok(());
            }
            if tokio::time::Instant::now() > deadline {
                return Err(NigiriError::Timeout(format!(
                    "Transaction {} not confirmed within {:?}",
                    txid, timeout
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Response from Chopsticks `/mint` endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MintResponse {
    pub asset: String,
    #[serde(alias = "txId")]
    pub txid: String,
    #[serde(default)]
    pub issuance_txin: Option<serde_json::Value>,
}

/// A single UTXO from the Esplora API.
///
/// On Liquid, confidential outputs use commitments instead of plain values.
/// `value` and `asset` are only present for unblinded outputs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Utxo {
    pub txid: String,
    pub vout: u32,
    #[serde(default)]
    pub value: Option<u64>,
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub valuecommitment: Option<String>,
    #[serde(default)]
    pub assetcommitment: Option<String>,
    pub status: TxStatus,
}

/// Transaction confirmation status.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TxStatus {
    pub confirmed: bool,
    #[serde(default)]
    pub block_height: Option<u64>,
    #[serde(default)]
    pub block_hash: Option<String>,
    #[serde(default)]
    pub block_time: Option<u64>,
}

/// Abbreviated transaction info from Esplora.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TxInfo {
    pub txid: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub weight: Option<u64>,
    #[serde(default)]
    pub fee: Option<u64>,
    pub status: TxStatus,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum NigiriError {
    #[error("Nigiri not ready: {0}")]
    NotReady(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API error: {0}")]
    Api(String),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Timeout: {0}")]
    Timeout(String),
}

// ---------------------------------------------------------------------------
// Smoke tests — run only when Nigiri is actually available
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn require_nigiri() -> NigiriClient {
        let client = NigiriClient::new();
        match client.wait_ready().await {
            Ok(()) => client,
            Err(_) => {
                eprintln!("Skipping: Nigiri not running (`nigiri start --liquid`)");
                std::process::exit(0);
            }
        }
    }

    #[tokio::test]
    async fn test_nigiri_health() {
        let nigiri = require_nigiri().await;
        let height = nigiri.block_height().await.unwrap();
        assert!(height > 0, "Block height should be > 0, got {}", height);
    }

    #[tokio::test]
    async fn test_faucet_and_utxos() {
        let nigiri = require_nigiri().await;

        // Get a fresh address from the Liquid node for testing
        let output = std::process::Command::new("nigiri")
            .args(["rpc", "--liquid", "getnewaddress"])
            .output()
            .expect("Failed to run nigiri rpc");
        let address = String::from_utf8(output.stdout).unwrap().trim().to_string();

        let txid = nigiri.faucet(&address, Some(0.001)).await.unwrap();
        assert!(!txid.is_empty(), "Faucet should return a txid");

        // Wait for electrs to index the new block
        let mut utxos = vec![];
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            utxos = nigiri.get_utxos(&address).await.unwrap();
            if !utxos.is_empty() {
                break;
            }
        }
        assert!(!utxos.is_empty(), "Address should have UTXOs after faucet");

        // On Liquid, faucet outputs are confidential — they have commitments, not plain values
        let has_commitment = utxos.iter().any(|u| u.valuecommitment.is_some());
        let has_plain = utxos.iter().any(|u| u.value.is_some());
        assert!(
            has_commitment || has_plain,
            "UTXOs should have either plain values or commitments"
        );
    }

    #[tokio::test]
    async fn test_mint_asset() {
        let nigiri = require_nigiri().await;

        let output = std::process::Command::new("nigiri")
            .args(["rpc", "--liquid", "getnewaddress"])
            .output()
            .expect("Failed to run nigiri rpc");
        let address = String::from_utf8(output.stdout).unwrap().trim().to_string();

        let mint_resp = nigiri
            .mint(&address, 1000, "TestToken", "TST")
            .await
            .unwrap();

        assert!(!mint_resp.asset.is_empty(), "Should return asset hex ID");
        assert!(!mint_resp.txid.is_empty(), "Should return txid");
    }

    #[tokio::test]
    async fn test_tx_confirmation() {
        let nigiri = require_nigiri().await;

        let output = std::process::Command::new("nigiri")
            .args(["rpc", "--liquid", "getnewaddress"])
            .output()
            .expect("Failed to run nigiri rpc");
        let address = String::from_utf8(output.stdout).unwrap().trim().to_string();

        let txid = nigiri.faucet(&address, Some(0.001)).await.unwrap();

        // Chopsticks auto-mines, so tx should be confirmed quickly
        nigiri
            .wait_for_confirmation(&txid, Duration::from_secs(10))
            .await
            .unwrap();

        let tx_info = nigiri.get_tx(&txid).await.unwrap();
        assert!(tx_info.status.confirmed, "Tx should be confirmed");
    }

    #[tokio::test]
    async fn test_esplora_url() {
        let nigiri = NigiriClient::new();
        assert_eq!(nigiri.esplora_url(), "http://localhost:30001");
    }
}
