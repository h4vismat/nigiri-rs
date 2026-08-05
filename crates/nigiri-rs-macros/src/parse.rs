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

/// One fixture the generated wrapper must start, derived from one function parameter.
pub(crate) struct FixtureParam {
    pub(crate) ident: syn::Ident,
    /// The chain marker, e.g. `Bitcoin`, taken from `NigiriClient<Bitcoin>`.
    pub(crate) chain: syn::Path,
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

    let chain = chain_of(&typed.ty)?;

    Ok(FixtureParam {
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
            "`#[nigiri_rs::test]` parameters must be `NigiriClient<Bitcoin>` or \
             `NigiriClient<Liquid>`; the chain is taken from this type",
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
