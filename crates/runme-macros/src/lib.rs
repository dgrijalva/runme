use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Expr, ExprLit, FnArg, GenericArgument, ItemFn, Lit, Meta, MetaNameValue, Pat, PathArguments,
    ReturnType, Type, TypePath, parse_macro_input,
};

/// Known primitive type names for the detection heuristic.
/// If a single remaining parameter has one of these types, it's Form 2 (simple args).
/// If it has a non-primitive type, it's Form 3 (parser struct).
const KNOWN_PRIMITIVES: &[&str] = &[
    "String", "bool", "u8", "u16", "u32", "u64", "u128", "i8", "i16", "i32", "i64", "i128",
    "f32", "f64", "usize", "isize",
];

/// Describes which argument form a task function uses.
enum ArgForm {
    /// Zero extra params after ctx: async fn build(ctx: &TaskContext) -> TaskResult
    ZeroArgs,
    /// Simple params: async fn deploy(ctx: &TaskContext, env: String, port: u16) -> TaskResult
    SimpleArgs(Vec<SimpleParam>),
    /// Parser struct: async fn deploy(ctx: &TaskContext, args: DeployArgs) -> TaskResult
    ParserStruct {
        #[allow(dead_code)]
        param_name: syn::Ident,
        param_type: Box<syn::Type>,
    },
}

/// A simple parameter extracted from the function signature.
struct SimpleParam {
    name: syn::Ident,
    ty: syn::Type,
    kind: SimpleParamKind,
}

/// Classification of a simple parameter for clap arg generation.
enum SimpleParamKind {
    /// bool -> --flag (no value, presence = true)
    Bool,
    /// String, numeric types -> --name <value> (required)
    Required,
    /// Option<T> -> --name <value> (optional)
    Optional(syn::Type),
    /// Vec<T> -> --name <value> (repeatable)
    Repeatable(syn::Type),
}

/// Check if a type path's last segment matches a given name (ignoring generics).
fn type_ident_is(ty: &Type, name: &str) -> bool {
    if let Type::Path(TypePath { path, .. }) = ty
        && let Some(seg) = path.segments.last()
    {
        return seg.ident == name;
    }
    false
}

/// Extract the inner type from Option<T> or Vec<T>.
fn extract_generic_inner(ty: &Type) -> Option<syn::Type> {
    if let Type::Path(TypePath { path, .. }) = ty
        && let Some(seg) = path.segments.last()
        && let PathArguments::AngleBracketed(ref args) = seg.arguments
        && let Some(GenericArgument::Type(inner)) = args.args.first()
    {
        return Some(inner.clone());
    }
    None
}

/// Check if a type is a known primitive (not Option/Vec wrapper).
fn is_known_primitive(ty: &Type) -> bool {
    if let Type::Path(TypePath { path, .. }) = ty
        && let Some(seg) = path.segments.last()
    {
        let name = seg.ident.to_string();
        if KNOWN_PRIMITIVES.contains(&name.as_str()) {
            return true;
        }
        // Option<T> and Vec<T> are considered "primitive wrappers"
        if (name == "Option" || name == "Vec") && extract_generic_inner(ty).is_some() {
            return true;
        }
    }
    false
}

/// Classify a simple parameter for clap arg generation.
fn classify_param(name: syn::Ident, ty: syn::Type) -> SimpleParam {
    let kind = if type_ident_is(&ty, "bool") {
        SimpleParamKind::Bool
    } else if type_ident_is(&ty, "Option") {
        let inner = extract_generic_inner(&ty).unwrap();
        SimpleParamKind::Optional(inner)
    } else if type_ident_is(&ty, "Vec") {
        let inner = extract_generic_inner(&ty).unwrap();
        SimpleParamKind::Repeatable(inner)
    } else {
        SimpleParamKind::Required
    };
    SimpleParam { name, ty, kind }
}

/// Detect the argument form from function parameters.
fn detect_arg_form(input_fn: &ItemFn) -> Result<ArgForm, syn::Error> {
    // Collect params after the first (ctx: &TaskContext)
    let params: Vec<_> = input_fn
        .sig
        .inputs
        .iter()
        .skip(1) // skip ctx
        .collect();

    if params.is_empty() {
        return Ok(ArgForm::ZeroArgs);
    }

    if params.len() > 1 {
        // Multiple params -> Form 2 (simple args)
        let mut simple_params = Vec::new();
        for param in params {
            let (name, ty) = extract_typed_param(param)?;
            simple_params.push(classify_param(name, ty));
        }
        return Ok(ArgForm::SimpleArgs(simple_params));
    }

    // Exactly one param. Check if it's a known primitive -> Form 2, else -> Form 3.
    let (name, ty) = extract_typed_param(params[0])?;
    if is_known_primitive(&ty) {
        let simple = classify_param(name, ty);
        return Ok(ArgForm::SimpleArgs(vec![simple]));
    }

    // Non-primitive single param -> Form 3 (parser struct)
    Ok(ArgForm::ParserStruct {
        param_name: name,
        param_type: Box::new(ty),
    })
}

/// Extract the name and type from a function parameter.
fn extract_typed_param(arg: &FnArg) -> Result<(syn::Ident, syn::Type), syn::Error> {
    match arg {
        FnArg::Typed(pat_type) => {
            let name = match pat_type.pat.as_ref() {
                Pat::Ident(pat_ident) => pat_ident.ident.clone(),
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected a simple identifier pattern for task parameter",
                    ));
                }
            };
            Ok((name, (*pat_type.ty).clone()))
        }
        FnArg::Receiver(r) => Err(syn::Error::new_spanned(
            r,
            "task functions cannot have a `self` parameter",
        )),
    }
}

/// Attribute macro for defining a runme task.
///
/// Supports both sync and async task functions. The function is wrapped
/// to produce the `TaskFn` signature: `fn(&TaskContext, &[String]) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>`.
///
/// The generated `TaskDef` includes `group: __RUNME_GROUP`. This constant is
/// injected by the code generator at compile time. For standalone usage (tests,
/// examples), define `const __RUNME_GROUP: &str = "";` manually.
///
/// # Three argument forms
///
/// **Form 1: Zero args**
/// ```ignore
/// #[runme::task(desc = "Build the project")]
/// async fn build(ctx: &TaskContext) -> TaskResult {
///     ctx.exec("cargo build").await?.ok()?;
///     Ok(())
/// }
/// ```
///
/// **Form 2: Simple args** (auto-generates clap from params)
/// ```ignore
/// #[runme::task(desc = "Deploy to environment")]
/// async fn deploy(ctx: &TaskContext, env: String, port: u16, verbose: bool) -> TaskResult {
///     // env -> --env <value>, port -> --port <value>, verbose -> --verbose (flag)
///     Ok(())
/// }
/// ```
///
/// **Form 3: Parser struct** (single non-primitive param, uses clap derive)
/// ```ignore
/// #[derive(clap::Parser)]
/// struct DeployArgs {
///     #[arg(long)]
///     env: String,
/// }
///
/// #[runme::task(desc = "Deploy to environment")]
/// async fn deploy(ctx: &TaskContext, args: DeployArgs) -> TaskResult {
///     Ok(())
/// }
/// ```
#[proc_macro_attribute]
pub fn task(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut input_fn = parse_macro_input!(item as ItemFn);
    let fn_name = &input_fn.sig.ident;
    let fn_name_str = fn_name.to_string();
    let is_async = input_fn.sig.asyncness.is_some();

    // Parse attributes: desc = "..."
    let mut description: Option<String> = None;

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

    // Detect argument form
    let arg_form = match detect_arg_form(&input_fn) {
        Ok(form) => form,
        Err(e) => return e.to_compile_error().into(),
    };

    // Inject start_task() as the first statement in the function body
    {
        let task_name_str = &fn_name_str;
        // Extract the actual context parameter name (first param) instead of hardcoding "ctx"
        let ctx_ident = match input_fn.sig.inputs.first() {
            Some(FnArg::Typed(pat_type)) => match pat_type.pat.as_ref() {
                Pat::Ident(pat_ident) => pat_ident.ident.clone(),
                _ => syn::Ident::new("ctx", proc_macro2::Span::call_site()),
            },
            _ => syn::Ident::new("ctx", proc_macro2::Span::call_site()),
        };
        let start_task_stmt: syn::Stmt = syn::parse_quote! {
            let _task = #ctx_ident.start_task(#task_name_str);
        };
        input_fn.block.stmts.insert(0, start_task_stmt);
    }

    // Generate a wrapper function name for the TaskFn registration
    let wrapper_name = syn::Ident::new(&format!("__runme_taskfn_{}", fn_name), fn_name.span());

    // Generate the arg_metadata function name
    let arg_metadata_name =
        syn::Ident::new(&format!("__runme_argmeta_{}", fn_name), fn_name.span());

    // Detect whether the function has an explicit return type (Result) or returns ()
    let has_return_type = !matches!(input_fn.sig.output, ReturnType::Default);

    // Generate the parse block, function call expression, and arg_metadata function
    // based on argument form.
    //
    // For forms with arguments:
    // - The parse_block uses `match`/`Err` (not `?`) to return parse errors as futures,
    //   since the wrapper function returns `Pin<Box<Future<Result>>>`, not `Result`.
    // - The parse_block runs synchronously (before `Box::pin`) so `__args` doesn't need
    //   to outlive the function body.
    // - Parsed values are moved into the async block for async tasks.
    let (parse_block, fn_call, arg_metadata_tokens) = match &arg_form {
        ArgForm::ZeroArgs => {
            let parse = quote! {};
            let call = quote! { #fn_name(ctx) };
            let metadata = quote! {
                fn #arg_metadata_name() -> Option<::runme::clap::Command> {
                    None
                }
            };
            (parse, call, metadata)
        }
        ArgForm::SimpleArgs(params) => {
            let (parse_stmts, call_args, cmd_build) =
                generate_simple_args(fn_name_str.clone(), params);
            let parse = parse_stmts;
            let call = quote! { #fn_name(ctx, #(#call_args),*) };
            let metadata = quote! {
                fn #arg_metadata_name() -> Option<::runme::clap::Command> {
                    Some({ #cmd_build })
                }
            };
            (parse, call, metadata)
        }
        ArgForm::ParserStruct {
            param_name: _,
            param_type,
        } => {
            let parse = quote! {
                let __parsed = match <#param_type as ::runme::clap::Parser>::try_parse_from(
                    ::std::iter::once(::std::string::String::from(#fn_name_str))
                        .chain(__args.iter().cloned())
                ) {
                    Ok(v) => v,
                    Err(e) => return ::std::boxed::Box::pin(::std::future::ready(
                        Err(::runme::error::TaskError::from_display(e))
                    )),
                };
            };
            let call = quote! { #fn_name(ctx, __parsed) };
            let metadata = quote! {
                fn #arg_metadata_name() -> Option<::runme::clap::Command> {
                    Some(<#param_type as ::runme::clap::CommandFactory>::command())
                }
            };
            (parse, call, metadata)
        }
    };

    // Build the wrapper function. The wrapper adapts the user's function
    // (sync/async, void/Result, with/without args) to TaskFn:
    //   for<'a> fn(&'a TaskContext, &[String]) -> Pin<Box<dyn Future<...> + Send + 'a>>
    //
    // The parse_block runs synchronously (before `Box::pin`), so that `__args`
    // doesn't need to live into the async future. The parsed values are moved
    // into the async block.
    let wrapper = match (is_async, has_return_type) {
        (true, true) => {
            // async fn(...) -> Result<(), TaskError>
            quote! {
                fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::runme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::runme::error::TaskError>> + Send + '__runme_lt>> {
                    #parse_block
                    ::std::boxed::Box::pin(async move { #fn_call .await })
                }
            }
        }
        (true, false) => {
            // async fn(...) — no return type, wrap with Ok(())
            quote! {
                fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::runme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::runme::error::TaskError>> + Send + '__runme_lt>> {
                    #parse_block
                    ::std::boxed::Box::pin(async move {
                        #fn_call .await;
                        Ok(())
                    })
                }
            }
        }
        (false, true) => {
            // fn(...) -> Result<(), TaskError>
            quote! {
                fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::runme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::runme::error::TaskError>> + Send + '__runme_lt>> {
                    #parse_block
                    let result = #fn_call;
                    ::std::boxed::Box::pin(::std::future::ready(result))
                }
            }
        }
        (false, false) => {
            // fn(...) — no return type, wrap with Ok(())
            quote! {
                fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::runme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::runme::error::TaskError>> + Send + '__runme_lt>> {
                    #parse_block
                    #fn_call;
                    ::std::boxed::Box::pin(::std::future::ready(Ok(())))
                }
            }
        }
    };

    let expanded = quote! {
        #input_fn

        #wrapper

        #arg_metadata_tokens

        ::runme::inventory::submit! {
            ::runme::task::TaskDef {
                name: #fn_name_str,
                description: #desc_tokens,
                group: __RUNME_GROUP,
                func: #wrapper_name,
                arg_metadata: #arg_metadata_name,
                ui_hint: None,
            }
        }
    };

    expanded.into()
}

/// Generate the parsing block, call arguments, and clap::Command builder
/// for Form 2 (simple args).
fn generate_simple_args(
    task_name: String,
    params: &[SimpleParam],
) -> (
    proc_macro2::TokenStream,
    Vec<proc_macro2::TokenStream>,
    proc_macro2::TokenStream,
) {
    // Build the clap::Command with args
    let mut arg_builders = Vec::new();
    for param in params {
        let name_str = param.name.to_string();
        let long_name = name_str.replace('_', "-");
        let arg_build = match &param.kind {
            SimpleParamKind::Bool => {
                quote! {
                    ::runme::clap::Arg::new(#name_str)
                        .long(#long_name)
                        .action(::runme::clap::ArgAction::SetTrue)
                }
            }
            SimpleParamKind::Required => {
                quote! {
                    ::runme::clap::Arg::new(#name_str)
                        .long(#long_name)
                        .required(true)
                        .action(::runme::clap::ArgAction::Set)
                }
            }
            SimpleParamKind::Optional(_) => {
                quote! {
                    ::runme::clap::Arg::new(#name_str)
                        .long(#long_name)
                        .required(false)
                        .action(::runme::clap::ArgAction::Set)
                }
            }
            SimpleParamKind::Repeatable(_) => {
                quote! {
                    ::runme::clap::Arg::new(#name_str)
                        .long(#long_name)
                        .action(::runme::clap::ArgAction::Append)
                }
            }
        };
        arg_builders.push(arg_build);
    }

    // Build the Command construction code (shared between wrapper and metadata)
    let cmd_build = quote! {
        ::runme::clap::Command::new(#task_name)
            #(.arg(#arg_builders))*
    };

    // Generate the parsing code for the wrapper
    let mut parse_stmts = Vec::new();
    let mut call_args = Vec::new();

    // Parse the args with clap. Uses match + early return instead of `?`
    // because the wrapper function returns Pin<Box<Future<Result>>>, not Result.
    let parse_match = quote! {
        let __clap_matches = match ({
            #cmd_build
        }).try_get_matches_from(
            ::std::iter::once(::std::string::String::from(#task_name))
                .chain(__args.iter().cloned())
        ) {
            Ok(m) => m,
            Err(e) => return ::std::boxed::Box::pin(::std::future::ready(
                Err(::runme::error::TaskError::from_display(e))
            )),
        };
    };
    parse_stmts.push(parse_match);

    // Extract each parameter. Uses match + early return for parse errors.
    for param in params {
        let param_name = &param.name;
        let name_str = param.name.to_string();
        let ty = &param.ty;

        let extract = match &param.kind {
            SimpleParamKind::Bool => {
                quote! {
                    let #param_name: #ty = __clap_matches.get_flag(#name_str);
                }
            }
            SimpleParamKind::Required => {
                quote! {
                    let #param_name: #ty = match __clap_matches.get_one::<String>(#name_str) {
                        Some(v) => match v.parse::<#ty>() {
                            Ok(parsed) => parsed,
                            Err(e) => return ::std::boxed::Box::pin(::std::future::ready(
                                Err(::runme::error::TaskError::from_display(
                                    format!("invalid value for --{}: {}", #name_str, e)
                                ))
                            )),
                        },
                        None => return ::std::boxed::Box::pin(::std::future::ready(
                            Err(::runme::error::TaskError::from_display(
                                format!("missing required argument: --{}", #name_str)
                            ))
                        )),
                    };
                }
            }
            SimpleParamKind::Optional(inner) => {
                quote! {
                    let #param_name: #ty = match __clap_matches.get_one::<String>(#name_str)
                        .map(|v| v.parse::<#inner>())
                        .transpose()
                    {
                        Ok(v) => v,
                        Err(e) => return ::std::boxed::Box::pin(::std::future::ready(
                            Err(::runme::error::TaskError::from_display(
                                format!("invalid value for --{}: {}", #name_str, e)
                            ))
                        )),
                    };
                }
            }
            SimpleParamKind::Repeatable(inner) => {
                quote! {
                    let #param_name: #ty = match __clap_matches.get_many::<String>(#name_str)
                        .map(|vals| vals.map(|v| v.parse::<#inner>()).collect::<Result<Vec<_>, _>>())
                        .transpose()
                    {
                        Ok(v) => v.unwrap_or_default(),
                        Err(e) => return ::std::boxed::Box::pin(::std::future::ready(
                            Err(::runme::error::TaskError::from_display(
                                format!("invalid value for --{}: {}", #name_str, e)
                            ))
                        )),
                    };
                }
            }
        };
        parse_stmts.push(extract);
        call_args.push(quote! { #param_name });
    }

    let parse_block = quote! { #(#parse_stmts)* };
    (parse_block, call_args, cmd_build)
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
