use std::{borrow::Cow, str::FromStr};

use bitcoin::{
    OutPoint, Txid,
    hashes::{Hash, sha256},
    secp256k1::PublicKey,
};
use lightning_invoice::Bolt11Invoice;

use crate::{
    Channel, InvoiceRecord, InvoiceState, LndError, Millisats, NodeInfo, PaymentRecord,
    PaymentState, Peer, Sats, WalletBalance,
    endpoint::parse_peer_endpoint,
    proto::lnrpc::{
        AddInvoiceResponse, Channel as ProtoChannel, ChannelPoint, GetInfoResponse,
        Invoice as ProtoInvoice, Payment, Peer as ProtoPeer, WalletBalanceResponse, channel_point,
    },
};

#[allow(deprecated)]
pub(crate) fn node_info(response: GetInfoResponse) -> Result<NodeInfo, LndError> {
    let public_key = PublicKey::from_str(&response.identity_pubkey)
        .map_err(|_| invalid_get_info("identity public key is malformed"))?;
    let [chain] = response.chains.as_slice() else {
        return Err(invalid_response(
            "get info",
            "exactly one active chain is required",
        ));
    };
    if chain.chain != "bitcoin" || chain.network.is_empty() {
        return Err(invalid_response(
            "get info",
            "active chain must name a bitcoin network",
        ));
    }
    Ok(NodeInfo::new(
        public_key,
        response.alias,
        response.version,
        response.block_height,
        chain.network.clone(),
        response.synced_to_chain,
        response.synced_to_graph,
    ))
}

pub(crate) fn wallet_balance(response: WalletBalanceResponse) -> Result<WalletBalance, LndError> {
    let total = response_amount("wallet balance", response.total_balance)?;
    let confirmed = response_amount("wallet balance", response.confirmed_balance)?;
    let unconfirmed = response_amount("wallet balance", response.unconfirmed_balance)?;
    let parts = confirmed
        .as_u64()
        .checked_add(unconfirmed.as_u64())
        .ok_or_else(|| invalid_response("wallet balance", "wallet amount sum overflows"))?;
    if parts != total.as_u64() {
        return Err(invalid_response(
            "wallet balance",
            "total does not equal confirmed plus unconfirmed balance",
        ));
    }
    Ok(WalletBalance::new(total, confirmed, unconfirmed))
}

pub(crate) fn peer(response: ProtoPeer) -> Result<Peer, LndError> {
    let public_key = PublicKey::from_str(&response.pub_key)
        .map_err(|_| invalid_response("list peers", "peer public key is malformed"))?;
    let address = parse_peer_endpoint(&response.address)
        .map_err(|()| invalid_response("list peers", "peer address is malformed"))?;
    Ok(Peer::new(public_key, address, true))
}

pub(crate) fn channel(response: ProtoChannel) -> Result<Channel, LndError> {
    let remote_public_key = PublicKey::from_str(&response.remote_pubkey)
        .map_err(|_| invalid_response("list channels", "remote public key is malformed"))?;
    let channel_point = OutPoint::from_str(&response.channel_point)
        .map_err(|_| invalid_response("list channels", "channel point is malformed"))?;
    let capacity = response_amount("list channels", response.capacity)?;
    let local_sats = response_amount("list channels", response.local_balance)?;
    let remote_sats = response_amount("list channels", response.remote_balance)?;
    let accounted = local_sats
        .as_u64()
        .checked_add(remote_sats.as_u64())
        .ok_or_else(|| invalid_response("list channels", "channel balance sum overflows"))?;
    if accounted > capacity.as_u64() {
        return Err(invalid_response(
            "list channels",
            "channel balances exceed capacity",
        ));
    }
    let local_balance = Millisats::try_from(local_sats)
        .map_err(|_| invalid_response("list channels", "local balance overflows millisatoshis"))?;
    let remote_balance = Millisats::try_from(remote_sats)
        .map_err(|_| invalid_response("list channels", "remote balance overflows millisatoshis"))?;
    Ok(Channel::new(
        channel_point,
        remote_public_key,
        response.active,
        capacity,
        local_balance,
        remote_balance,
    ))
}

pub(crate) fn channel_point(response: ChannelPoint) -> Result<OutPoint, LndError> {
    let txid = match response.funding_txid {
        Some(channel_point::FundingTxid::FundingTxidBytes(bytes)) => Txid::from_slice(&bytes)
            .map_err(|_| invalid_response("open channel", "funding transaction id is malformed"))?,
        Some(channel_point::FundingTxid::FundingTxidStr(value)) => Txid::from_str(&value)
            .map_err(|_| invalid_response("open channel", "funding transaction id is malformed"))?,
        None => {
            return Err(invalid_response(
                "open channel",
                "funding transaction id is missing",
            ));
        }
    };
    Ok(OutPoint::new(txid, response.output_index))
}

pub(crate) fn created_invoice(response: AddInvoiceResponse) -> Result<InvoiceRecord, LndError> {
    let payment_hash = response_hash("create invoice", &response.r_hash)?;
    invoice_record(
        "create invoice",
        response.payment_request,
        payment_hash,
        None,
        InvoiceState::Open,
    )
}

pub(crate) fn invoice(response: ProtoInvoice) -> Result<InvoiceRecord, LndError> {
    let payment_hash = response_hash("lookup invoice", &response.r_hash)?;
    let amount = response_millisats(
        "lookup invoice",
        response.value_msat,
        Some(payment_hash.to_string()),
    )?;
    let creation_date = response_time(
        "lookup invoice",
        response.creation_date,
        "invoice creation date is negative",
        payment_hash,
    )?;
    let expiry = response_time(
        "lookup invoice",
        response.expiry,
        "invoice expiry is negative",
        payment_hash,
    )?;
    let record = invoice_record(
        "lookup invoice",
        response.payment_request,
        payment_hash,
        Some(amount),
        invoice_state(response.state),
    )?;
    if record.invoice().duration_since_epoch().as_secs() != creation_date {
        return Err(invalid_response_with_identifier(
            "lookup invoice",
            "invoice creation date does not match the BOLT11 timestamp",
            Some(payment_hash.to_string()),
        ));
    }
    if record.invoice().expiry_time().as_secs() != expiry {
        return Err(invalid_response_with_identifier(
            "lookup invoice",
            "invoice expiry does not match the BOLT11 expiry",
            Some(payment_hash.to_string()),
        ));
    }
    Ok(record)
}

fn invoice_record(
    operation: &'static str,
    payment_request: String,
    payment_hash: sha256::Hash,
    response_amount: Option<Millisats>,
    state: InvoiceState,
) -> Result<InvoiceRecord, LndError> {
    let identifier = Some(payment_hash.to_string());
    let invoice = Bolt11Invoice::from_str(&payment_request).map_err(|_| {
        invalid_response_with_identifier(
            operation,
            "BOLT11 invoice is malformed",
            identifier.clone(),
        )
    })?;
    if invoice.payment_hash() != &payment_hash {
        return Err(invalid_response_with_identifier(
            operation,
            "BOLT11 payment hash does not match the response hash",
            identifier,
        ));
    }
    let encoded_amount = invoice.amount_milli_satoshis().ok_or_else(|| {
        invalid_response_with_identifier(
            operation,
            "BOLT11 invoice has no amount",
            Some(payment_hash.to_string()),
        )
    })?;
    let amount = response_amount.unwrap_or_else(|| Millisats::new(encoded_amount));
    if amount.as_u64() != encoded_amount {
        return Err(invalid_response_with_identifier(
            operation,
            "BOLT11 amount does not match the response amount",
            Some(payment_hash.to_string()),
        ));
    }
    Ok(InvoiceRecord::new(invoice, payment_hash, amount, state))
}

pub(crate) fn payment(response: Payment) -> Result<PaymentRecord, LndError> {
    let payment_hash = sha256::Hash::from_str(&response.payment_hash)
        .map_err(|_| invalid_response("payment", "payment hash is malformed"))?;
    let identifier = Some(payment_hash.to_string());
    let value = response_millisats("payment", response.value_msat, identifier.clone())?;
    let fee = response_millisats("payment", response.fee_msat, identifier.clone())?;
    let state = payment_state(response.status);
    let preimage = if response.payment_preimage.is_empty() {
        None
    } else {
        Some(parse_preimage(&response.payment_preimage).map_err(|()| {
            invalid_response_with_identifier(
                "payment",
                "payment preimage is malformed",
                identifier.clone(),
            )
        })?)
    };
    if state == PaymentState::Succeeded {
        let preimage = preimage.ok_or_else(|| {
            invalid_response_with_identifier(
                "payment",
                "succeeded payment has no preimage",
                identifier.clone(),
            )
        })?;
        if sha256::Hash::hash(&preimage) != payment_hash {
            return Err(invalid_response_with_identifier(
                "payment",
                "payment preimage does not prove the payment hash",
                identifier,
            ));
        }
    }
    Ok(PaymentRecord::new(
        payment_hash,
        preimage,
        value,
        fee,
        state,
    ))
}

fn invoice_state(value: i32) -> InvoiceState {
    match value {
        0 => InvoiceState::Open,
        1 => InvoiceState::Settled,
        2 => InvoiceState::Canceled,
        3 => InvoiceState::Accepted,
        unknown => InvoiceState::Unknown(unknown),
    }
}

fn payment_state(value: i32) -> PaymentState {
    match value {
        1 | 4 => PaymentState::InFlight,
        2 => PaymentState::Succeeded,
        3 => PaymentState::Failed,
        unknown => PaymentState::Unknown(unknown),
    }
}

fn response_hash(operation: &'static str, value: &[u8]) -> Result<sha256::Hash, LndError> {
    sha256::Hash::from_slice(value)
        .map_err(|_| invalid_response(operation, "payment hash is malformed"))
}

fn response_millisats(
    operation: &'static str,
    value: i64,
    identifier: Option<String>,
) -> Result<Millisats, LndError> {
    u64::try_from(value)
        .map(Millisats::new)
        .map_err(|_| invalid_response_with_identifier(operation, "amount is negative", identifier))
}

fn response_time(
    operation: &'static str,
    value: i64,
    negative_detail: &'static str,
    payment_hash: sha256::Hash,
) -> Result<u64, LndError> {
    u64::try_from(value).map_err(|_| {
        invalid_response_with_identifier(operation, negative_detail, Some(payment_hash.to_string()))
    })
}

fn parse_preimage(value: &str) -> Result<[u8; 32], ()> {
    if value.len() != 64 || !value.is_ascii() {
        return Err(());
    }
    let mut bytes = [0_u8; 32];
    for (index, output) in bytes.iter_mut().enumerate() {
        let offset = index * 2;
        let high = hex_nibble(value.as_bytes()[offset]).ok_or(())?;
        let low = hex_nibble(value.as_bytes()[offset + 1]).ok_or(())?;
        *output = (high << 4) | low;
    }
    Ok(bytes)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn response_amount(operation: &'static str, value: i64) -> Result<Sats, LndError> {
    u64::try_from(value)
        .map(Sats::new)
        .map_err(|_| invalid_response(operation, "amount is negative"))
}

fn invalid_get_info(detail: &'static str) -> LndError {
    invalid_response("get info", detail)
}

pub(crate) fn invalid_response(operation: &'static str, detail: &'static str) -> LndError {
    invalid_response_with_identifier(operation, detail, None)
}

pub(crate) fn invalid_response_with_identifier(
    operation: &'static str,
    detail: &'static str,
    identifier: Option<String>,
) -> LndError {
    LndError::InvalidResponse {
        operation: Cow::Borrowed(operation),
        detail: Cow::Borrowed(detail),
        identifier,
    }
}
