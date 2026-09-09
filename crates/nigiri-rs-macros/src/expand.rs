use proc_macro2::TokenStream;
use quote::quote;

use crate::parse::{FixtureParam, MacroArgs, RESERVED_PREFIX, TestFn};

pub(crate) fn expand(parsed: TestFn) -> TokenStream {
    let TestFn {
        mut item,
        fixtures,
        args,
    } = parsed;

    let attrs = std::mem::take(&mut item.attrs);
    let vis = item.vis.clone();
    let name = item.sig.ident.clone();
    let output = item.sig.output.clone();

    // The body becomes an inner async fn keeping the original parameters; the wrapper takes none.
    let inner_name = quote::format_ident!("{RESERVED_PREFIX}inner");
    let mut inner = item;
    inner.sig.ident = inner_name.clone();
    inner.vis = syn::Visibility::Inherited;

    let crate_path = args
        .crate_path
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(::nigiri_rs));
    let tokio_path = quote!(#crate_path::__private::tokio);
    let tokio_crate = syn::LitStr::new(&tokio_path.to_string(), proc_macro2::Span::call_site());
    let stacks = start_stacks(&fixtures, &args, &crate_path);

    let call_args = fixtures.iter().map(|fixture| fixture.ident());

    let runtime_attr = match &args.flavor {
        Some(flavor) => quote! { #[#tokio_path::test(flavor = #flavor, crate = #tokio_crate)] },
        None => quote! { #[#tokio_path::test(crate = #tokio_crate)] },
    };

    quote! {
        #runtime_attr
        #(#attrs)*
        #vis async fn #name() #output {
            #inner

            #stacks

            #inner_name(#(#call_args),*).await
        }
    }
}

/// Emits the `let` statements that start every requested fixture and bind its client.
///
/// Two or more fixtures start concurrently. Starting a stack is almost entirely waiting on
/// Docker and on the indexer reaching the node's tip, so overlapping that wait is close to free:
/// the project measures one Bitcoin fixture at about 3s and two at about 4.4s, where awaiting
/// them one after another costs the sum. A cross-chain test is the shape the attribute exists to
/// make easy, so it should not be the shape that pays most.
///
/// One fixture is emitted sequentially — joining a single future buys nothing — and zero
/// fixtures emit nothing at all.
fn start_stacks(
    fixtures: &[FixtureParam],
    args: &MacroArgs,
    crate_path: &syn::Path,
) -> TokenStream {
    if fixtures.len() < 2 {
        return fixtures
            .iter()
            .enumerate()
            .map(|(index, fixture)| {
                let handle = handle_ident(index);
                let start = start_expr(fixture, args, crate_path);
                let bind = bind_fixture(fixture, index);
                let failed = start_failure_message(fixture);
                quote! {
                    let #handle = #start.await.expect(#failed);
                    #bind
                }
            })
            .collect();
    }

    let futures = fixtures
        .iter()
        .map(|fixture| start_expr(fixture, args, crate_path));
    let slots = (0..fixtures.len()).map(started_ident);
    let unwrap = fixtures.iter().enumerate().map(|(index, fixture)| {
        let slot = started_ident(index);
        let handle = handle_ident(index);
        let bind = bind_fixture(fixture, index);
        let failed = start_failure_message(fixture);
        quote! {
            let #handle = #slot.expect(#failed);
            #bind
        }
    });

    quote! {
        // `join!` drives every start on this one task, so they interleave at each await rather
        // than running end to end. If one fails the others still finish; the `expect` below
        // panics on the first failure and the remaining handles drop as the panic unwinds,
        // which runs the same teardown a successful test would.
        let ( #(#slots),* ) = #crate_path::__private::tokio::join!( #(#futures),* );
        #(#unwrap)*
    }
}

fn handle_ident(index: usize) -> syn::Ident {
    quote::format_ident!("{RESERVED_PREFIX}fixture_{index}")
}

fn started_ident(index: usize) -> syn::Ident {
    quote::format_ident!("{RESERVED_PREFIX}started_{index}")
}

/// Binds what the body asked for.
///
/// A client is cloned so the fixture handle stays owned by the wrapper and keeps the containers
/// alive for the test's duration. A pair *is* that handle — it owns all of its containers and
/// clients together — so it moves into the binding instead of being cloned out of one.
fn bind_fixture(fixture: &FixtureParam, index: usize) -> TokenStream {
    let handle = handle_ident(index);
    let binding = fixture.ident();
    match fixture {
        FixtureParam::Client { .. } => quote! { let #binding = #handle.client().clone(); },
        FixtureParam::PegPair { .. } | FixtureParam::LndPair { .. } => {
            quote! { let #binding = #handle; }
        }
    }
}

fn start_expr(fixture: &FixtureParam, args: &MacroArgs, crate_path: &syn::Path) -> TokenStream {
    match fixture {
        FixtureParam::Client { chain, .. } => match args.startup_timeout {
            Some(secs) => quote! {
                #crate_path::__private::fixtures::Fixture::<#chain>::builder()
                    .startup_timeout(::core::time::Duration::from_secs(#secs))
                    .start()
            },
            None => quote! {
                #crate_path::__private::fixtures::Fixture::<#chain>::start()
            },
        },
        FixtureParam::PegPair { .. } => match args.startup_timeout {
            Some(secs) => quote! {
                #crate_path::__private::fixtures::PegPair::builder()
                    .startup_timeout(::core::time::Duration::from_secs(#secs))
                    .start()
            },
            None => quote! {
                #crate_path::__private::fixtures::PegPair::start()
            },
        },
        FixtureParam::LndPair { .. } => match args.startup_timeout {
            Some(secs) => quote! {
                #crate_path::__private::fixtures::LndPair::builder()
                    .startup_timeout(::core::time::Duration::from_secs(#secs))
                    .start()
            },
            None => quote! {
                #crate_path::__private::fixtures::LndPair::start()
            },
        },
    }
}

/// `expect` rather than `?`: a fixture that will not start is an environment failure, not a test
/// assertion, and the test's own error type need not convert from it.
///
/// The fixture is named because concurrent starts mean more than one can fail, and "the fixture"
/// would not say which.
fn start_failure_message(fixture: &FixtureParam) -> String {
    let named = match fixture {
        FixtureParam::Client { chain, .. } => chain
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
            .unwrap_or_else(|| "requested".to_owned()),
        FixtureParam::PegPair { .. } => "PegPair".to_owned(),
        FixtureParam::LndPair { .. } => "LndPair".to_owned(),
    };
    format!("nigiri-rs: the {named} fixture could not start; is Docker running?")
}
