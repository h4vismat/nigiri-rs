#[nigiri_rs_macros::test]
async fn takes_a_type_parameter<T: Default>() {
    let _ = T::default();
}

fn main() {}
