#[nigiri_rs_macros::test(startup_timeout = "two minutes")]
async fn timeout_is_not_a_number() {}

fn main() {}
