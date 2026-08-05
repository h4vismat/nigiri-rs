use std::{marker::PhantomData, str::FromStr, time::Duration};

use bitcoin::Denomination;
use url::Url;

use crate::{
    Bitcoin, ElectrumEndpoint, Liquid, NigiriConfig, NigiriError, NigiriNetwork, TxStatus,
    http::{endpoint, send_bounded},
};

/// Typed client for configured regtest services.
#[derive(Debug, Clone)]
pub struct NigiriClient<N: NigiriNetwork> {
    pub(crate) config: NigiriConfig,
    pub(crate) http: reqwest::Client,
    network: PhantomData<N>,
}

impl NigiriClient<Bitcoin> {
    /// Constructs a client using Nigiri v0.5.16 Bitcoin defaults.
    pub fn new() -> Self {
        Self::with_config(NigiriConfig::bitcoin()).expect("static Nigiri configuration is valid")
    }
}

impl Default for NigiriClient<Bitcoin> {
    fn default() -> Self {
        Self::new()
    }
}

impl NigiriClient<Liquid> {
    /// Constructs a client using Nigiri v0.5.16 Liquid defaults.
    pub fn new() -> Self {
        Self::with_config(NigiriConfig::liquid()).expect("static Nigiri configuration is valid")
    }
}

impl Default for NigiriClient<Liquid> {
    fn default() -> Self {
        Self::new()
    }
}

impl<N: NigiriNetwork> NigiriClient<N> {
    /// Constructs a client from a validated, normalized configuration.
    pub fn with_config(config: NigiriConfig) -> Result<Self, NigiriError> {
        let config = config.validate_and_normalize()?;
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|source| NigiriError::HttpTransport {
                operation: "build HTTP client".into(),
                source,
            })?;
        Ok(Self {
            config,
            http,
            network: PhantomData,
        })
    }

    /// Returns the normalized Esplora base URL.
    pub fn esplora_url(&self) -> &Url {
        &self.config.esplora_url
    }

    /// Returns the Electrum host and port.
    ///
    /// A fixture reports its runtime-mapped port here, not the fixed container port, so a caller
    /// building an Electrum connection string must read it rather than assume 50000 or 50001.
    pub fn electrum_endpoint(&self) -> &ElectrumEndpoint {
        &self.config.electrum
    }

    /// Waits until the configured Esplora endpoint responds successfully.
    pub async fn wait_ready(&self) -> Result<(), NigiriError> {
        let started = tokio::time::Instant::now();
        loop {
            if self.block_height().await.is_ok() {
                return Ok(());
            }
            if started.elapsed() >= self.config.timeout {
                return Err(NigiriError::Timeout {
                    operation: "wait for readiness".into(),
                    duration: self.config.timeout,
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Returns the current Esplora block height.
    pub async fn block_height(&self) -> Result<u64, NigiriError> {
        const OPERATION: &str = "block height";
        let url = endpoint(
            &self.config.esplora_url,
            OPERATION,
            &["blocks", "tip", "height"],
        )?;
        let body = send_bounded(self, OPERATION, self.http.get(url), &[]).await?;
        let text = std::str::from_utf8(&body).map_err(|_| invalid(OPERATION, "decimal height"))?;
        text.trim()
            .parse()
            .map_err(|_| invalid(OPERATION, "decimal height"))
    }

    /// Funds an address through the node wallet RPC, mines exactly one block, and
    /// returns the native funding transaction identifier.
    ///
    /// `None` sends exactly 1 BTC. That default is chosen here rather than by the
    /// service: earlier versions routed this through Chopsticks, which omitted the
    /// amount and let the server decide.
    ///
    /// If the wallet funding transaction commits but the subsequent mining fails,
    /// returns [`NigiriError::PostTransactionMiningFailed`] with the committed
    /// transaction identifier. Inspect node state before retrying.
    pub async fn faucet(
        &self,
        address: &str,
        amount: Option<bitcoin::Amount>,
    ) -> Result<N::Txid, NigiriError> {
        const OPERATION: &str = "faucet";
        let amount = amount.unwrap_or(bitcoin::Amount::ONE_BTC);
        let (amount_text, amount) =
            amount_as_json_number(amount, "faucet amount could not be represented as JSON")?;
        let value: String = crate::node_rpc::call_sensitive(
            self,
            "sendtoaddress",
            N::native_send_params(address, amount),
            &[address, &amount_text],
        )
        .await?;
        let txid = N::parse_txid(OPERATION, &value)?;
        self.mine_committed_transaction(OPERATION, &txid).await?;
        Ok(txid)
    }

    async fn mine_committed_transaction(
        &self,
        operation: &'static str,
        txid: &N::Txid,
    ) -> Result<(), NigiriError> {
        let result = async {
            let address = self.new_address().await?;
            self.generate_to_address(1, &address.to_string()).await?;
            Ok::<(), NigiriError>(())
        }
        .await;

        result.map_err(|source| NigiriError::PostTransactionMiningFailed {
            operation: operation.into(),
            txid: txid.to_string(),
            source: Box::new(source),
        })
    }

    /// Returns the UTXOs associated with an address path.
    pub async fn get_utxos(&self, address: &str) -> Result<Vec<N::Utxo>, NigiriError> {
        const OPERATION: &str = "get UTXOs";
        let url = endpoint(
            &self.config.esplora_url,
            OPERATION,
            &["address", address, "utxo"],
        )?;
        let body = send_bounded(self, OPERATION, self.http.get(url), &[address]).await?;
        N::parse_utxos(OPERATION, &body)
    }

    /// Returns whether an address currently has at least one UTXO.
    pub async fn has_funds(&self, address: &str) -> Result<bool, NigiriError> {
        self.get_utxos(address).await.map(|utxos| !utxos.is_empty())
    }

    /// Returns typed Esplora address information.
    pub async fn get_address_info(&self, address: &str) -> Result<N::AddressInfo, NigiriError> {
        const OPERATION: &str = "get address information";
        let url = endpoint(&self.config.esplora_url, OPERATION, &["address", address])?;
        let body = send_bounded(self, OPERATION, self.http.get(url), &[address]).await?;
        N::parse_address_info(OPERATION, &body)
    }

    /// Returns typed Esplora transaction information.
    pub async fn get_tx(&self, txid: &N::Txid) -> Result<N::TxInfo, NigiriError> {
        const OPERATION: &str = "get transaction";
        let txid = txid.to_string();
        let url = endpoint(&self.config.esplora_url, OPERATION, &["tx", &txid])?;
        let body = send_bounded(self, OPERATION, self.http.get(url), &[&txid]).await?;
        N::parse_tx_info(OPERATION, &body)
    }

    /// Returns typed Esplora confirmation status.
    pub async fn get_tx_status(
        &self,
        txid: &N::Txid,
    ) -> Result<TxStatus<N::BlockHash>, NigiriError> {
        const OPERATION: &str = "get transaction status";
        let txid = txid.to_string();
        let url = endpoint(
            &self.config.esplora_url,
            OPERATION,
            &["tx", &txid, "status"],
        )?;
        let body = send_bounded(self, OPERATION, self.http.get(url), &[&txid]).await?;
        N::parse_tx_status(OPERATION, &body)
    }

    /// Broadcasts a raw transaction through the node RPC, then mines exactly one block.
    ///
    /// If the broadcast commits but the subsequent mining fails, returns
    /// [`NigiriError::PostTransactionMiningFailed`] with the committed transaction
    /// identifier. Inspect node state before retrying.
    pub async fn broadcast_tx(&self, transaction_hex: &str) -> Result<N::Txid, NigiriError> {
        const OPERATION: &str = "broadcast transaction";
        let value: String = crate::node_rpc::call_sensitive(
            self,
            "sendrawtransaction",
            (transaction_hex,),
            &[transaction_hex],
        )
        .await?;
        let txid = N::parse_txid(OPERATION, &value)?;
        self.mine_committed_transaction(OPERATION, &txid).await?;
        Ok(txid)
    }

    /// Polls until a native transaction is confirmed or the supplied timeout elapses.
    pub async fn wait_for_confirmation(
        &self,
        txid: &N::Txid,
        timeout: Duration,
    ) -> Result<(), NigiriError> {
        let started = tokio::time::Instant::now();
        loop {
            if self.get_tx_status(txid).await?.confirmed {
                return Ok(());
            }
            if started.elapsed() >= timeout {
                return Err(NigiriError::Timeout {
                    operation: "wait for confirmation".into(),
                    duration: timeout,
                });
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

fn invalid(operation: &'static str, expected: &'static str) -> NigiriError {
    NigiriError::InvalidResponse {
        operation: operation.into(),
        detail: format!("expected {expected}"),
    }
}

/// Formats a Bitcoin-denominated amount as a JSON number for a node RPC call.
///
/// Node RPCs take decimal BTC, not satoshis, and take it as a JSON number, not a string. Routing
/// that through `f64` risks rounding a value that started out exact; going through
/// [`bitcoin::Amount::to_string_in`] and [`serde_json::Number::from_str`] keeps it exact instead,
/// which is why this exists rather than a call to `serde_json::json!`.
///
/// `detail` fills [`NigiriError::InvalidRequest`] if the conversion is ever rejected, which in
/// practice does not happen for a value that came from [`bitcoin::Amount`] — the check exists
/// because the conversion is fallible, not because it is expected to fail.
///
/// Returns the formatted decimal alongside the parsed number: a caller that also needs the text —
/// for a [`crate::node_rpc::call_sensitive`] redaction list — gets it for free instead of
/// formatting the amount twice.
pub(crate) fn amount_as_json_number(
    amount: bitcoin::Amount,
    detail: &'static str,
) -> Result<(String, serde_json::Number), NigiriError> {
    let amount_text = amount.to_string_in(Denomination::Bitcoin);
    let number =
        serde_json::Number::from_str(&amount_text).map_err(|_| NigiriError::InvalidRequest {
            detail: detail.into(),
        })?;
    Ok((amount_text, number))
}

#[cfg(test)]
mod tests {
    use crate::{Bitcoin, NigiriClient};

    // Catches a regression that stops surfacing the Electrum endpoint from the client. A consumer
    // pointing a BDK or LWK wallet at the stack reaches it only through here.
    #[test]
    fn client_surfaces_its_electrum_endpoint() {
        let client = NigiriClient::<Bitcoin>::new();

        assert_eq!(client.electrum_endpoint().host(), "localhost");
        assert_eq!(client.electrum_endpoint().port(), 50_000);
    }
}
