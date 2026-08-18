use std::{borrow::Cow, str::FromStr};

use bitcoin::{OutPoint, Txid, hashes::Hash, secp256k1::PublicKey};

use crate::{
    Channel, LndError, Millisats, NodeInfo, Peer, Sats, WalletBalance,
    endpoint::parse_peer_endpoint,
    proto::lnrpc::{
        Channel as ProtoChannel, ChannelPoint, GetInfoResponse, Peer as ProtoPeer,
        WalletBalanceResponse, channel_point,
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

fn response_amount(operation: &'static str, value: i64) -> Result<Sats, LndError> {
    u64::try_from(value)
        .map(Sats::new)
        .map_err(|_| invalid_response(operation, "amount is negative"))
}

fn invalid_get_info(detail: &'static str) -> LndError {
    invalid_response("get info", detail)
}

pub(crate) fn invalid_response(operation: &'static str, detail: &'static str) -> LndError {
    LndError::InvalidResponse {
        operation: Cow::Borrowed(operation),
        detail: Cow::Borrowed(detail),
        identifier: None,
    }
}
