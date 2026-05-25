mod cmd_macro;

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
    "String", "bool", "u8", "u16", "u32", "u64", "u128", "i8", "i16", "i32", "i64", "i128", "f32",
    "f64", "usize", "isize",
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

/// Attribute macro for defining a rnme task.
///
/// Supports both sync and async task functions. The function is wrapped
/// to produce the `TaskFn` signature: `fn(&TaskContext, &[String]) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>`.
///
/// The generated `TaskDef` includes `group: __RNME_GROUP` and
/// `dir: __RNME_DIR`. Both constants are injected by the code generator at
/// compile time. For standalone usage (tests, examples), define
/// `const __RNME_GROUP: &str = "";` and `const __RNME_DIR: &str = "";`
/// manually — the empty `__RNME_DIR` makes `ctx.spawn` inherit the
/// process cwd as before.
///
/// The task description is taken from the function's `///` doc comments.
///
/// # Three argument forms
///
/// **Form 1: Zero args**
/// ```ignore
/// /// Build the project
/// #[rnme::task]
/// async fn build(ctx: &TaskContext) -> TaskResult {
///     ctx.exec("cargo build").await?.ok()?;
///     Ok(())
/// }
/// ```
///
/// **Form 2: Simple args** (auto-generates clap from params)
/// ```ignore
/// /// Deploy to environment
/// #[rnme::task]
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
/// /// Deploy to environment
/// #[rnme::task]
/// async fn deploy(ctx: &TaskContext, args: DeployArgs) -> TaskResult {
///     Ok(())
/// }
/// ```
#[proc_macro_attribute]
pub fn task(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut input_fn = parse_macro_input!(item as ItemFn);
    let fn_name = input_fn.sig.ident.clone();
    let fn_name_str = fn_name.to_string();
    let is_async = input_fn.sig.asyncness.is_some();

    // Renamed body symbol. The user's fn keeps its full signature and
    // body but is emitted under this private name. The user-facing
    // identifier is taken over by the typed shim (below) that returns a
    // `TaskBuilder`. Both the shim and the string-args wrapper call
    // this symbol.
    let body_name = syn::Ident::new(&format!("__rnme_body_{}", fn_name), fn_name.span());

    let TaskFnMeta {
        desc_tokens,
        ui_hint_tokens,
        arg_form,
    } = match parse_task_attrs_and_meta(attr, &input_fn) {
        Ok(m) => m,
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

    // Named static holding the TaskDef. Both inventory and the typed shim
    // (Phase 2) reference this single instance.
    let taskdef_static_name =
        syn::Ident::new(&format!("__RNME_TASKDEF_{}", fn_name), fn_name.span());

    // Detect whether the function has an explicit return type (Result) or returns ()
    let has_return_type = !matches!(input_fn.sig.output, ReturnType::Default);

    // Capture the typed parameter list (after the `ctx: &TaskContext`
    // first param) for the public shim. We need both the original `name:
    // ty` pattern (for the shim signature) and bare idents (for
    // forwarding into the closure's call to `body_name`). Lifted before
    // we rename the input fn so they remain in sync with the body's
    // signature.
    let typed_params: Vec<(syn::Ident, syn::Type)> = input_fn
        .sig
        .inputs
        .iter()
        .skip(1)
        .filter_map(|arg| match arg {
            FnArg::Typed(pat_type) => match pat_type.pat.as_ref() {
                Pat::Ident(pat_ident) => Some((pat_ident.ident.clone(), (*pat_type.ty).clone())),
                _ => None,
            },
            FnArg::Receiver(_) => None,
        })
        .collect();
    let shim_param_decls: Vec<proc_macro2::TokenStream> = typed_params
        .iter()
        .map(|(name, ty)| quote! { #name: #ty })
        .collect();
    let shim_param_idents: Vec<syn::Ident> =
        typed_params.iter().map(|(name, _)| name.clone()).collect();

    // Rename the user's fn to the private body symbol. The shim emitted
    // below takes over the public ident. The string-args wrapper and the
    // shim closure both call `body_name` to dispatch to the actual user
    // code. All other metadata (doc-comment description, mode/ui_hint)
    // continues to live on the `TaskDef` named static, not the shim.
    input_fn.sig.ident = body_name.clone();
    // Strip `pub`/visibility from the renamed body — it's private to the
    // module. The shim below carries the original visibility (always
    // `pub fn` so descendant crates can reference it via `subtasks::...`).
    input_fn.vis = syn::Visibility::Inherited;

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
            let call = quote! { #body_name(ctx) };
            let metadata = quote! {
                fn #arg_metadata_name() -> Option<::rnme::clap::Command> {
                    None
                }
            };
            (parse, call, metadata)
        }
        ArgForm::SimpleArgs(params) => {
            let (parse_stmts, call_args, cmd_build) =
                generate_simple_args(fn_name_str.clone(), params);
            let parse = parse_stmts;
            let call = quote! { #body_name(ctx, #(#call_args),*) };
            let metadata = quote! {
                fn #arg_metadata_name() -> Option<::rnme::clap::Command> {
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
                let __parsed = match <#param_type as ::rnme::clap::Parser>::try_parse_from(
                    ::std::iter::once(::std::string::String::from(#fn_name_str))
                        .chain(__args.iter().cloned())
                ) {
                    Ok(v) => v,
                    Err(e) => return ::std::boxed::Box::pin(::std::future::ready(
                        Err(::rnme::error::TaskError::from_display(e))
                    )),
                };
            };
            let call = quote! { #body_name(ctx, __parsed) };
            let metadata = quote! {
                fn #arg_metadata_name() -> Option<::rnme::clap::Command> {
                    Some(<#param_type as ::rnme::clap::CommandFactory>::command())
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
                fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::rnme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::rnme::error::TaskError>> + Send + '__runme_lt>> {
                    #parse_block
                    ::std::boxed::Box::pin(async move { #fn_call .await })
                }
            }
        }
        (true, false) => {
            // async fn(...) — no return type, wrap with Ok(())
            quote! {
                fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::rnme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::rnme::error::TaskError>> + Send + '__runme_lt>> {
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
                fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::rnme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::rnme::error::TaskError>> + Send + '__runme_lt>> {
                    #parse_block
                    let result = #fn_call;
                    ::std::boxed::Box::pin(::std::future::ready(result))
                }
            }
        }
        (false, false) => {
            // fn(...) — no return type, wrap with Ok(())
            quote! {
                fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::rnme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::rnme::error::TaskError>> + Send + '__runme_lt>> {
                    #parse_block
                    #fn_call;
                    ::std::boxed::Box::pin(::std::future::ready(Ok(())))
                }
            }
        }
    };

    // Body expression inside the shim's async block, matching the
    // (is_async, has_return_type) matrix of the user's fn. The async
    // block lives inside `Box::pin(async move { ... })`. The async
    // block captures `body_ctx: &TaskContext` (the closure param) and
    // the typed args (by `move`), then calls the renamed body symbol.
    let shim_body_expr = match (is_async, has_return_type) {
        (true, true) => quote! {
            #body_name(body_ctx, #(#shim_param_idents),*).await
        },
        (true, false) => quote! {
            #body_name(body_ctx, #(#shim_param_idents),*).await;
            ::std::result::Result::Ok(())
        },
        (false, true) => quote! {
            #body_name(body_ctx, #(#shim_param_idents),*)
        },
        (false, false) => quote! {
            #body_name(body_ctx, #(#shim_param_idents),*);
            ::std::result::Result::Ok(())
        },
    };

    // Public typed shim at the original fn name. Returns a `TaskBuilder`
    // configured with `Invocation::Factory` so the engine dispatches to
    // the renamed body symbol with typed args, bypassing the
    // string-args parser entirely. `#[must_use]` triggers a warning when
    // a caller writes `build_wasm(ctx, true, false);` without `.await?`
    // or `.spawn()?`.
    let shim = quote! {
        #[must_use = "task builders do nothing until `.await` or `.spawn()` — \
                      a bare call constructs the builder and drops it"]
        pub fn #fn_name(
            ctx: &::rnme::task::TaskContext,
            #(#shim_param_decls,)*
        ) -> ::rnme::execution::builder::TaskBuilder {
            ::rnme::execution::builder::TaskBuilder::from_factory(
                ctx,
                &#taskdef_static_name,
                ::std::boxed::Box::new(move |body_ctx: &::rnme::task::TaskContext| {
                    ::std::boxed::Box::pin(async move {
                        #shim_body_expr
                    })
                }),
            )
        }
    };

    // Hardening probes. `#[rnme::task]` is only meaningful inside a scope
    // that has `__RNME_GROUP` / `__RNME_DIR` `const &str` items — these
    // are auto-injected inside `RUNME.rs` files and may be defined
    // manually in shared library crates that intend to register tasks
    // directly. If either constant is missing, the probes below produce
    // a hard compile error pointing at this macro invocation.
    //
    // The probes are bare const references rather than a wrapped
    // `compile_error!`-with-message because proc macros can't detect
    // surrounding scope at expand time. The resulting error is rustc's
    // E0425 "cannot find value `__RNME_GROUP` in this scope". For library
    // crates, the alternative is `#[rnme::task_template]` —
    // documented at the `#[rnme::task]` doc-comment.
    let group_probe_name =
        syn::Ident::new(&format!("__rnme_task_requires_group_{}", fn_name), fn_name.span());
    let dir_probe_name =
        syn::Ident::new(&format!("__rnme_task_requires_dir_{}", fn_name), fn_name.span());
    let hardening_probes = quote! {
        #[allow(dead_code, non_upper_case_globals)]
        const #group_probe_name: &str = __RNME_GROUP;
        #[allow(dead_code, non_upper_case_globals)]
        const #dir_probe_name: &str = __RNME_DIR;
    };

    let expanded = quote! {
        #hardening_probes

        #input_fn

        #wrapper

        #arg_metadata_tokens

        #[allow(non_upper_case_globals)]
        pub static #taskdef_static_name: ::rnme::task::TaskDef = ::rnme::task::TaskDef {
            name: #fn_name_str,
            description: #desc_tokens,
            group: __RNME_GROUP,
            dir: __RNME_DIR,
            func: ::rnme::task::TaskFnKind::Static(#wrapper_name),
            arg_metadata: #arg_metadata_name,
            ui_hint: #ui_hint_tokens,
        };

        ::rnme::inventory::submit! {
            ::rnme::task::TaskDefRef(&#taskdef_static_name)
        }

        #shim
    };

    expanded.into()
}

/// Attribute macro for declaring a reusable task **template** in a regular Rust crate.
///
/// Distinct from `#[rnme::task]`: a template is *not* a self-registering task. It
/// produces the building blocks (renamed body, string-args wrapper, arg-metadata
/// fn, and a per-task `macro_rules!` helper) that a *consumer* RUNME.rs can
/// re-stamp into a fully-local typed task registration via `rnme::import_task!`.
///
/// The library site emits **no** `TaskDef` static, **no** `inventory::submit!`, and
/// **no** typed shim. All three are stamped at the consumer site by the per-task
/// helper macro, using the consumer's `__RNME_GROUP` and `__RNME_DIR` constants.
///
/// Accepts the same three argument forms as `#[rnme::task]`. The captured signature,
/// description (from doc comments), and `ui_hint` are baked into the helper macro
/// at proc-macro time.
///
/// See `docs/task_templates.md` for the design.
#[proc_macro_attribute]
pub fn task_template(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut input_fn = parse_macro_input!(item as ItemFn);
    let fn_name = input_fn.sig.ident.clone();
    let fn_name_str = fn_name.to_string();
    let is_async = input_fn.sig.asyncness.is_some();

    // Per-task symbol names. The body, string-args wrapper, and argmeta fn
    // live in the library crate and are reached from the consumer site via
    // `$crate::...` inside the stamp macro_rules expansion.
    let body_name = syn::Ident::new(&format!("__rnme_body_{}", fn_name), fn_name.span());
    let wrapper_name = syn::Ident::new(&format!("__runme_taskfn_{}", fn_name), fn_name.span());
    let arg_metadata_name =
        syn::Ident::new(&format!("__runme_argmeta_{}", fn_name), fn_name.span());
    let stamp_macro_name =
        syn::Ident::new(&format!("__rnme_stamp_{}", fn_name), fn_name.span());

    let TaskFnMeta {
        desc_tokens,
        ui_hint_tokens,
        arg_form,
    } = match parse_task_attrs_and_meta(attr, &input_fn) {
        Ok(m) => m,
        Err(e) => return e.to_compile_error().into(),
    };

    let has_return_type = !matches!(input_fn.sig.output, ReturnType::Default);

    // Capture the typed parameter list (after `ctx: &TaskContext`) for the
    // stamped-out typed shim. We embed the original `name: ty` token shapes
    // verbatim into the macro_rules arm — the consumer is responsible for
    // having the types in scope (e.g. via `use rnme_cargo::BuildOpts;`).
    let typed_params: Vec<(syn::Ident, syn::Type)> = input_fn
        .sig
        .inputs
        .iter()
        .skip(1)
        .filter_map(|arg| match arg {
            FnArg::Typed(pat_type) => match pat_type.pat.as_ref() {
                Pat::Ident(pat_ident) => Some((pat_ident.ident.clone(), (*pat_type.ty).clone())),
                _ => None,
            },
            FnArg::Receiver(_) => None,
        })
        .collect();
    let shim_param_decls: Vec<proc_macro2::TokenStream> = typed_params
        .iter()
        .map(|(name, ty)| quote! { #name: #ty })
        .collect();
    let shim_param_idents: Vec<syn::Ident> =
        typed_params.iter().map(|(name, _)| name.clone()).collect();

    // Rename the user's fn to the private body symbol and make it `pub` so
    // the stamp expansion can reach it as `$crate::__rnme_body_<name>`.
    //
    // **No `start_task` injection here.** The runtime tracing span is opened
    // at the consumer site (with the consumer-stamped name) by the stamp
    // expansion below.
    input_fn.sig.ident = body_name.clone();
    input_fn.vis = syn::Visibility::Public(syn::Token![pub](fn_name.span()));

    // Build the string-args wrapper for the library site. Same shape as
    // `#[rnme::task]` emits, but `pub` so the consumer-stamped wrapper can
    // delegate to it via `$crate::__runme_taskfn_<name>`. The wrapper does
    // not open a tracing span — that happens at the consumer site.
    let (parse_block, fn_call, arg_metadata_tokens) = match &arg_form {
        ArgForm::ZeroArgs => {
            let parse = quote! {};
            let call = quote! { #body_name(ctx) };
            let metadata = quote! {
                pub fn #arg_metadata_name() -> Option<::rnme::clap::Command> {
                    None
                }
            };
            (parse, call, metadata)
        }
        ArgForm::SimpleArgs(params) => {
            let (parse_stmts, call_args, cmd_build) =
                generate_simple_args(fn_name_str.clone(), params);
            let parse = parse_stmts;
            let call = quote! { #body_name(ctx, #(#call_args),*) };
            let metadata = quote! {
                pub fn #arg_metadata_name() -> Option<::rnme::clap::Command> {
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
                let __parsed = match <#param_type as ::rnme::clap::Parser>::try_parse_from(
                    ::std::iter::once(::std::string::String::from(#fn_name_str))
                        .chain(__args.iter().cloned())
                ) {
                    Ok(v) => v,
                    Err(e) => return ::std::boxed::Box::pin(::std::future::ready(
                        Err(::rnme::error::TaskError::from_display(e))
                    )),
                };
            };
            let call = quote! { #body_name(ctx, __parsed) };
            let metadata = quote! {
                pub fn #arg_metadata_name() -> Option<::rnme::clap::Command> {
                    Some(<#param_type as ::rnme::clap::CommandFactory>::command())
                }
            };
            (parse, call, metadata)
        }
    };

    let wrapper = match (is_async, has_return_type) {
        (true, true) => quote! {
            pub fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::rnme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::rnme::error::TaskError>> + Send + '__runme_lt>> {
                #parse_block
                ::std::boxed::Box::pin(async move { #fn_call .await })
            }
        },
        (true, false) => quote! {
            pub fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::rnme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::rnme::error::TaskError>> + Send + '__runme_lt>> {
                #parse_block
                ::std::boxed::Box::pin(async move {
                    #fn_call .await;
                    Ok(())
                })
            }
        },
        (false, true) => quote! {
            pub fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::rnme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::rnme::error::TaskError>> + Send + '__runme_lt>> {
                #parse_block
                let result = #fn_call;
                ::std::boxed::Box::pin(::std::future::ready(result))
            }
        },
        (false, false) => quote! {
            pub fn #wrapper_name<'__runme_lt>(ctx: &'__runme_lt ::rnme::task::TaskContext, __args: &[String]) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ::std::result::Result<(), ::rnme::error::TaskError>> + Send + '__runme_lt>> {
                #parse_block
                #fn_call;
                ::std::boxed::Box::pin(::std::future::ready(Ok(())))
            }
        },
    };

    // Shim body expression — what the factory closure does at the consumer
    // site to dispatch to the library body. Matches the (is_async,
    // has_return_type) matrix of the user's fn.
    let shim_body_expr = match (is_async, has_return_type) {
        (true, true) => quote! {
            $crate::#body_name(body_ctx, #(#shim_param_idents),*).await
        },
        (true, false) => quote! {
            $crate::#body_name(body_ctx, #(#shim_param_idents),*).await;
            ::std::result::Result::Ok(())
        },
        (false, true) => quote! {
            $crate::#body_name(body_ctx, #(#shim_param_idents),*)
        },
        (false, false) => quote! {
            $crate::#body_name(body_ctx, #(#shim_param_idents),*);
            ::std::result::Result::Ok(())
        },
    };

    // Per-task stamp helper. `#[macro_export]` makes it reachable as
    // `<library_crate>::__rnme_stamp_<name>!` from the consumer site.
    //
    // The arm:
    //
    // - Reads `__RNME_GROUP` / `__RNME_DIR` as bare identifiers — they bind
    //   to the consumer's `const __RNME_GROUP: &str = ...;` /
    //   `const __RNME_DIR: &str = ...;` (call-site scope in macro_rules).
    //
    // - Refers to library fns via `$crate::...` so they resolve back to the
    //   defining crate regardless of how the consumer imports it.
    //
    // - Emits a consumer-local string-args wrapper that opens the tracing
    //   span with the consumer's stamped name, then delegates to the library
    //   wrapper. This way `start_task` fires for both the typed path (via
    //   the factory closure) and the string-args path (via this wrapper)
    //   with the consumer-visible name.
    //
    // - Emits `pub static __RNME_TASKDEF_<name>`, the `inventory::submit!`,
    //   and the `pub fn <name>(...) -> TaskBuilder` typed shim.
    let stamp_wrapper_name =
        syn::Ident::new(&format!("__runme_taskfn_{}", fn_name), fn_name.span());
    let stamp_taskdef_name =
        syn::Ident::new(&format!("__RNME_TASKDEF_{}", fn_name), fn_name.span());

    let stamp_macro = quote! {
        #[macro_export]
        #[doc(hidden)]
        macro_rules! #stamp_macro_name {
            () => {
                // Consumer-local string-args wrapper that opens the tracing
                // span with the consumer-stamped name (here baked in as the
                // template's fn name — `rnme::import_task!` does not allow
                // renaming today).
                #[allow(non_snake_case)]
                fn #stamp_wrapper_name<'__runme_lt>(
                    ctx: &'__runme_lt ::rnme::task::TaskContext,
                    __args: &[::std::string::String],
                ) -> ::std::pin::Pin<::std::boxed::Box<
                    dyn ::std::future::Future<
                        Output = ::std::result::Result<(), ::rnme::error::TaskError>,
                    > + ::std::marker::Send + '__runme_lt,
                >> {
                    let __inner = $crate::#wrapper_name(ctx, __args);
                    ::std::boxed::Box::pin(async move {
                        let _task = ctx.start_task(#fn_name_str);
                        __inner.await
                    })
                }

                #[allow(non_upper_case_globals)]
                pub static #stamp_taskdef_name: ::rnme::task::TaskDef = ::rnme::task::TaskDef {
                    name: #fn_name_str,
                    description: #desc_tokens,
                    group: __RNME_GROUP,
                    dir: __RNME_DIR,
                    func: ::rnme::task::TaskFnKind::Static(#stamp_wrapper_name),
                    arg_metadata: $crate::#arg_metadata_name,
                    ui_hint: #ui_hint_tokens,
                };

                ::rnme::inventory::submit! {
                    ::rnme::task::TaskDefRef(&#stamp_taskdef_name)
                }

                #[must_use = "task builders do nothing until `.await` or `.spawn()` — \
                              a bare call constructs the builder and drops it"]
                pub fn #fn_name(
                    ctx: &::rnme::task::TaskContext,
                    #(#shim_param_decls,)*
                ) -> ::rnme::execution::builder::TaskBuilder {
                    ::rnme::execution::builder::TaskBuilder::from_factory(
                        ctx,
                        &#stamp_taskdef_name,
                        ::std::boxed::Box::new(move |body_ctx: &::rnme::task::TaskContext| {
                            ::std::boxed::Box::pin(async move {
                                let _task = body_ctx.start_task(#fn_name_str);
                                #shim_body_expr
                            })
                        }),
                    )
                }
            };
        }
    };

    let expanded = quote! {
        #input_fn

        #wrapper

        #arg_metadata_tokens

        #stamp_macro
    };

    expanded.into()
}

/// Shared front-matter parser for `#[rnme::task]` and `#[rnme::task_template]`.
///
/// Pulls the doc-comment description, the optional `mode = cli|tui`
/// attribute, and detects the argument form from the function signature.
/// Returns the assembled token bits the two macros both need.
struct TaskFnMeta {
    desc_tokens: proc_macro2::TokenStream,
    ui_hint_tokens: proc_macro2::TokenStream,
    arg_form: ArgForm,
}

fn parse_task_attrs_and_meta(
    attr: TokenStream,
    input_fn: &ItemFn,
) -> Result<TaskFnMeta, syn::Error> {
    // Parse the attribute as a comma-separated list of name = value pairs.
    let attr_parser = syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated;
    let parsed_attrs = syn::parse::Parser::parse(attr_parser, attr)?;

    let mut ui_hint: Option<proc_macro2::TokenStream> = None;
    for meta in parsed_attrs {
        match meta {
            Meta::NameValue(MetaNameValue { path, value, .. }) => {
                let key = path.get_ident().map(|i| i.to_string()).unwrap_or_default();
                match key.as_str() {
                    "mode" => {
                        let mode_str = match &value {
                            Expr::Path(p) => match p.path.get_ident() {
                                Some(i) => i.to_string(),
                                None => {
                                    return Err(syn::Error::new_spanned(
                                        value,
                                        "expected `cli` or `tui`",
                                    ));
                                }
                            },
                            Expr::Lit(ExprLit {
                                lit: Lit::Str(s), ..
                            }) => s.value(),
                            _ => {
                                return Err(syn::Error::new_spanned(
                                    value,
                                    "expected `cli` or `tui` (bare ident or string literal)",
                                ));
                            }
                        };
                        ui_hint = Some(match mode_str.as_str() {
                            "cli" | "Cli" | "CLI" => {
                                quote! { Some(::rnme::task::UiHint::Cli) }
                            }
                            "tui" | "Tui" | "TUI" => {
                                quote! { Some(::rnme::task::UiHint::Tui) }
                            }
                            other => {
                                return Err(syn::Error::new_spanned(
                                    value,
                                    format!("unknown mode `{}` — expected `cli` or `tui`", other),
                                ));
                            }
                        });
                    }
                    "desc" | "description" => {
                        return Err(syn::Error::new_spanned(
                            path,
                            "task descriptions come from `///` doc comments — \
                             remove this attribute and write a `///` line above the fn",
                        ));
                    }
                    other => {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!("unknown attribute: {}", other),
                        ));
                    }
                }
            }
            other => {
                return Err(syn::Error::new_spanned(other, "expected `key = value` format"));
            }
        }
    }

    let ui_hint_tokens = ui_hint.unwrap_or_else(|| quote! { None });

    // Description from `///` doc comments.
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
    let desc_tokens = if doc_lines.is_empty() {
        quote! { None }
    } else {
        let joined = doc_lines.join(" ");
        quote! { Some(#joined) }
    };

    let arg_form = detect_arg_form(input_fn)?;

    Ok(TaskFnMeta {
        desc_tokens,
        ui_hint_tokens,
        arg_form,
    })
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
                    ::rnme::clap::Arg::new(#name_str)
                        .long(#long_name)
                        .action(::rnme::clap::ArgAction::SetTrue)
                }
            }
            SimpleParamKind::Required => {
                quote! {
                    ::rnme::clap::Arg::new(#name_str)
                        .long(#long_name)
                        .required(true)
                        .action(::rnme::clap::ArgAction::Set)
                }
            }
            SimpleParamKind::Optional(_) => {
                quote! {
                    ::rnme::clap::Arg::new(#name_str)
                        .long(#long_name)
                        .required(false)
                        .action(::rnme::clap::ArgAction::Set)
                }
            }
            SimpleParamKind::Repeatable(_) => {
                quote! {
                    ::rnme::clap::Arg::new(#name_str)
                        .long(#long_name)
                        .action(::rnme::clap::ArgAction::Append)
                }
            }
        };
        arg_builders.push(arg_build);
    }

    // Build the Command construction code (shared between wrapper and metadata)
    let cmd_build = quote! {
        ::rnme::clap::Command::new(#task_name)
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
                Err(::rnme::error::TaskError::from_display(e))
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
                                Err(::rnme::error::TaskError::from_display(
                                    format!("invalid value for --{}: {}", #name_str, e)
                                ))
                            )),
                        },
                        None => return ::std::boxed::Box::pin(::std::future::ready(
                            Err(::rnme::error::TaskError::from_display(
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
                            Err(::rnme::error::TaskError::from_display(
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
                            Err(::rnme::error::TaskError::from_display(
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

/// Import a task template from a library crate into the current scope.
///
/// `rnme::import_task!(lib_crate::task_name);` expands to
/// `lib_crate::__rnme_stamp_task_name!();`, invoking the per-task stamp
/// helper that `#[rnme::task_template]` generated at the library site.
/// The expansion produces a fully-local typed task registration at the
/// consumer site — `pub static __RNME_TASKDEF_<name>`, an
/// `inventory::submit!`, and a `#[must_use] pub fn <name>(...) -> TaskBuilder`
/// shim — using the consumer's `__RNME_GROUP` / `__RNME_DIR`.
///
/// A function-like proc macro (not `macro_rules!`) because synthesizing
/// the identifier `__rnme_stamp_<task>` from a captured `$task:ident`
/// requires token pasting, which declarative macros can't do.
///
/// ```ignore
/// // In a RUNME.rs:
/// rnme::import_task!(rnme_test_task_templates::build);
/// ```
///
/// A typo in the task name produces a compile error pointing at the
/// library path (the missing `__rnme_stamp_<typo>!` macro).
#[proc_macro]
pub fn import_task(input: TokenStream) -> TokenStream {
    let path: syn::Path = match syn::parse(input) {
        Ok(p) => p,
        Err(e) => return e.to_compile_error().into(),
    };

    if path.segments.is_empty() {
        return syn::Error::new_spanned(&path, "expected a path like `lib_crate::task_name`")
            .to_compile_error()
            .into();
    }

    let mut lib_path = path.clone();
    // Pop the final segment — that's the task ident. Everything before is
    // the library path used to reach the stamp macro.
    let task_seg = lib_path
        .segments
        .pop()
        .expect("at least one segment, checked above")
        .into_value();

    if !task_seg.arguments.is_empty() {
        return syn::Error::new_spanned(
            &task_seg.arguments,
            "task name must not carry generic arguments",
        )
        .to_compile_error()
        .into();
    }

    if lib_path.segments.is_empty() {
        return syn::Error::new_spanned(
            &path,
            "expected `lib_crate::task_name` — a library path followed by the task name",
        )
        .to_compile_error()
        .into();
    }
    // Drop the trailing `::` separator left over from popping the final segment.
    lib_path.segments.pop_punct();

    let task_ident = &task_seg.ident;
    let stamp_ident = syn::Ident::new(
        &format!("__rnme_stamp_{}", task_ident),
        task_ident.span(),
    );

    let expanded = quote! {
        #lib_path :: #stamp_ident !();
    };
    expanded.into()
}

/// Attribute macro for per-file initialization hooks.
///
/// Registers an `InitDef` via `inventory`. The function can accept either
/// `&mut InitContext` or no arguments.
///
/// The generated `InitDef` includes `group: __RNME_GROUP` and
/// `dir: __RNME_DIR`. Both constants are injected by the code generator at
/// compile time. For standalone usage (tests, examples), define
/// `const __RNME_GROUP: &str = "";` and `const __RNME_DIR: &str = "";`
/// manually.
///
/// Usage:
/// ```ignore
/// #[rnme::init]
/// fn setup(ctx: &mut InitContext) {
///     ctx.set_group_name("Auth Service");
/// }
/// ```
///
/// Or without arguments:
/// ```ignore
/// #[rnme::init]
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
            fn #wrapper_name(ctx: &mut ::rnme::init::InitContext) {
                #fn_name(ctx)
            }
        }
    } else {
        // fn() — ignore the context argument
        quote! {
            fn #wrapper_name(_ctx: &mut ::rnme::init::InitContext) {
                #fn_name()
            }
        }
    };

    let expanded = quote! {
        #input_fn

        #wrapper

        ::rnme::inventory::submit! {
            ::rnme::init::InitDef {
                group: __RNME_GROUP,
                dir: __RNME_DIR,
                func: #wrapper_name,
            }
        }
    };

    expanded.into()
}

/// Build a structured `Cmd` from shell-like syntax.
///
/// Whitespace separates arguments. `{expr}` interpolates a Rust expression
/// as a single argument. `{expr...}` splats an `IntoIterator` as zero or
/// more arguments — useful for `Option<T>` (0/1 args), `Vec<T>`, slices, etc.
/// Quoted strings (`"..."`) are single arguments. Adjacent tokens (no
/// whitespace) merge into one argument.
///
/// ```ignore
/// // These are equivalent:
/// cmd!(curl -X POST {&url} -H "Content-Type: application/json")
/// Cmd::new("curl").arg("-X").arg("POST").arg(&url).arg("-H").arg("Content-Type: application/json")
///
/// // Splat for optional flags and lists:
/// let verbose = debug.then_some("--verbose");
/// cmd!(cargo test {verbose...} {test_names...})
/// ```
#[proc_macro]
pub fn cmd(input: TokenStream) -> TokenStream {
    match cmd_macro::expand_cmd(input.into()) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}
