//! Drives a full peg round trip against a hand-wired Elements and bitcoind pair.
//!
//! Not a test: it needs two containers this crate does not start, because `nigiri-rs-core` starts
//! nothing. Bring the pair up first:
//!
//! ```sh
//! docker network create pegsmoke-net
//!
//! docker run -d --name pegsmoke-btc --network pegsmoke-net \
//!     -p 28443:18443 \
//!     ghcr.io/getumbrel/docker-bitcoind:v30.0 \
//!     -chain=regtest -server=1 -txindex=1 \
//!     -rpcbind=0.0.0.0:18443 -rpcallowip=0.0.0.0/0 \
//!     -rpcuser=admin1 -rpcpassword=123 -fallbackfee=0.00001 -printtoconsole=1
//!
//! docker run -d --name pegsmoke-lqd --network pegsmoke-net \
//!     -p 28884:18884 \
//!     --entrypoint elementsd \
//!     blockstream/elementsd:23.3.3 \
//!     -chain=liquidregtest -server=1 -txindex=1 \
//!     -rpcbind=0.0.0.0:18884 -rpcallowip=0.0.0.0/0 -rpcport=18884 \
//!     -rpcuser=admin1 -rpcpassword=123 \
//!     -validatepegin=1 \
//!     -mainchainrpchost=pegsmoke-btc -mainchainrpcport=18443 \
//!     -mainchainrpcuser=admin1 -mainchainrpcpassword=123 \
//!     -initialfreecoins=2100000000000000 -fallbackfee=0.000001 \
//!     -con_connect_genesis_outputs=1 -printtoconsole=1
//! ```
//!
//! Then, against each node's RPC, create wallets and fund the Bitcoin side:
//!
//! ```text
//! lqd createwallet peg
//! lqd rescanblockchain 0
//! lqd generatetoaddress 1 <a new Liquid address>   # see note below
//!
//! btc createwallet smoke
//! btc generatetoaddress 101 <a new Bitcoin address>
//! ```
//!
//! The Liquid block mined after `rescanblockchain 0` matters: with `-validatepegin=1`, the node
//! reports `initialblockdownload: true` at height 0 and refuses `getpeginaddress` with `"This
//! action cannot be completed during initial sync or reindexing."` until a block is mined.
//!
//! Then run this example:
//!
//! ```sh
//! cargo run -p nigiri-rs-core --example peg_smoke
//! ```

use bitcoin::Amount;
use nigiri_rs_core::{Bitcoin, Liquid, NigiriClient, NigiriConfig, Peg};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Deliberately NOT Nigiri's standard ports. A host Nigiri install publishes 18443, 18884,
    // 30000 and 30001, so publishing those same ports from the hand-wired pair would risk a
    // conflict with one. The pair publishes 28443 and 28884 instead.
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
