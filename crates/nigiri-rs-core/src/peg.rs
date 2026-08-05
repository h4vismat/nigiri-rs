//! Cross-chain peg operations. See `peg/output.rs` for the pure decoding half.

mod output;

use std::str::FromStr;

use bitcoin::{Amount, Denomination, hex::FromHex};
use serde::Deserialize;

use crate::peg::output::decode_peg_out_script;
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

/// Whether mining another block could plausibly change the outcome of a claim.
///
/// The Liquid node's view of the mainchain lags the mainchain, so a claim it has considered and
/// rejected may succeed a block later — that is the whole reason `complete_peg_in` retries. A
/// transport failure or an unusable response is not that: no amount of mining fixes a dead socket
/// or a malformed reply, and retrying one only delays the real error by twenty blocks.
///
/// Matched on the variant rather than the node's message text, which carries no compatibility
/// promise.
fn worth_retrying(error: &NigiriError) -> bool {
    matches!(
        error,
        NigiriError::PegInImmature { .. } | NigiriError::RpcFailed { .. }
    )
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
    /// guessing a margin. See [`CLAIM_RETRY_BLOCKS`]. A claim failure that another block cannot
    /// plausibly fix — see [`worth_retrying`] — returns immediately instead of spending the
    /// retry budget on it.
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
            Err(error) if worth_retrying(&error) => error,
            Err(error) => return Err(error),
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
                Err(error) if worth_retrying(&error) => last = error,
                Err(error) => return Err(error),
            }
        }

        Err(last)
    }

    /// Burns L-BTC and records a Bitcoin destination in the resulting Liquid transaction.
    ///
    /// This is a genuine Elements RPC. Nothing releases the BTC: on regtest there is no
    /// federation. Follow it with [`Peg::release_peg_out`].
    ///
    /// No peg-out wallet setup is needed. `initpegoutwallet` is rejected on this chain — PAK
    /// enforcement is off, so there is no PAK entry to register — and `sendtomainchain` does not
    /// require one.
    pub async fn send_to_mainchain(
        &self,
        destination: &str,
        amount: Amount,
    ) -> Result<elements::Txid, NigiriError> {
        let amount = serde_json::Number::from_str(&amount.to_string_in(Denomination::Bitcoin))
            .map_err(|_| NigiriError::InvalidRequest {
                detail: "peg-out amount could not be represented as JSON".into(),
            })?;

        self.liquid
            .rpc("sendtomainchain", (destination, amount))
            .await
    }

    /// Plays federation: decodes the peg-out and pays its destination on Bitcoin.
    ///
    /// **This is a simulation.** The released BTC comes from the Bitcoin node's own wallet, not
    /// from a locked reserve, so total BTC on the mainchain side grows with every release. The
    /// Liquid side stays honest — `sendtomainchain` genuinely burned — but no 1:1 invariant holds
    /// across the pair. The release also mines a confirming block, inherited from
    /// [`NigiriClient::faucet`].
    ///
    /// The destination is read out of the transaction rather than taken as an argument, so a
    /// consumer who encodes it wrongly gets no payout, exactly as on liquidv1.
    pub async fn release_peg_out(
        &self,
        liquid_txid: &elements::Txid,
    ) -> Result<PegOut, NigiriError> {
        let txid = liquid_txid.to_string();
        let transaction: LiquidTransaction =
            self.liquid.rpc("getrawtransaction", (&txid, 1_u64)).await?;

        let (destination, amount) = self.decode_peg_out(&transaction, &txid)?;

        let bitcoin_txid = self
            .bitcoin
            .faucet(&destination.to_string(), Some(amount))
            .await?;

        Ok(PegOut {
            liquid_txid: *liquid_txid,
            destination,
            amount,
            bitcoin_txid,
        })
    }

    /// Finds and reads the one peg-out output, or explains what was wrong with it.
    fn decode_peg_out(
        &self,
        transaction: &LiquidTransaction,
        txid: &str,
    ) -> Result<(bitcoin::Address, Amount), NigiriError> {
        let malformed = |detail: String| NigiriError::PegOutputMalformed {
            liquid_txid: txid.to_owned(),
            detail,
        };

        // A wrong-chain output structurally shaped like a peg-out is not the peg-out being looked
        // for: keep scanning rather than rejecting the whole transaction, since a genuine peg-out
        // for this pair may still follow it. The mismatch detail is kept around only in case
        // nothing better is ever found.
        let mut mismatch: Option<String> = None;

        for output in &transaction.vout {
            let Ok(raw) = Vec::<u8>::from_hex(&output.script_pub_key.hex) else {
                continue;
            };
            let script = bitcoin::ScriptBuf::from_bytes(raw);
            let Ok(target) = decode_peg_out_script(&script) else {
                continue;
            };

            if target.parent_genesis != self.parent_genesis {
                if mismatch.is_none() {
                    mismatch = Some(format!(
                        "peg-out names parent chain {} but this pair's parent is {}",
                        target.parent_genesis, self.parent_genesis
                    ));
                }
                continue;
            }

            let destination =
                bitcoin::Address::from_script(&target.destination, bitcoin::Network::Regtest)
                    .map_err(|_| {
                        malformed("destination script is not a standard address".to_owned())
                    })?;

            let value = output
                .value
                .as_ref()
                .ok_or_else(|| malformed("peg-out output has no explicit value".to_owned()))?;
            let amount = Amount::from_str_in(&value.to_string(), Denomination::Bitcoin)
                .map_err(|_| malformed(format!("peg-out value {value} is not an amount")))?;

            return Ok((destination, amount));
        }

        match mismatch {
            Some(detail) => Err(malformed(detail)),
            None => Err(NigiriError::PegOutputNotFound {
                liquid_txid: txid.to_owned(),
            }),
        }
    }
}

/// A peg-out that the simulated federation has released.
#[derive(Debug, Clone)]
pub struct PegOut {
    /// The Liquid transaction that burned the L-BTC.
    pub liquid_txid: elements::Txid,
    /// The destination decoded out of the peg-out output, not supplied by the caller.
    pub destination: bitcoin::Address,
    /// The value decoded out of the peg-out output.
    pub amount: Amount,
    /// The Bitcoin transaction this crate sent to simulate the federation's release.
    pub bitcoin_txid: bitcoin::Txid,
}

#[derive(Deserialize)]
struct LiquidTransaction {
    vout: Vec<LiquidOutput>,
}

#[derive(Deserialize)]
struct LiquidOutput {
    /// Deserialized as a `Number`, never `f64`: `arbitrary_precision` keeps it exact.
    #[serde(default)]
    value: Option<serde_json::Number>,
    #[serde(rename = "scriptPubKey")]
    script_pub_key: OutputScript,
}

#[derive(Deserialize)]
struct OutputScript {
    hex: String,
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
