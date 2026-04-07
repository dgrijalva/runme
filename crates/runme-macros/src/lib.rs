use proc_macro::TokenStream;
use quote::quote;
use syn::{Expr, ExprLit, ItemFn, Lit, Meta, MetaNameValue, ReturnType, parse_macro_input};

/// Attribute macro for defining a runme task.
///
/// Supports both sync and async task functions. The function is wrapped
/// to produce the `TaskFn` signature: `fn(&TaskContext) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>`.
///
/// The generated `TaskDef` includes `group: __RUNME_GROUP`. This constant is
/// injected by the code generator at compile time. For standalone usage (tests,
/// examples), define `const __RUNME_GROUP: &str = "";` manually.
///
/// Usage:
/// ```ignore
/// #[runme::task(desc = "Build the project", watch = "src/**/*.rs")]
/// async fn build(ctx: &TaskContext) {
///     ctx.exec("cargo build").await.unwrap();
/// }
/// ```
///
/// Also works with sync functions:
/// ```ignore
/// #[runme::task(desc = "Say hello")]
/// fn hello(ctx: &TaskContext) {
///     println!("Hello from task: {}", ctx.name);
/// }
/// ```
#[proc_macro_attribute]
pub fn task(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn = parse_macro_input!(item as ItemFn);
    let fn_name = &input_fn.sig.ident;
    let fn_name_str = fn_name.to_string();
    let is_async = input_fn.sig.asyncness.is_some();

    // Parse attributes: desc = "...", watch = "...", depends_on = "a,b,c"
    let mut description: Option<String> = None;
    let mut watch: Option<String> = None;
    let mut depends_on: Vec<String> = Vec::new();

    // Parse the attribute as a comma-separated list of name = "value" pairs
    let attr_parser = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated;
    let parsed_attrs = match syn::parse::Parser::parse(attr_parser, attr) {
        Ok(attrs) => attrs,
        Err(e) => return e.to_compile_error().into(),
    };

    for meta in parsed_attrs {
        match meta {
            Meta::NameValue(MetaNameValue { path, value, .. }) => {
                let key = path.get_ident().map(|i| i.to_string()).unwrap_or_default();
                let val = match &value {
                    Expr::Lit(ExprLit {
                        lit: Lit::Str(s), ..
                    }) => s.value(),
                    _ => {
                        return syn::Error::new_spanned(value, "expected string literal")
                            .to_compile_error()
                            .into();
                    }
                };
                match key.as_str() {
                    "desc" | "description" => description = Some(val),
                    "watch" => watch = Some(val),
                    "depends_on" => {
                        depends_on = val.split(',').map(|s| s.trim().to_string()).collect();
                    }
                    other => {
                        return syn::Error::new_spanned(
                            path,
                            format!("unknown attribute: {}", other),
                        )
                        .to_compile_error()
                        .into();
                    }
                }
            }
            other => {
                return syn::Error::new_spanned(other, "expected `key = \"value\"` format")
                    .to_compile_error()
                    .into();
            }
        }
    }

    // If no desc attribute, extract from doc comments (/// lines)
    if description.is_none() {
        let doc_lines: Vec<String> = input_fn
            .attrs
            .iter()
            .filter_map(|attr| {
                if attr.path().is_ident("doc")
                    && let Meta::NameValue(MetaNameValue {
                        value:
                            Expr::Lit(ExprLit {
                                lit: Lit::Str(s), ..
                            }),
                        ..
                    }) = &attr.meta
                {
                    return Some(s.value().trim().to_string());
                }
                None
            })
            .collect();
        if !doc_lines.is_empty() {
            description = Some(doc_lines.join(" "));
        }
    }

    // Generate the description token
    let desc_tokens = match &description {
        Some(d) => quote! { Some(#d) },
        None => quote! { None },
    };

    // Generate the watch token
    let watch_tokens = match &watch {
        Some(w) => quote! { Some(#w) },
        None => quote! { None },
    };

    // Generate the depends_on token as a static slice
    let deps_tokens = if depends_on.is_empty() {
        quote! { &[] }
    } else {
        let dep_strs: Vec<&str> = depends_on.iter().map(|s| s.as_str()).collect();
        quote! { &[#(#dep_strs),*] }
    };

    // Generate a wrapper function name for the TaskFn registration
    let wrapper_name = syn::Ident::new(&format!("__runme_taskfn_{}", fn_name), fn_name.span());

    // Detect whether the function has an explicit return type (Result) or returns ()
    let has_return_type = !matches!(input_fn.sig.output, ReturnType::Default);

    // The wrapper adapts the user's function (sync/async, void/Result)
    // to TaskFn: fn(&TaskContext) -> Pin<Box<dyn Future<Output = Result<(), TaskError>> + Send + '_>>
    let wrapper = match (is_async, has_return_type) {
        (true, true) => {
            // async fn(...) -> Result<(), TaskError>
            quote! {
                fn #wrapper_name(ctx: &::runme::task::TaskContext) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::runme::error::TaskError>> + Send + '_>> {
                    ::std::boxed::Box::pin(#fn_name(ctx))
                }
            }
        }
        (true, false) => {
            // async fn(...) — no return type, wrap with Ok(())
            quote! {
                fn #wrapper_name(ctx: &::runme::task::TaskContext) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::runme::error::TaskError>> + Send + '_>> {
                    ::std::boxed::Box::pin(async move {
                        #fn_name(ctx).await;
                        Ok(())
                    })
                }
            }
        }
        (false, true) => {
            // fn(...) -> Result<(), TaskError>
            quote! {
                fn #wrapper_name(ctx: &::runme::task::TaskContext) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::runme::error::TaskError>> + Send + '_>> {
                    let result = #fn_name(ctx);
                    ::std::boxed::Box::pin(::std::future::ready(result))
                }
            }
        }
        (false, false) => {
            // fn(...) — no return type, wrap with Ok(())
            quote! {
                fn #wrapper_name(ctx: &::runme::task::TaskContext) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::runme::error::TaskError>> + Send + '_>> {
                    #fn_name(ctx);
                    ::std::boxed::Box::pin(::std::future::ready(Ok(())))
                }
            }
        }
    };

    let expanded = quote! {
        #input_fn

        #wrapper

        ::runme::inventory::submit! {
            ::runme::task::TaskDef {
                name: #fn_name_str,
                description: #desc_tokens,
                group: __RUNME_GROUP,
                watch: #watch_tokens,
                depends_on: #deps_tokens,
                func: #wrapper_name,
            }
        }
    };

    expanded.into()
}

/// Attribute macro for per-file initialization hooks.
///
/// Registers an `InitDef` via `inventory`. The function can accept either
/// `&mut InitContext` or no arguments.
///
/// The generated `InitDef` includes `group: __RUNME_GROUP`. This constant is
/// injected by the code generator at compile time. For standalone usage (tests,
/// examples), define `const __RUNME_GROUP: &str = "";` manually.
///
/// Usage:
/// ```ignore
/// #[runme::init]
/// fn setup(ctx: &mut InitContext) {
///     ctx.set_group_name("Auth Service");
/// }
/// ```
///
/// Or without arguments:
/// ```ignore
/// #[runme::init]
/// fn setup() {
///     // one-time setup
/// }
/// ```
#[proc_macro_attribute]
pub fn init(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn = parse_macro_input!(item as ItemFn);
    let fn_name = &input_fn.sig.ident;

    // Determine whether the function takes an InitContext argument
    let has_ctx_arg = !input_fn.sig.inputs.is_empty();

    // Generate a wrapper function name
    let wrapper_name = syn::Ident::new(&format!("__runme_initfn_{}", fn_name), fn_name.span());

    // The wrapper adapts the user's function to fn(&mut InitContext)
    let wrapper = if has_ctx_arg {
        // fn(ctx: &mut InitContext) — pass through directly
        quote! {
            fn #wrapper_name(ctx: &mut ::runme::init::InitContext) {
                #fn_name(ctx)
            }
        }
    } else {
        // fn() — ignore the context argument
        quote! {
            fn #wrapper_name(_ctx: &mut ::runme::init::InitContext) {
                #fn_name()
            }
        }
    };

    let expanded = quote! {
        #input_fn

        #wrapper

        ::runme::inventory::submit! {
            ::runme::init::InitDef {
                group: __RUNME_GROUP,
                func: #wrapper_name,
            }
        }
    };

    expanded.into()
}
