//! A downstream adapter must be able to supply every successful LightningNode response.
use bitcoin::{
    Address, OutPoint,
    address::NetworkUnchecked,
    hashes::{Hash, sha256},
    secp256k1::{PublicKey, Secp256k1, SecretKey},
};
use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder, PaymentSecret};
use nigiri_rs_lnd::*;
use std::time::Duration;

#[derive(Clone)]
struct TestNode;
fn key() -> PublicKey {
    PublicKey::from_secret_key(
        &Secp256k1::new(),
        &SecretKey::from_slice(&[42; 32]).unwrap(),
    )
}
fn invoice() -> Bolt11Invoice {
    InvoiceBuilder::new(Currency::Regtest)
        .description("adapter".into())
        .payment_hash(sha256::Hash::hash(&[1; 32]))
        .payment_secret(PaymentSecret([2; 32]))
        .duration_since_epoch(Duration::from_secs(100))
        .min_final_cltv_expiry_delta(18)
        .amount_milli_satoshis(1000)
        .build_signed(|m| {
            Secp256k1::new().sign_ecdsa_recoverable(m, &SecretKey::from_slice(&[42; 32]).unwrap())
        })
        .unwrap()
}
impl LightningNode for TestNode {
    type Error = LndError;
    async fn get_info(&self) -> Result<NodeInfo, LndError> {
        NodeInfo::try_new(
            key(),
            "test".into(),
            "1".into(),
            10,
            "regtest".into(),
            true,
            true,
        )
    }
    async fn new_address(&self) -> Result<Address<NetworkUnchecked>, LndError> {
        Ok(
            Address::p2pkh(bitcoin::PublicKey::new(key()), bitcoin::Network::Regtest)
                .into_unchecked(),
        )
    }
    async fn wallet_balance(&self) -> Result<WalletBalance, LndError> {
        WalletBalance::try_new(Sats::new(2), Sats::new(1), Sats::new(1))
    }
    async fn connect_peer(&self, _: &PeerAddress) -> Result<(), LndError> {
        Ok(())
    }
    async fn list_peers(&self) -> Result<Vec<Peer>, LndError> {
        Ok(vec![Peer::try_new(key(), "localhost:9735", true)?])
    }
    async fn open_channel(&self, _: OpenChannelRequest) -> Result<OutPoint, LndError> {
        Ok(OutPoint::null())
    }
    async fn list_channels(&self) -> Result<Vec<Channel>, LndError> {
        Ok(vec![Channel::try_new(
            OutPoint::null(),
            key(),
            true,
            Sats::new(2),
            Millisats::new(1000),
            Millisats::new(1000),
        )?])
    }
    async fn create_invoice(&self, _: CreateInvoiceRequest) -> Result<InvoiceRecord, LndError> {
        InvoiceRecord::from_invoice(invoice(), InvoiceState::Open)
    }
    async fn lookup_invoice(&self, _: sha256::Hash) -> Result<InvoiceRecord, LndError> {
        InvoiceRecord::from_invoice(invoice(), InvoiceState::Settled)
    }
    async fn pay_invoice(
        &self,
        _: &Bolt11Invoice,
        _: PaymentOptions,
    ) -> Result<PaymentRecord, LndError> {
        self.lookup_payment(sha256::Hash::hash(&[1; 32])).await
    }
    async fn lookup_payment(&self, hash: sha256::Hash) -> Result<PaymentRecord, LndError> {
        PaymentRecord::try_new(
            hash,
            Some([1; 32]),
            Millisats::new(1000),
            Millisats::new(0),
            PaymentState::Succeeded,
        )
    }
}
#[tokio::test]
async fn downstream_adapter_constructs_validated_responses() {
    let node = TestNode;
    assert!(node.get_info().await.unwrap().synced_to_chain());
    assert_eq!(node.wallet_balance().await.unwrap().total(), Sats::new(2));
    assert_eq!(node.list_peers().await.unwrap().len(), 1);
    assert_eq!(node.list_channels().await.unwrap().len(), 1);
    let record = node
        .lookup_invoice(*invoice().payment_hash())
        .await
        .unwrap();
    let paid = node
        .pay_invoice(
            record.invoice(),
            PaymentOptions::new(Millisats::new(1), Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(paid.state(), PaymentState::Succeeded);
}
#[test]
fn contradictory_records_are_rejected() {
    assert!(
        NodeInfo::try_new(
            key(),
            String::new(),
            String::new(),
            0,
            String::new(),
            false,
            false
        )
        .is_err()
    );
    assert!(WalletBalance::try_new(Sats::new(1), Sats::new(u64::MAX), Sats::new(2)).is_err());
    assert!(Peer::try_new(key(), "host:0", true).is_err());
    assert!(
        Channel::try_new(
            OutPoint::null(),
            key(),
            true,
            Sats::new(1),
            Millisats::new(1001),
            Millisats::new(0)
        )
        .is_err()
    );
    assert!(
        PaymentRecord::try_new(
            sha256::Hash::hash(&[1; 32]),
            None,
            Millisats::new(1),
            Millisats::new(0),
            PaymentState::Succeeded
        )
        .is_err()
    );
    assert!(
        PaymentRecord::try_new(
            sha256::Hash::hash(&[1; 32]),
            Some([2; 32]),
            Millisats::new(1),
            Millisats::new(0),
            PaymentState::Succeeded
        )
        .is_err()
    );
}
