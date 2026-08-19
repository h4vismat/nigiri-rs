//! Procedural macros that inject ready clients or owning `PegPair`/`LndPair` fixtures for `nigiri-rs`.

mod expand;
mod parse;

use proc_macro::TokenStream;

/// Provisions regtest stacks and injects ready clients or owning `PegPair`/`LndPair` handles.
///
/// See the `nigiri-rs` crate documentation for usage.
#[proc_macro_attribute]
pub fn test(args: TokenStream, item: TokenStream) -> TokenStream {
    match parse::parse(args.into(), item.into()) {
        Ok(parsed) => expand::expand(parsed).into(),
        Err(error) => error.to_compile_error().into(),
    }
}
