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

    let starts = fixtures.iter().enumerate().map(|(index, fixture)| {
        let handle = quote::format_ident!("__nigiri_rs_fixture_{index}");
        let chain = &fixture.chain;
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
        }
    });

    let bindings = fixtures.iter().enumerate().map(|(index, fixture)| {
        let handle = quote::format_ident!("__nigiri_rs_fixture_{index}");
        let binding = &fixture.ident;
        // The client is cloned so the fixture stays owned by the wrapper and outlives the body,
        // which is what keeps the containers alive for the test's duration.
        quote! { let #binding = #handle.client().clone(); }
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

            #(#starts)*
            #(#bindings)*

            #inner_name(#(#call_args),*).await
        }
    }
}
