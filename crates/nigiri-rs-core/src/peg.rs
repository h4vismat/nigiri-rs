//! Cross-chain peg operations. See `peg/output.rs` for the pure decoding half.

mod output;

use serde::Deserialize;

use crate::{Bitcoin, Liquid, NigiriClient, NigiriError};

/// A Bitcoin and Liquid pair that can move value across the peg.
///
/// Holds both clients by value. [`NigiriClient`] is [`Clone`] and cheap — immutable configuration
/// plus a shared transport — so this avoids threading two lifetimes through every signature.
/// Nothing here knows or cares what started the nodes.
#[derive(Debug, Clone)]
pub struct Peg {
    bitcoin: NigiriClient<Bitcoin>,
    liquid: NigiriClient<Liquid>,
    #[allow(dead_code, reason = "read by release_peg_out in a later task")]
    parent_genesis: bitcoin::BlockHash,
    pegin_confirmation_depth: u64,
}

#[derive(Deserialize)]
struct SideChainInfo {
    parent_blockhash: bitcoin::BlockHash,
    pegin_confirmation_depth: u64,
}

impl Peg {
    /// Pairs two clients after verifying they are actually a peg pair.
    ///
    /// Reads the Liquid node's `getsidechaininfo` and compares its parent block hash against the
    /// Bitcoin node's genesis. A mismatch means the two were never wired together, which is the
    /// easiest mistake to make here and the hardest to diagnose later.
    ///
    /// There is deliberately no infallible constructor.
    pub async fn connect(
        bitcoin: NigiriClient<Bitcoin>,
        liquid: NigiriClient<Liquid>,
    ) -> Result<Self, NigiriError> {
        let info: SideChainInfo = liquid.rpc("getsidechaininfo", ()).await?;
        let genesis: bitcoin::BlockHash = bitcoin.rpc("getblockhash", (0_u64,)).await?;

        if info.parent_blockhash != genesis {
            return Err(NigiriError::PegNotConfigured {
                detail: format!(
                    "the Liquid node's parent chain is {} but the Bitcoin node's genesis is {genesis}",
                    info.parent_blockhash
                )
                .into(),
            });
        }

        Ok(Self {
            bitcoin,
            liquid,
            parent_genesis: genesis,
            pegin_confirmation_depth: info.pegin_confirmation_depth,
        })
    }

    /// The Bitcoin side of the pair.
    #[must_use]
    pub fn bitcoin(&self) -> &NigiriClient<Bitcoin> {
        &self.bitcoin
    }

    /// The Liquid side of the pair.
    #[must_use]
    pub fn liquid(&self) -> &NigiriClient<Liquid> {
        &self.liquid
    }

    /// Confirmations a deposit needs before it can be claimed, as reported by the sidechain.
    #[must_use]
    pub fn pegin_confirmation_depth(&self) -> u64 {
        self.pegin_confirmation_depth
    }
}
