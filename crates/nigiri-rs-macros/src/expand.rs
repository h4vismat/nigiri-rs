use proc_macro2::TokenStream;
use quote::quote;

use crate::parse::TestFn;

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
    let inner_name = quote::format_ident!("__nigiri_rs_inner");
    let mut inner = item;
    inner.sig.ident = inner_name.clone();
    inner.vis = syn::Visibility::Inherited;

    // One pass per fixture, emitting its start and its binding together. Deriving the handle
    // identifier in two separate passes would leave the two `format_ident!` calls having to
    // agree by coincidence; a divergence would surface as an unresolved name deep inside
    // generated code, pointing at nothing the author wrote.
    let stacks = fixtures.iter().enumerate().map(|(index, fixture)| {
        let handle = quote::format_ident!("__nigiri_rs_fixture_{index}");
        let chain = &fixture.chain;
        let binding = &fixture.ident;
        let builder = match args.startup_timeout {
            Some(secs) => quote! {
                ::nigiri_rs::__private::testcontainers::Fixture::<#chain>::builder()
                    .startup_timeout(::core::time::Duration::from_secs(#secs))
                    .start()
            },
            None => quote! {
                ::nigiri_rs::__private::testcontainers::Fixture::<#chain>::start()
            },
        };
        quote! {
            // `expect` rather than `?`: a fixture that will not start is an environment failure,
            // not a test assertion, and the test's own error type need not convert from it.
            let #handle = #builder
                .await
                .expect("nigiri-rs: the fixture could not start; is Docker running?");
            // The client is cloned so the fixture stays owned by the wrapper and outlives the
            // body, which is what keeps the containers alive for the test's duration.
            let #binding = #handle.client().clone();
        }
    });

    let call_args = fixtures.iter().map(|fixture| &fixture.ident);

    let runtime_attr = match &args.flavor {
        Some(flavor) => quote! { #[::nigiri_rs::__private::tokio::test(flavor = #flavor)] },
        None => quote! { #[::nigiri_rs::__private::tokio::test] },
    };

    quote! {
        #runtime_attr
        #(#attrs)*
        #vis async fn #name() #output {
            #inner

            #(#stacks)*

            #inner_name(#(#call_args),*).await
        }
    }
}
