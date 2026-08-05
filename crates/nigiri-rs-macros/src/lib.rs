//! Procedural macros for `nigiri-rs`.

mod parse;

use proc_macro::TokenStream;

/// Provisions a regtest stack for a test and injects a ready client.
///
/// See the `nigiri-rs` crate documentation for usage.
#[proc_macro_attribute]
pub fn test(args: TokenStream, item: TokenStream) -> TokenStream {
    match parse::parse(args.into(), item.into()) {
        Ok(_parsed) => TokenStream::new(),
        Err(error) => error.to_compile_error().into(),
    }
}
