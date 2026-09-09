//! Consumer regression: only the facade is a dependency; Tokio must resolve through it.

#![cfg(test)]

#[regtest::test(crate = "::regtest")]
async fn facade_supplies_the_runtime() {
    assert_eq!(async { 2 + 2 }.await, 4);
}
