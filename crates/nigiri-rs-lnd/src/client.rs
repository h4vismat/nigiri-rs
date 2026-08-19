use std::{fmt, sync::Arc};

use crate::{LndConfig, LndError, NodeInfo, transport::ClientInner};

/// Immutable, cheaply cloneable client for an externally managed LND node.
#[derive(Clone)]
pub struct LndClient {
    pub(crate) inner: Arc<ClientInner>,
}

impl LndClient {
    /// Builds an authenticated client without opening a network connection.
    pub fn with_config(config: LndConfig) -> Result<Self, LndError> {
        Ok(Self {
            inner: ClientInner::authenticated(&config)?,
        })
    }

    /// Performs a bounded `GetInfo` call and returns once the daemon is reachable.
    pub async fn wait_ready(&self) -> Result<NodeInfo, LndError> {
        self.get_info().await
    }
}

impl fmt::Debug for LndClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LndClient")
            .field("timeout", &self.inner.timeout)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bitcoin::{
        hashes::{Hash, sha256},
        secp256k1::PublicKey,
    };
    use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};

    use crate::{
        CreateInvoiceRequest, LightningNode, LndClient, LndConfig, LndError, Millisats,
        OpenChannelRequest, PaymentOptions, PeerAddress, Sats,
    };

    fn config() -> LndConfig {
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .unwrap()
            .cert
            .pem()
            .into_bytes();
        LndConfig::new(
            "https://localhost:10009",
            certificate,
            vec![1],
            Duration::from_millis(100),
        )
        .unwrap()
    }

    #[test]
    fn construction_does_not_require_a_tokio_runtime() {
        let client = LndClient::with_config(config()).unwrap();

        assert!(format!("{client:?}").starts_with("LndClient"));
    }

    #[tokio::test]
    async fn construction_is_lazy_and_wait_ready_performs_the_first_rpc() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let config = LndConfig::new(
            format!("https://localhost:{port}"),
            config().certificate().to_vec(),
            vec![1],
            Duration::from_millis(100),
        )
        .unwrap();

        let client = LndClient::with_config(config).unwrap();
        let error = client.wait_ready().await.unwrap_err();

        assert!(matches!(
            error,
            LndError::Status { .. } | LndError::Transport { .. } | LndError::Timeout { .. }
        ));
    }

    #[test]
    fn lightning_node_operations_return_send_futures() {
        fn assert_node<T: LightningNode<Error = LndError>>(_node: &T) {}
        fn assert_send<T: Send>(_value: T) {}

        let client = LndClient::with_config(config()).unwrap();
        let public_key = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
            .parse::<PublicKey>()
            .unwrap();
        let peer = PeerAddress::new(public_key, "bob.internal", 9735).unwrap();
        let open = OpenChannelRequest::new(public_key, Sats::new(2), Sats::new(1)).unwrap();
        let payment_hash = sha256::Hash::from_byte_array([7; 32]);
        let invoice = InvoiceBuilder::new(Currency::Regtest)
            .description("trait future test".into())
            .payment_hash(payment_hash)
            .payment_secret(PaymentSecret([21; 32]))
            .amount_milli_satoshis(1_000)
            .duration_since_epoch(Duration::from_secs(1_700_000_000))
            .min_final_cltv_expiry_delta(18)
            .build_signed(|message| {
                bitcoin::secp256k1::Secp256k1::new().sign_ecdsa_recoverable(
                    message,
                    &bitcoin::secp256k1::SecretKey::from_slice(&[42; 32]).unwrap(),
                )
            })
            .unwrap();
        let create = CreateInvoiceRequest::new(
            Millisats::new(1_000),
            "trait future test",
            Duration::from_secs(60),
        )
        .unwrap();
        let payment_options =
            PaymentOptions::new(Millisats::new(100), Duration::from_secs(5)).unwrap();

        assert_node(&client);
        assert_send(LightningNode::get_info(&client));
        assert_send(LightningNode::new_address(&client));
        assert_send(LightningNode::wallet_balance(&client));
        assert_send(LightningNode::connect_peer(&client, &peer));
        assert_send(LightningNode::list_peers(&client));
        assert_send(LightningNode::open_channel(&client, open));
        assert_send(LightningNode::list_channels(&client));
        assert_send(LightningNode::create_invoice(&client, create));
        assert_send(LightningNode::lookup_invoice(&client, payment_hash));
        assert_send(LightningNode::pay_invoice(
            &client,
            &invoice,
            payment_options,
        ));
        assert_send(LightningNode::lookup_payment(&client, payment_hash));
    }
}
