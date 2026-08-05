//! Decoding an Elements peg-out output. Pure: no client, no I/O, no async.
//!
//! A peg-out output's `scriptPubKey` is `OP_RETURN <32-byte parent genesis hash>
//! <destination scriptPubKey>`. Decoding it rather than trusting a caller-supplied destination is
//! what makes the simulated federation worth testing against: a consumer who encodes the
//! destination wrong gets no payout, exactly as on liquidv1.

use bitcoin::{
    ScriptBuf,
    blockdata::{
        opcodes::all::OP_RETURN,
        script::{Instruction, Script},
    },
    hashes::Hash,
};

/// What a peg-out output names: which parent chain, and where on it.
// `Debug` is required by the tests' `expect_err` on `Result<PegOutTarget, _>`.
#[derive(Debug)]
pub(crate) struct PegOutTarget {
    pub(crate) parent_genesis: bitcoin::BlockHash,
    pub(crate) destination: ScriptBuf,
}

/// Reads a peg-out output, or explains why it is not one.
///
/// Returns `String` rather than [`crate::NigiriError`] so this stays free of transaction context:
/// the caller knows the txid and wraps the detail with it.
pub(crate) fn decode_peg_out_script(script: &Script) -> Result<PegOutTarget, String> {
    let mut instructions = script.instructions();

    match instructions.next() {
        Some(Ok(Instruction::Op(op))) if op == OP_RETURN => {}
        _ => return Err("script does not begin with OP_RETURN".to_owned()),
    }

    let parent_genesis = match instructions.next() {
        Some(Ok(Instruction::PushBytes(bytes))) if bytes.len() == 32 => {
            let mut raw = [0_u8; 32];
            raw.copy_from_slice(bytes.as_bytes());
            bitcoin::BlockHash::from_byte_array(raw)
        }
        _ => return Err("expected a 32-byte parent genesis hash push".to_owned()),
    };

    let destination = match instructions.next() {
        Some(Ok(Instruction::PushBytes(bytes))) if !bytes.as_bytes().is_empty() => {
            ScriptBuf::from_bytes(bytes.as_bytes().to_vec())
        }
        _ => return Err("expected a non-empty destination script push".to_owned()),
    };

    Ok(PegOutTarget {
        parent_genesis,
        destination,
    })
}

#[cfg(test)]
mod tests {
    use bitcoin::hex::FromHex;

    use super::decode_peg_out_script;

    /// The regtest parent genesis hash, in RPC display order. Fixed in Bitcoin Core's chain
    /// parameters, identical on every regtest node.
    const REGTEST_GENESIS: &str =
        "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206";

    /// Recorded from a real `sendtomainchain` during the Task 1 spike. Replace with the value in
    /// the spec's Spikes results before this test is meaningful.
    const GOLDEN_PEG_OUT_SCRIPT: &str = "6a2006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f160014153a100bf13cf08f49d13163e49df5a51d186626";
    const GOLDEN_DESTINATION_SCRIPT: &str = "0014153a100bf13cf08f49d13163e49df5a51d186626";

    fn script(hex: &str) -> bitcoin::ScriptBuf {
        bitcoin::ScriptBuf::from_bytes(Vec::<u8>::from_hex(hex).expect("valid hex"))
    }

    // Catches a regression in decoding a real peg-out output. This vector came off a live
    // Elements node rather than being constructed here, so it also catches a wrong idea about
    // the format itself.
    #[test]
    fn a_real_peg_out_output_yields_its_parent_and_destination() {
        let decoded = decode_peg_out_script(&script(GOLDEN_PEG_OUT_SCRIPT))
            .expect("a real peg-out output decodes");

        assert_eq!(decoded.parent_genesis.to_string(), REGTEST_GENESIS);
        assert_eq!(
            decoded.destination.to_hex_string(),
            GOLDEN_DESTINATION_SCRIPT
        );
    }

    // Catches a regression that accepts an ordinary output as a peg-out, which would make the
    // simulated federation pay out against transactions that never pegged out.
    #[test]
    fn a_plain_output_is_not_a_peg_out() {
        // A bare P2WPKH: OP_0 <20 bytes>.
        let error = decode_peg_out_script(&script("0014389ffce9cd9ae88dcc0631e88a821ffdbe9bfe26"))
            .expect_err("a payment output is not a peg-out");

        assert!(error.contains("OP_RETURN"), "unhelpful detail: {error}");
    }

    // Catches a regression that reads a short push as a genesis hash, which would decode a
    // truncated or unrelated OP_RETURN as though it named a parent chain.
    #[test]
    fn a_short_genesis_push_is_rejected() {
        let error = decode_peg_out_script(&script("6a04deadbeef"))
            .expect_err("a 4-byte push is not a genesis hash");

        assert!(error.contains("32-byte"), "unhelpful detail: {error}");
    }

    // Catches a regression that accepts a peg-out with no destination, which would leave the
    // federation with nowhere to pay.
    #[test]
    fn a_missing_destination_push_is_rejected() {
        let genesis_push = format!("6a20{}", "11".repeat(32));
        let error = decode_peg_out_script(&script(&genesis_push))
            .expect_err("a peg-out with no destination is malformed");

        assert!(error.contains("destination"), "unhelpful detail: {error}");
    }

    // Catches a regression that accepts an empty destination push, which parses structurally but
    // names no address.
    #[test]
    fn an_empty_destination_push_is_rejected() {
        let script_hex = format!("6a20{}00", "11".repeat(32));
        let error = decode_peg_out_script(&script(&script_hex))
            .expect_err("an empty destination is malformed");

        assert!(error.contains("destination"), "unhelpful detail: {error}");
    }
}
