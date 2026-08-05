//! The macro's generated code reaches everything through `nigiri_rs::__private`. If a re-export
//! here disappears, expansions break in the consumer's crate with an error pointing at code they
//! did not write — so the paths are pinned here, where the failure is local and legible.

#![cfg(feature = "testcontainers")]

// Catches a dropped re-export in the facade's private surface. Each path below appears verbatim in
// generated code; naming them in type position forces the compiler to resolve them.
#[test]
fn generated_code_paths_resolve() {
    fn accepts<T>() {}

    accepts::<nigiri_rs::__private::testcontainers::Fixture<nigiri_rs::Bitcoin>>();
    accepts::<nigiri_rs::__private::testcontainers::FixtureError>();

    // The generated wrapper is annotated `#[::nigiri_rs::__private::tokio::test]`, so the runtime
    // must be reachable by that path too.
    let _runtime_is_reachable = nigiri_rs::__private::tokio::runtime::Builder::new_current_thread();
}
