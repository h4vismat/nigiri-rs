use syn::{
    Error, FnArg, ItemFn, Pat, PathArguments, Result, Signature, Token, Type,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    spanned::Spanned,
};

/// Prefix every identifier the expander invents carries.
///
/// Declared here rather than in `expand.rs` because the parser rejects parameters that would
/// collide with it. The two have to agree, so they read the same constant.
pub(crate) const RESERVED_PREFIX: &str = "__nigiri_rs_";

/// The accepted fixture parameter types, named in one place.
///
/// The rejection message, the documentation, and every future composite read this. With three
/// composites in flight it would otherwise be spelled out at each site and drift.
pub(crate) const ACCEPTED_PARAMETERS: &str =
    "`NigiriClient<Bitcoin>`, `NigiriClient<Liquid>`, or `PegPair`";

/// One fixture the generated wrapper must start, derived from one function parameter.
///
/// An enum rather than a struct because a composite parameter names no chain: `PegPair` is one
/// such parameter, already landed, and `LightningStack` is the one still to come. Each adds a
/// variant here and an arm at the three match sites in `expand.rs`.
pub(crate) enum FixtureParam {
    /// `NigiriClient<C>`, whose chain marker is taken from the type.
    Client {
        ident: syn::Ident,
        /// The chain marker, e.g. `Bitcoin`, taken from `NigiriClient<Bitcoin>`.
        chain: syn::Path,
    },
    /// `PegPair`, a wired Bitcoin and Liquid stack. Names no chain: it is both.
    PegPair { ident: syn::Ident },
}

impl FixtureParam {
    /// The parameter's binding, which every variant has and the expander always needs.
    pub(crate) fn ident(&self) -> &syn::Ident {
        match self {
            Self::Client { ident, .. } | Self::PegPair { ident } => ident,
        }
    }
}

/// Arguments accepted by the attribute itself.
///
/// Deliberately excludes the chain: the parameter type already names it, and an attribute argument
/// could contradict the signature.
#[derive(Default)]
pub(crate) struct MacroArgs {
    pub(crate) startup_timeout: Option<u64>,
    pub(crate) flavor: Option<syn::LitStr>,
}

impl Parse for MacroArgs {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut args = MacroArgs::default();
        let pairs = Punctuated::<syn::MetaNameValue, Token![,]>::parse_terminated(input)?;

        for pair in pairs {
            let name = pair
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();

            match name.as_str() {
                "startup_timeout" => {
                    let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Int(int),
                        ..
                    }) = &pair.value
                    else {
                        return Err(Error::new(
                            pair.value.span(),
                            "`startup_timeout` takes a number of seconds, e.g. \
                             `#[nigiri_rs::test(startup_timeout = 120)]`",
                        ));
                    };
                    args.startup_timeout = Some(int.base10_parse()?);
                }
                "flavor" => {
                    let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(string),
                        ..
                    }) = &pair.value
                    else {
                        return Err(Error::new(
                            pair.value.span(),
                            "`flavor` takes a string, e.g. `flavor = \"multi_thread\"`",
                        ));
                    };
                    args.flavor = Some(string.clone());
                }
                other => {
                    return Err(Error::new(
                        pair.path.span(),
                        format!(
                            "unknown argument `{other}`; \
                             `#[nigiri_rs::test]` accepts `startup_timeout` and `flavor`. \
                             The chain is taken from the parameter type, not from an argument."
                        ),
                    ));
                }
            }
        }

        Ok(args)
    }
}

pub(crate) struct TestFn {
    pub(crate) item: ItemFn,
    pub(crate) fixtures: Vec<FixtureParam>,
    pub(crate) args: MacroArgs,
}

pub(crate) fn parse(
    args: proc_macro2::TokenStream,
    item: proc_macro2::TokenStream,
) -> Result<TestFn> {
    let args: MacroArgs = syn::parse2(args)?;
    let item: ItemFn = syn::parse2(item)?;

    check_async(&item.sig)?;
    check_not_generic(&item.sig)?;

    let mut fixtures = Vec::new();
    for arg in &item.sig.inputs {
        fixtures.push(fixture_param(arg)?);
    }
    // The signature is left intact here. The expander builds a parameterless wrapper around the
    // original function, which keeps its own parameters as the inner fn — so nothing needs
    // stripping at parse time.

    Ok(TestFn {
        item,
        fixtures,
        args,
    })
}

fn check_async(sig: &Signature) -> Result<()> {
    if sig.asyncness.is_none() {
        return Err(Error::new(
            sig.fn_token.span(),
            "`#[nigiri_rs::test]` requires an `async fn`: starting a fixture awaits Docker",
        ));
    }
    Ok(())
}

/// Rejects a generic signature before the expander can produce a worse error than this one.
///
/// The wrapper is emitted parameterless and without generics, so it would call a still-generic
/// inner fn with nothing to infer from. rustc then reports "type annotations needed" against the
/// attribute rather than against the signature, which says nothing about what to change. A test
/// harness cannot supply a type argument anyway, so there is no shape to support here — only a
/// diagnostic to get right.
fn check_not_generic(sig: &Signature) -> Result<()> {
    if let Some(param) = sig.generics.params.first() {
        return Err(Error::new(
            param.span(),
            "`#[nigiri_rs::test]` cannot be applied to a generic function: the test harness has \
             no way to choose the type arguments",
        ));
    }
    if let Some(clause) = &sig.generics.where_clause {
        return Err(Error::new(
            clause.span(),
            "`#[nigiri_rs::test]` cannot be applied to a function with a `where` clause: the \
             test harness has no way to satisfy it",
        ));
    }
    Ok(())
}

/// Reads one parameter as a fixture request, or explains why it cannot be one.
fn fixture_param(arg: &FnArg) -> Result<FixtureParam> {
    let FnArg::Typed(typed) = arg else {
        return Err(Error::new(
            arg.span(),
            "`#[nigiri_rs::test]` cannot be applied to a method taking `self`",
        ));
    };

    let Pat::Ident(pat) = &*typed.pat else {
        return Err(Error::new(
            typed.pat.span(),
            "each parameter must be a plain name, so the generated wrapper can bind it",
        ));
    };

    // The wrapper binds each parameter as a local beside its own `__nigiri_rs_*` items. A
    // parameter spelling one of those shadows it, and the failure lands on generated code the
    // author never wrote: naming a parameter `__nigiri_rs_inner` shadows the inner fn, so calling
    // it reports `expected function, found struct NigiriClient`. Reserving the prefix costs a
    // consumer nothing and turns that into a sentence about their own signature.
    if pat.ident.to_string().starts_with(RESERVED_PREFIX) {
        return Err(Error::new(
            pat.ident.span(),
            format!(
                "parameter names beginning `{RESERVED_PREFIX}` are reserved for the code \
                 `#[nigiri_rs::test]` generates; rename this parameter"
            ),
        ));
    }

    if is_peg_pair(&typed.ty) {
        return Ok(FixtureParam::PegPair {
            ident: pat.ident.clone(),
        });
    }

    let chain = chain_of(&typed.ty)?;

    Ok(FixtureParam::Client {
        ident: pat.ident.clone(),
        chain,
    })
}

/// Extracts `C` from `NigiriClient<C>`.
///
/// Matched on the last path segment rather than the full path, so `nigiri_rs::NigiriClient<Bitcoin>`
/// and a bare `NigiriClient<Bitcoin>` both work — a consumer may have imported it either way.
fn chain_of(ty: &Type) -> Result<syn::Path> {
    let unsupported = || {
        Error::new(
            ty.span(),
            format!(
                "`#[nigiri_rs::test]` parameters must be {ACCEPTED_PARAMETERS}; \
                 the chain is taken from this type"
            ),
        )
    };

    let Type::Path(path) = ty else {
        return Err(unsupported());
    };
    let segment = path.path.segments.last().ok_or_else(unsupported)?;
    if segment.ident != "NigiriClient" {
        return Err(unsupported());
    }

    let PathArguments::AngleBracketed(generics) = &segment.arguments else {
        return Err(unsupported());
    };
    let Some(syn::GenericArgument::Type(Type::Path(chain))) = generics.args.first() else {
        return Err(unsupported());
    };

    Ok(chain.path.clone())
}

/// Whether a parameter names the wired pair.
///
/// Matched on the last path segment, like [`chain_of`], so `PegPair` and
/// `nigiri_rs::testcontainers::PegPair` both work. The exported type takes no generic arguments, so
/// a `PegPair<…>` is something else and falls through to `chain_of`'s rejection rather than being
/// accepted and expanded into code that cannot compile.
fn is_peg_pair(ty: &Type) -> bool {
    let Type::Path(path) = ty else {
        return false;
    };

    path.path.segments.last().is_some_and(|segment| {
        segment.ident == "PegPair" && matches!(segment.arguments, PathArguments::None)
    })
}

#[cfg(test)]
mod tests {
    use super::{ACCEPTED_PARAMETERS, FixtureParam, parse};

    // Catches a regression that stops deriving the chain from the parameter type, and pins the enum
    // shape the composites add variants to.
    #[test]
    fn a_client_parameter_parses_into_the_client_variant() {
        let parsed = parse(
            proc_macro2::TokenStream::new(),
            quote::quote! {
                async fn a_test(bitcoin: NigiriClient<Bitcoin>) {}
            },
        )
        .expect("a NigiriClient parameter is accepted");

        assert_eq!(parsed.fixtures.len(), 1);
        let FixtureParam::Client { ident, chain } = &parsed.fixtures[0] else {
            panic!("a NigiriClient parameter must parse into the client variant");
        };
        assert_eq!(ident.to_string(), "bitcoin");
        assert_eq!(
            chain
                .segments
                .last()
                .expect("the chain path has a segment")
                .ident
                .to_string(),
            "Bitcoin"
        );
    }

    // Catches a regression that spells the accepted-parameter list out at a second call site, which
    // is how the message drifts once PegPair and LightningStack are added to it.
    #[test]
    fn the_rejection_message_names_the_accepted_parameters_from_one_source() {
        // `expect_err` would require `TestFn: Debug`, which it deliberately does not derive; match
        // instead.
        let error = match parse(
            proc_macro2::TokenStream::new(),
            quote::quote! {
                async fn a_test(unsupported: String) {}
            },
        ) {
            Ok(_) => panic!("a String parameter is not a fixture"),
            Err(error) => error,
        };

        assert!(error.to_string().contains(ACCEPTED_PARAMETERS), "{error}");
    }

    // Catches a regression that stops recognizing the wired pair, or that tries to read a chain out
    // of a parameter that names none.
    #[test]
    fn a_peg_pair_parameter_parses_into_the_pair_variant() {
        for signature in [
            quote::quote! { async fn a_test(peg: PegPair) {} },
            quote::quote! { async fn a_test(peg: nigiri_rs::testcontainers::PegPair) {} },
        ] {
            let parsed = parse(proc_macro2::TokenStream::new(), signature)
                .expect("a PegPair parameter is accepted");

            assert_eq!(parsed.fixtures.len(), 1);
            let FixtureParam::PegPair { ident } = &parsed.fixtures[0] else {
                panic!("a PegPair parameter must parse into the pair variant");
            };
            assert_eq!(ident.to_string(), "peg");
        }
    }

    // Catches a regression that accepts a generic `PegPair<…>`, which is not the type this crate
    // exports and would expand into code that cannot compile.
    #[test]
    fn a_generic_peg_pair_is_rejected_with_the_accepted_list() {
        let error = match parse(
            proc_macro2::TokenStream::new(),
            quote::quote! {
                async fn a_test(peg: PegPair<Bitcoin>) {}
            },
        ) {
            Ok(_) => panic!("`PegPair<Bitcoin>` is not an accepted parameter"),
            Err(error) => error,
        };

        assert!(error.to_string().contains(ACCEPTED_PARAMETERS), "{error}");
    }

    // Catches a regression that drops the pair from the one place the accepted list is spelled.
    #[test]
    fn the_accepted_list_names_the_pair() {
        assert!(
            ACCEPTED_PARAMETERS.contains("PegPair"),
            "{ACCEPTED_PARAMETERS}"
        );
    }
}
