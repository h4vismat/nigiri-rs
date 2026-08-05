//! Compile-fail tests for `#[nigiri_rs::test]`.
//!
//! A proc macro's error messages have no other gate: they are not exercised by ordinary tests and
//! degrade silently. These cases pin the wording and the span so a regression is visible.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
