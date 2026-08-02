use std::{marker::PhantomData, str::FromStr, time::Duration};

use bitcoin::Denomination;
use url::Url;

use crate::{
    Bitcoin, Liquid, NigiriConfig, NigiriError, NigiriNetwork, TxStatus,
    http::{endpoint, parse_txid, send_bounded},
};

/// Typed client for an already-running Nigiri network.
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

    /// Funds an address and returns the native funding transaction identifier.
    pub async fn faucet(
        &self,
        address: &str,
        amount: Option<bitcoin::Amount>,
    ) -> Result<N::Txid, NigiriError> {
        const OPERATION: &str = "faucet";
        let amount = amount.unwrap_or(bitcoin::Amount::ONE_BTC);
        let amount = serde_json::Number::from_str(&amount.to_string_in(Denomination::Bitcoin))
            .map_err(|_| NigiriError::InvalidRequest {
                detail: "faucet amount could not be represented as JSON".into(),
            })?;
        let value: String = crate::node_rpc::call(
            self,
            "sendtoaddress",
            N::native_send_params(address, amount),
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

    /// Broadcasts a raw transaction through Chopsticks, which mines a block.
    pub async fn broadcast_tx(&self, transaction_hex: &str) -> Result<N::Txid, NigiriError> {
        const OPERATION: &str = "broadcast transaction";
        let url = endpoint(&self.config.chopsticks_url, OPERATION, &["tx"])?;
        let body = send_bounded(
            self,
            OPERATION,
            self.http.post(url).body(transaction_hex.to_owned()),
            &[transaction_hex],
        )
        .await?;
        parse_txid::<N>(OPERATION, &body)
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
