//! What `Peg::connect` can and cannot prove, pinned against two real nodes.
//!
//! `Peg::connect` compares the Liquid node's reported parent block hash against the Bitcoin node's
//! genesis. This file exists because the strength of that check is not obvious from reading it, and
//! the peg documentation makes a claim about it that only a container run can settle.

use nigiri_rs_core::Peg;
use nigiri_rs_testcontainers::{Bitcoin, Fixture, Liquid};

// Catches a regression that turns `Peg::connect` into a weaker or stronger check than the docs
// claim. Bitcoin's regtest genesis is hardcoded in chainparams — the same value on every node,
// never generated per instance — and `liquidregtest` carries it as a chain parameter. So two
// fixtures that have never heard of each other still agree on the parent chain, and the comparison
// cannot tell "wired" from "not wired": it only catches a Liquid node built for a *different*
// parent chain. `PegPair` is what guarantees wiring; this pins the honest limit of the check.
#[tokio::test]
async fn connect_cannot_tell_two_independent_fixtures_from_a_wired_pair() {
    let bitcoin = Fixture::<Bitcoin>::start()
        .await
        .expect("a pinned Bitcoin fixture must start against a real daemon");
    let liquid = Fixture::<Liquid>::start()
        .await
        .expect("a pinned Liquid fixture must start against a real daemon");

    let paired = Peg::connect(bitcoin.client().clone(), liquid.client().clone()).await;

    match paired {
        Ok(peg) => {
            println!(
                "connect accepted two independent fixtures; reported depth {}",
                peg.pegin_confirmation_depth()
            );
        }
        Err(error) => panic!(
            "if this now rejects an unwired pair, the check is stronger than recorded — update \
             this test and the `Peg::connect` documentation together: {error}"
        ),
    }
}
