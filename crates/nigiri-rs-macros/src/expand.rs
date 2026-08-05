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

    let stacks = start_stacks(&fixtures, &args);

    let call_args = fixtures.iter().map(|fixture| fixture.ident());

    let runtime_attr = match &args.flavor {
        Some(flavor) => quote! { #[::nigiri_rs::__private::tokio::test(flavor = #flavor)] },
        None => quote! { #[::nigiri_rs::__private::tokio::test] },
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
fn start_stacks(fixtures: &[FixtureParam], args: &MacroArgs) -> TokenStream {
    if fixtures.len() < 2 {
        return fixtures
            .iter()
            .enumerate()
            .map(|(index, fixture)| {
                let handle = handle_ident(index);
                let start = start_expr(fixture, args);
                let bind = bind_client(fixture, index);
                let failed = start_failure_message(fixture);
                quote! {
                    let #handle = #start.await.expect(#failed);
                    #bind
                }
            })
            .collect();
    }

    let futures = fixtures.iter().map(|fixture| start_expr(fixture, args));
    let slots = (0..fixtures.len()).map(started_ident);
    let unwrap = fixtures.iter().enumerate().map(|(index, fixture)| {
        let slot = started_ident(index);
        let handle = handle_ident(index);
        let bind = bind_client(fixture, index);
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
        let ( #(#slots),* ) = ::nigiri_rs::__private::tokio::join!( #(#futures),* );
        #(#unwrap)*
    }
}

fn handle_ident(index: usize) -> syn::Ident {
    quote::format_ident!("{RESERVED_PREFIX}fixture_{index}")
}

fn started_ident(index: usize) -> syn::Ident {
    quote::format_ident!("{RESERVED_PREFIX}started_{index}")
}

/// The client is cloned so the fixture stays owned by the wrapper and outlives the body, which is
/// what keeps the containers alive for the test's duration.
fn bind_client(fixture: &FixtureParam, index: usize) -> TokenStream {
    let handle = handle_ident(index);
    let binding = fixture.ident();
    match fixture {
        FixtureParam::Client { .. } => quote! { let #binding = #handle.client().clone(); },
    }
}

fn start_expr(fixture: &FixtureParam, args: &MacroArgs) -> TokenStream {
    match fixture {
        FixtureParam::Client { chain, .. } => match args.startup_timeout {
            Some(secs) => quote! {
                ::nigiri_rs::__private::testcontainers::Fixture::<#chain>::builder()
                    .startup_timeout(::core::time::Duration::from_secs(#secs))
                    .start()
            },
            None => quote! {
                ::nigiri_rs::__private::testcontainers::Fixture::<#chain>::start()
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
    };
    format!("nigiri-rs: the {named} fixture could not start; is Docker running?")
}
