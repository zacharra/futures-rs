//! The futures-rs `select! macro implementation.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::{parenthesized, parse_quote, Expr, Ident, Pat, Token};
use syn::parse::{Parse, ParseStream};

mod kw {
    syn::custom_keyword!(complete);
    syn::custom_keyword!(futures_crate_path);
}

struct Select {
    futures_crate_path: Option<syn::Path>,
    // span of `complete`, then expression after `=> ...`
    complete: Option<Expr>,
    default: Option<Expr>,
    normal_fut_exprs: Vec<Expr>,
    normal_fut_handlers: Vec<(Pat, Expr)>,
}

#[allow(clippy::large_enum_variant)]
enum CaseKind {
    Complete,
    Default,
    Normal(Pat, Expr),
}

impl Parse for Select {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut select = Select {
            futures_crate_path: None,
            complete: None,
            default: None,
            normal_fut_exprs: vec![],
            normal_fut_handlers: vec![],
        };

        // When `futures_crate_path(::path::to::futures::lib)` is provided,
        // it sets the path through which futures library functions will be
        // accessed.
        if input.peek(kw::futures_crate_path) {
            input.parse::<kw::futures_crate_path>()?;
            let content;
            parenthesized!(content in input);
            select.futures_crate_path = Some(content.parse()?);
        }

        while !input.is_empty() {
            let case_kind = if input.peek(kw::complete) {
                // `complete`
                if select.complete.is_some() {
                    return Err(input.error("multiple `complete` cases found, only one allowed"));
                }
                input.parse::<kw::complete>()?;
                CaseKind::Complete
            } else if input.peek(Token![default]) {
                // `default`
                if select.default.is_some() {
                    return Err(input.error("multiple `default` cases found, only one allowed"));
                }
                input.parse::<Ident>()?;
                CaseKind::Default
            } else {
                // `<pat> = <expr>`
                let pat = input.parse()?;
                input.parse::<Token![=]>()?;
                let expr = input.parse()?;
                CaseKind::Normal(pat, expr)
            };

            // `=> <expr>`
            input.parse::<Token![=>]>()?;
            let expr = input.parse::<Expr>()?;

            // Commas after the expression are only optional if it's a `Block`
            // or it is the last branch in the `match`.
            let is_block = match expr { Expr::Block(_) => true, _ => false };
            if is_block || input.is_empty() {
                input.parse::<Option<Token![,]>>()?;
            } else {
                input.parse::<Token![,]>()?;
            }

            match case_kind {
                CaseKind::Complete => select.complete = Some(expr),
                CaseKind::Default => select.default = Some(expr),
                CaseKind::Normal(pat, fut_expr) => {
                    select.normal_fut_exprs.push(fut_expr);
                    select.normal_fut_handlers.push((pat, expr));
                },
            }
        }

        Ok(select)
    }
}

// Enum over all the cases in which the `select!` waiting has completed and the result
// can be processed.
//
// `enum __PrivResult<_1, _2, ...> { _1(_1), _2(_2), ..., Complete }`
fn declare_result_enum(
    result_ident: Ident,
    variants: usize,
    complete: bool,
    span: Span
) -> (Vec<Ident>, syn::ItemEnum) {
    // "_0", "_1", "_2"
    let variant_names: Vec<Ident> =
        (0..variants)
            .map(|num| format_ident!("_{}", num, span = span))
            .collect();

    let type_parameters = &variant_names;
    let variants = &variant_names;

    let complete_variant = if complete {
        Some(quote!(Complete))
    } else {
        None
    };

    let enum_item = parse_quote! {
        enum #result_ident<#(#type_parameters,)*> {
            #(
                #variants(#type_parameters),
            )*
            #complete_variant
        }
    };

    (variant_names, enum_item)
}

/// The `select!` macro.
pub(crate) fn select(input: TokenStream) -> TokenStream {
    select_inner(input, true)
}

/// The `select_biased!` macro.
pub(crate) fn select_biased(input: TokenStream) -> TokenStream {
    select_inner(input, false)
}

fn select_inner(input: TokenStream, random: bool) -> TokenStream {
    let parsed = syn::parse_macro_input!(input as Select);

    let futures_crate: syn::Path = parsed.futures_crate_path.unwrap_or_else(|| parse_quote!(::futures_util));

    // should be def_site, but that's unstable
    let span = Span::call_site();

    let enum_ident = Ident::new("__PrivResult", span);

    let (variant_names, enum_item) = declare_result_enum(
        enum_ident.clone(),
        parsed.normal_fut_exprs.len(),
        parsed.complete.is_some(),
        span,
    );

    // bind non-`Ident` future exprs w/ `let`
    let mut future_let_bindings = Vec::with_capacity(parsed.normal_fut_exprs.len());
    let bound_future_names: Vec<_> = parsed.normal_fut_exprs.into_iter()
        .zip(variant_names.iter())
        .map(|(expr, variant_name)| {
            match expr {
                syn::Expr::Path(path) => {
                    // Don't bind futures that are already a path.
                    // This prevents creating redundant stack space
                    // for them.
                    // Passing Futures by path requires those Futures to implement Unpin.
                    // We check for this condition here in order to be able to
                    // safely use Pin::new_unchecked(&mut #path) later on.
                    future_let_bindings.push(quote! {
                        #futures_crate::async_await::assert_fused_future(&mut #path);
                        #futures_crate::async_await::assert_unpin(&mut #path);
                    });
                    path
                },
                _ => {
                    // Bind and pin the resulting Future on the stack. This is
                    // necessary to support direct select! calls on !Unpin
                    // Futures. The Future is not explicitly pinned here with
                    // a Pin call, but assumed as pinned. The actual Pin is
                    // created inside the poll() function below to defer the
                    // creation of the temporary pointer, which would otherwise
                    // increase the size of the generated Future.
                    // Safety: This is safe since the lifetime of the Future
                    // is totally constraint to the lifetime of the select!
                    // expression, and the Future can't get moved inside it
                    // (it is shadowed).
                    future_let_bindings.push(quote! {
                        let mut #variant_name = #expr;
                    });
                    parse_quote! { #variant_name }
                }
            }
        })
        .collect();

    // For each future, make an `&mut dyn FnMut(&mut Context<'_>) -> Option<Poll<__PrivResult<...>>`
    // to use for polling that individual future. These will then be put in an array.
    let poll_functions = bound_future_names.iter().zip(variant_names.iter())
        .map(|(bound_future_name, variant_name)| {
            // Below we lazily create the Pin on the Future below.
            // This is done in order to avoid allocating memory in the generator
            // for the Pin variable.
            // Safety: This is safe because one of the following condition applies:
            // 1. The Future is passed by the caller by name, and we assert that
            //    it implements Unpin.
            // 2. The Future is created in scope of the select! function and will
            //    not be moved for the duration of it. It is thereby stack-pinned
            quote! {
                let mut #variant_name = |__cx: &mut #futures_crate::task::Context<'_>| {
                    let mut #bound_future_name = unsafe {
                        ::core::pin::Pin::new_unchecked(&mut #bound_future_name)
                    };
                    if #futures_crate::future::FusedFuture::is_terminated(&#bound_future_name) {
                        None
                    } else {
                        Some(#futures_crate::future::FutureExt::poll_unpin(
                            &mut #bound_future_name,
                            __cx,
                        ).map(#enum_ident::#variant_name))
                    }
                };
                let #variant_name: &mut dyn FnMut(
                    &mut #futures_crate::task::Context<'_>
                ) -> Option<#futures_crate::task::Poll<_>> = &mut #variant_name;
            }
        });

    let none_polled = if parsed.complete.is_some() {
        quote! {
            #futures_crate::task::Poll::Ready(#enum_ident::Complete)
        }
    } else {
        quote! {
            panic!("all futures in select! were completed,\
                    but no `complete =>` handler was provided")
        }
    };

    let branches = parsed.normal_fut_handlers.into_iter()
        .zip(variant_names.iter())
        .map(|((pat, expr), variant_name)| {
            quote! {
                #enum_ident::#variant_name(#pat) => { #expr },
            }
        });
    let branches = quote! { #( #branches )* };

    let complete_branch = parsed.complete.map(|complete_expr| {
        quote! {
            #enum_ident::Complete => { #complete_expr },
        }
    });

    let branches = quote! {
        #branches
        #complete_branch
    };

    let await_select_fut = if parsed.default.is_some() {
        // For select! with default this returns the Poll result
        quote! {
            __poll_fn(&mut #futures_crate::task::Context::from_waker(
                #futures_crate::task::noop_waker_ref()
            ))
        }
    } else {
        quote! {
            #futures_crate::future::poll_fn(__poll_fn).await
        }
    };

    let execute_result_expr = if let Some(default_expr) = &parsed.default {
        // For select! with default __select_result is a Poll, otherwise not
        quote! {
            match __select_result {
                #futures_crate::task::Poll::Ready(result) => match result {
                    #branches
                },
                _ => #default_expr
            }
        }
    } else {
        quote! {
            match __select_result {
                #branches
            }
        }
    };

    let shuffle = if random {
        quote! {
            #futures_crate::async_await::shuffle(&mut __select_arr);
        }
    } else {
        quote!()
    };

    TokenStream::from(quote! { {
        #enum_item

        let __select_result = {
            #( #future_let_bindings )*

            let mut __poll_fn = |__cx: &mut #futures_crate::task::Context<'_>| {
                let mut __any_polled = false;

                #( #poll_functions )*

                let mut __select_arr = [#( #variant_names ),*];
                #shuffle
                for poller in &mut __select_arr {
                    let poller: &mut &mut dyn FnMut(
                        &mut #futures_crate::task::Context<'_>
                    ) -> Option<#futures_crate::task::Poll<_>> = poller;
                    match poller(__cx) {
                        Some(x @ #futures_crate::task::Poll::Ready(_)) =>
                            return x,
                        Some(#futures_crate::task::Poll::Pending) => {
                            __any_polled = true;
                        }
                        None => {}
                    }
                }

                if !__any_polled {
                    #none_polled
                } else {
                    #futures_crate::task::Poll::Pending
                }
            };

            #await_select_fut
        };

        #execute_result_expr
    } })
}
