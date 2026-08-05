//! Drives a full peg round trip against a hand-wired Elements and bitcoind pair.
//!
//! Not a test: it needs two containers this crate does not start, because `nigiri-rs-core` starts
//! nothing. Plan 2's `PegPair` fixture replaces it. Bring the pair up exactly as the plan's Task 1
//! spike does, then:
//!
//! ```sh
//! cargo run -p nigiri-rs-core --example peg_smoke
//! ```

use bitcoin::Amount;
use nigiri_rs_core::{Bitcoin, Liquid, NigiriClient, NigiriConfig, Peg};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Deliberately NOT Nigiri's standard ports. A host Nigiri install publishes 18443, 18884,
    // 30000 and 30001, and on this machine it currently does — so publishing those from the
    // hand-wired pair fails with a port conflict. The pair publishes 28443 and 28884 instead.
    let bitcoin = NigiriClient::<Bitcoin>::with_config(NigiriConfig {
        node_rpc_url: "http://localhost:28443/".parse()?,
        ..NigiriConfig::bitcoin()
    })?;
    let liquid = NigiriClient::<Liquid>::with_config(NigiriConfig {
        node_rpc_url: "http://localhost:28884/".parse()?,
        ..NigiriConfig::liquid()
    })?;

    let peg = Peg::connect(bitcoin, liquid).await?;
    println!(
        "paired, confirmation depth {}",
        peg.pegin_confirmation_depth()
    );

    let pegged = peg.complete_peg_in(Amount::from_sat(100_000)).await?;
    println!(
        "pegged in: deposit {} claimed by {}",
        pegged.mainchain_txid, pegged.claim_txid
    );

    // No peg-out wallet setup: initpegoutwallet is rejected on this chain and sendtomainchain
    // does not need it.
    let destination = peg.bitcoin().new_address().await?.to_string();
    let peg_out = peg
        .send_to_mainchain(&destination, Amount::from_sat(10_000))
        .await?;
    println!("pegged out: {peg_out}");

    let released = peg.release_peg_out(&peg_out).await?;
    println!(
        "released {} to {} in {}",
        released.amount, released.destination, released.bitcoin_txid
    );

    assert_eq!(released.destination.to_string(), destination);
    assert_eq!(released.amount, Amount::from_sat(10_000));

    println!("peg round trip complete");
    Ok(())
}
