// `__nigiri_rs_inner` is the name the expander gives the inner fn holding the test body. Binding a
// parameter to it would shadow that fn, and the call would fail inside generated code.
#[nigiri_rs_macros::test]
async fn shadows_the_generated_inner_fn(__nigiri_rs_inner: u8) {
    let _ = __nigiri_rs_inner;
}

fn main() {}
