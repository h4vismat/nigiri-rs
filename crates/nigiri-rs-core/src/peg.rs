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

/// How many extra blocks `complete_peg_in` will mine while waiting for the Liquid node to catch
/// up with the mainchain.
///
/// The node rejects a claim at exactly the depth it reports. Task 1's spike saw
/// `pegin_confirmation_depth = 8` and a claim that only succeeded at 11, with the node answering
/// "needs more confirmations to be sent" in between — its view of the mainchain lags the chain
/// itself. Retrying a block at a time adapts to however far behind it is; a fixed margin would
/// bake in a number measured once, on one machine, against one image.
const CLAIM_RETRY_BLOCKS: u64 = 20;

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

    /// Asks the Liquid node for a peg-in address.
    pub async fn peg_in_request(&self) -> Result<PegInRequest, NigiriError> {
        const OPERATION: &str = "peg-in address";

        let issued: PegInAddress = self.liquid.rpc("getpeginaddress", ()).await?;
        let mainchain_address = issued
            .mainchain_address
            .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
            .map_err(|_| NigiriError::InvalidResponse {
                operation: OPERATION.into(),
                detail: "expected a Bitcoin address".to_owned(),
            })?
            .require_network(bitcoin::Network::Regtest)
            .map_err(|_| NigiriError::InvalidResponse {
                operation: OPERATION.into(),
                detail: "expected a regtest Bitcoin address".to_owned(),
            })?;

        Ok(PegInRequest {
            mainchain_address,
            claim_script: issued.claim_script,
        })
    }

    /// Claims a matured deposit, minting L-BTC into the Liquid node's wallet.
    ///
    /// Fetches the deposit and its merkle proof from the Bitcoin node, then submits `claimpegin`.
    /// The claim script is omitted: Elements infers it when the claiming wallet issued the
    /// address, which it did.
    ///
    /// Returns [`NigiriError::PegInImmature`] rather than letting the node reject the claim, so a
    /// caller sees both the depth reached and the depth required.
    pub async fn claim_peg_in(
        &self,
        mainchain_txid: &bitcoin::Txid,
    ) -> Result<elements::Txid, NigiriError> {
        let txid = mainchain_txid.to_string();

        let deposit: MainchainTransaction =
            self.bitcoin.rpc("getrawtransaction", (&txid, true)).await?;
        if deposit.confirmations < self.pegin_confirmation_depth {
            return Err(NigiriError::PegInImmature {
                have: deposit.confirmations,
                need: self.pegin_confirmation_depth,
            });
        }

        let proof: String = self.bitcoin.rpc("gettxoutproof", ([&txid],)).await?;

        self.liquid.rpc("claimpegin", (deposit.hex, proof)).await
    }

    /// Runs a whole peg-in: address, deposit, maturity, claim.
    ///
    /// Mines to the depth the sidechain reports rather than a hardcoded number, so a regtest chain
    /// with a lowered `peginconfirmationdepth` and a production-shaped one both work.
    /// [`NigiriClient::faucet`] already mines one confirming block, so only the remainder is mined
    /// here.
    ///
    /// The node's view of the mainchain lags the mainchain, so reaching the reported depth is
    /// necessary but not sufficient. This mines one more block per rejected attempt rather than
    /// guessing a margin. See [`CLAIM_RETRY_BLOCKS`].
    pub async fn complete_peg_in(&self, amount: bitcoin::Amount) -> Result<PegIn, NigiriError> {
        let request = self.peg_in_request().await?;
        let mainchain_txid = self
            .bitcoin
            .faucet(&request.mainchain_address.to_string(), Some(amount))
            .await?;

        let mining_address = self.bitcoin.new_address().await?.to_string();
        let remaining = self.pegin_confirmation_depth.saturating_sub(1);
        if remaining > 0 {
            self.bitcoin
                .generate_to_address(remaining, &mining_address)
                .await?;
        }

        let mut last = match self.claim_peg_in(&mainchain_txid).await {
            Ok(claim_txid) => {
                return Ok(PegIn {
                    mainchain_txid,
                    claim_txid,
                    amount,
                });
            }
            Err(error) => error,
        };

        for _ in 0..CLAIM_RETRY_BLOCKS {
            self.bitcoin.generate_to_address(1, &mining_address).await?;
            match self.claim_peg_in(&mainchain_txid).await {
                Ok(claim_txid) => {
                    return Ok(PegIn {
                        mainchain_txid,
                        claim_txid,
                        amount,
                    });
                }
                Err(error) => last = error,
            }
        }

        Err(last)
    }
}

/// A peg-in address and the script that will claim it.
///
/// `getpeginaddress` takes no destination: the address is derived from the Liquid node's own
/// wallet, and the eventual claim credits that wallet. Moving pegged funds elsewhere is an
/// ordinary transfer afterwards, not part of the peg.
#[derive(Debug, Clone)]
pub struct PegInRequest {
    /// The Bitcoin address to fund. Federation-controlled, tweaked by the Liquid wallet's keys.
    pub mainchain_address: bitcoin::Address,
    /// Hex claim script, retained for callers that submit their own claim.
    pub claim_script: String,
}

/// A completed peg-in, on both chains.
#[derive(Debug, Clone)]
pub struct PegIn {
    /// The Bitcoin deposit that funded the peg-in address.
    pub mainchain_txid: bitcoin::Txid,
    /// The Liquid transaction that minted the L-BTC.
    pub claim_txid: elements::Txid,
    /// The amount deposited. L-BTC minted equals this minus network fees.
    pub amount: bitcoin::Amount,
}

#[derive(Deserialize)]
struct PegInAddress {
    mainchain_address: String,
    claim_script: String,
}

#[derive(Deserialize)]
struct MainchainTransaction {
    hex: String,
    #[serde(default)]
    confirmations: u64,
}
