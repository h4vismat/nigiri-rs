# How to point a wallet at a fixture

Get the Esplora URL and the Electrum host and port that a BDK (Bitcoin) or LWK (Liquid) wallet needs,
from a client that is already pointed at a running chain.

This is the reason fixtures exist: a wallet test that talks to a real node and a real indexer, on a
chain nothing else can see.

## Prerequisites

- A `NigiriClient<N>`, from a fixture or from services you run.
- Your wallet library. `nigiri-rs` does **not** depend on BDK or LWK and does not build the
  connection for you — it gives you the two endpoints and stays out of the way.

## Read the endpoints

Both accessors live on the client, so it makes no difference whether it came from a fixture:

```rust,ignore
use nigiri_rs::{Bitcoin, NigiriClient};

fn endpoints(client: &NigiriClient<Bitcoin>) {
    let esplora = client.esplora_url();          // &Url, always ends in '/'
    let electrum = client.electrum_endpoint();   // &ElectrumEndpoint

    println!("esplora:  {esplora}");
    println!("electrum: {}:{}", electrum.host(), electrum.port());
}
```

`ElectrumEndpoint` keeps host and port separate rather than handing you a string. Electrum is a raw
TCP protocol — there is no scheme to normalize and no path to preserve, and every client wants the
two parts in a slightly different shape.

Build whatever string yours expects:

```rust,ignore
let electrum = client.electrum_endpoint();
let plain = format!("{}:{}", electrum.host(), electrum.port());   // localhost:32773
let tcp_url = format!("tcp://{}:{}", electrum.host(), electrum.port());
```

A fixture's services are plaintext on loopback. Do not prefix `ssl://`.

## Never hardcode the ports

```rust,ignore
// Wrong against a fixture. Right only against a host-owned Nigiri.
let electrum = "localhost:50000";
```

Nigiri publishes fixed ports — 30000 for Bitcoin Esplora, 50000 for Bitcoin Electrum, 30001 and 50001
for Liquid — and a lot of example code hardcodes them. **A fixture cannot use them on the host
side.** It lets Docker assign a free host port for each service so two fixtures can run at once, so
the numbers differ every run.

Reading them from the client is correct in both worlds: against a host-owned Nigiri the client hands
back Nigiri's fixed ports anyway.

## Worked example: a wallet test

```rust,ignore
use nigiri_rs::{Bitcoin, NigiriClient};

type BoxError = Box<dyn std::error::Error>;

#[nigiri_rs::test]
async fn my_wallet_sees_its_funding(client: NigiriClient<Bitcoin>) -> Result<(), BoxError> {
    // 1. Build your wallet against the fixture's endpoints.
    let esplora_url = client.esplora_url().to_string();
    let mut wallet = my_wallet_crate::Wallet::new_esplora(&esplora_url)?;

    // 2. Fund an address the wallet owns. `faucet` mines one block, so this
    //    is confirmed by the time it returns.
    let address = wallet.next_address()?;
    let txid = client.faucet(&address.to_string(), None).await?;

    // 3. Sync and assert.
    wallet.sync().await?;
    assert_eq!(wallet.balance(), 100_000_000);   // `None` sends exactly 1 BTC
    assert!(wallet.transactions().any(|tx| tx.txid == txid));

    Ok(())
}
```

`my_wallet_crate` is a placeholder — substitute BDK, LWK, or your own.

### Verification

If the wallet's balance matches and the txid is in its transaction list, the wallet is genuinely
talking to the fixture's indexer. A wallet still pointed at a default `localhost:50000` will time out
or connect to something else entirely, so a passing balance assertion is real evidence.

## Liquid

Identical, with the Liquid marker:

```rust,ignore
use nigiri_rs::{Liquid, NigiriClient};

#[nigiri_rs::test]
async fn lwk_sees_its_funding(client: NigiriClient<Liquid>) -> Result<(), BoxError> {
    let electrum = client.electrum_endpoint();
    let wallet = my_lwk_setup(electrum.host(), electrum.port())?;
    // ...
    Ok(())
}
```

Two Liquid specifics worth knowing before you assert on balances:

- **Addresses stay `&str`.** Every address argument on the client takes `&str` rather than a native
  address type, which avoids a conversion for LWK's confidential addresses.
- **A fixture's wallet is rich.** It connects the genesis outputs, so it holds the full 21,000,000
  L-BTC of free coins. A running Nigiri Liquid node does not — its wallet reports zero. Same chain,
  same genesis; only the fixture is funded.

## Sync timing

`faucet` and `broadcast_tx` each mine one block before returning, so the transaction is confirmed on
the **node** by the time you get the txid.

The indexer is a separate story. A fixture guarantees node, Esplora, and Electrum agree on the tip
**once, at startup**. Blocks mined afterwards reach the indexer on its own schedule, usually within a
second or two. If your wallet syncs through Esplora or Electrum and sees nothing, poll rather than
sleeping:

```rust,ignore
use std::time::Duration;

let txid = client.faucet(&address, None).await?;
client.wait_for_confirmation(&txid, Duration::from_secs(30)).await?;
wallet.sync().await?;
```

`wait_for_confirmation` polls Esplora every 500 ms, which is exactly the service most wallets read.

## Related

- [Client API reference](reference-client.md) — `esplora_url`, `electrum_endpoint`,
  `ElectrumEndpoint`
- [How to run a throwaway regtest stack](how-to-run-a-fixture.md)
- [What "ready" means](explanation-fixture-readiness.md) — why startup agreement is a one-time
  guarantee
