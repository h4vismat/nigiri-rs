use std::{borrow::Cow, str::FromStr};

use bitcoin::secp256k1::PublicKey;

use crate::{
    LndError, NodeInfo,
    proto::lnrpc::{Chain, GetInfoResponse},
};

pub(crate) fn node_info(response: GetInfoResponse) -> Result<NodeInfo, LndError> {
    let public_key = PublicKey::from_str(&response.identity_pubkey)
        .map_err(|_| invalid_get_info("identity public key is malformed"))?;
    let network = response
        .chains
        .as_slice()
        .first()
        .map(|Chain { network, .. }| network.clone())
        .filter(|network| !network.is_empty())
        .ok_or_else(|| invalid_get_info("chain network is missing"))?;
    Ok(NodeInfo::new(
        public_key,
        response.alias,
        response.version,
        response.block_height,
        network,
        response.synced_to_chain,
        response.synced_to_graph,
    ))
}

fn invalid_get_info(detail: &'static str) -> LndError {
    LndError::InvalidResponse {
        operation: Cow::Borrowed("get info"),
        detail: Cow::Borrowed(detail),
        identifier: None,
    }
}
