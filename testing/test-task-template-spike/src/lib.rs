//! Hand-rolled prototype for the `task_template` design.
//!
//! This crate exists to validate the central design assumption of
//! `docs/task_templates.md`: a `#[macro_export] macro_rules!` defined
//! in a library crate, invoked at a consumer's RUNME.rs site, can stamp
//! out a fully-local typed task registration whose `TaskDef::group` and
//! `TaskDef::dir` come from the *consumer's* `__RNME_GROUP` /
//! `__RNME_DIR` constants, while the underlying body / wrapper / arg
//! metadata functions live in this (library) crate.
//!
//! Nothing in this file uses any proc macro — it is the literal token
//! shape that `#[rnme::task_template]` will eventually emit. If this
//! compiles and registers correctly, the proc-macro implementation is
//! just codegen of these same shapes.
//!
//! # What `#[rnme::task]` emits today, for comparison
//!
//! For an `async fn demo(ctx: &TaskContext) -> TaskResult`, the existing
//! `#[rnme::task]` macro produces (renamed for clarity):
//!
//!   1. `fn __rnme_body_demo(ctx) -> impl Future<Output = TaskResult>` — the user body.
//!   2. `fn __runme_taskfn_demo(ctx, &[String]) -> Pin<Box<dyn Future>>` — string-args wrapper.
//!   3. `fn __runme_argmeta_demo() -> Option<clap::Command>` — arg metadata.
//!   4. `pub static __RNME_TASKDEF_demo: TaskDef = TaskDef { group: __RNME_GROUP, dir: __RNME_DIR, ... };`
//!   5. `inventory::submit!(TaskDefRef(&__RNME_TASKDEF_demo));`
//!   6. `pub fn demo(ctx) -> TaskBuilder { TaskBuilder::from_factory(..., factory_closure_calling_body) }`
//!
//! # What the template design splits
//!
//! Items (1)–(3) live in the library (this file). The library has no
//! `__RNME_GROUP` / `__RNME_DIR`, so it cannot emit (4)–(6). Instead,
//! the library exposes a `#[macro_export] macro_rules! __rnme_stamp_demo!`
//! whose body, when invoked at the consumer site, emits (4)–(6) — with
//! the consumer's `__RNME_GROUP` / `__RNME_DIR` substituted in.

use rnme::prelude::*;

// =====================================================================
// (1) Renamed user body — `pub` so the stamped-out factory at the
// consumer site can reach it as `$crate::__rnme_body_demo`.
//
// Note: `#[rnme::task]` injects a `let _task = ctx.start_task("demo");`
// as the first statement of the body. The plan says template stamping
// should inject `start_task` at *stamp time* so the task's tracing
// span carries the consumer's name. For the spike, we put it here in
// the body — both placements compile; the design's tracing-span
// concern is orthogonal to the path-resolution risk this spike exists
// to settle.
// =====================================================================
pub async fn __rnme_body_demo(ctx: &TaskContext) -> TaskResult {
    let _task = ctx.start_task("demo");
    // Print the consumer-resolved task_dir so the spike test can
    // assert that the imported task ran in the consumer's directory,
    // not the library's location.
    let dir = ctx
        .task_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unset>".to_string());
    info!("demo template task running in: {dir}");
    println!("demo task_dir = {dir}");
    Ok(())
}

// =====================================================================
// (2) String-args wrapper — `pub`. Identical in shape to what
// `#[rnme::task]` emits for a Form-1 (zero-arg) task. Stays in the
// library; the stamped-out consumer-site `TaskDef` references it via
// `$crate::__runme_taskfn_demo`.
// =====================================================================
pub fn __runme_taskfn_demo<'a>(
    ctx: &'a TaskContext,
    _args: &[String],
) -> ::std::pin::Pin<
    ::std::boxed::Box<
        dyn ::std::future::Future<Output = ::std::result::Result<(), ::rnme::error::TaskError>>
            + Send
            + 'a,
    >,
> {
    ::std::boxed::Box::pin(async move { __rnme_body_demo(ctx).await })
}

// =====================================================================
// (3) Arg-metadata function — `pub`. Form-1 has no args.
// =====================================================================
pub fn __runme_argmeta_demo() -> ::std::option::Option<::rnme::clap::Command> {
    None
}

// =====================================================================
// Per-task stamp helper. `#[macro_export]` makes it referenceable as
// `rnme_test_task_template_spike::__rnme_stamp_demo!` at any consumer
// site. The body, when expanded at the consumer's RUNME.rs:
//
//   - Reads `__RNME_GROUP` and `__RNME_DIR` as bare identifiers, so they
//     bind to the consumer's `const __RNME_GROUP: &str = ...;` /
//     `const __RNME_DIR: &str = ...;` (or the codegen-injected ones in
//     a real RUNME.rs). Bare idents in macro_rules expansion have
//     *call-site* scope.
//
//   - Refers to the library's body / wrapper / argmeta fns via
//     `$crate::__rnme_body_demo`, `$crate::__runme_taskfn_demo`, and
//     `$crate::__runme_argmeta_demo`. `$crate` in a `#[macro_export]`
//     macro expands to the absolute path of the *defining* crate —
//     which is this crate, regardless of how the consumer imports it.
//
//   - Emits `pub static __RNME_TASKDEF_demo`, the `inventory::submit!`,
//     and the typed `pub fn demo(ctx) -> TaskBuilder` shim.
//
// All three of these must work *simultaneously* in one expansion. That
// asymmetry — call-site idents alongside def-site `$crate` paths in
// the same emission — is the central risk this spike exists to settle.
// =====================================================================
#[macro_export]
macro_rules! __rnme_stamp_demo {
    () => {
        #[allow(non_upper_case_globals)]
        pub static __RNME_TASKDEF_demo: ::rnme::task::TaskDef = ::rnme::task::TaskDef {
            name: "demo",
            description: ::std::option::Option::Some(
                "Demo task template (hand-rolled spike).",
            ),
            // Call-site idents — must resolve to the consumer's consts.
            group: __RNME_GROUP,
            dir: __RNME_DIR,
            // Def-site `$crate` paths — must resolve back to this library
            // regardless of how the consumer aliased the import.
            func: ::rnme::task::TaskFnKind::Static($crate::__runme_taskfn_demo),
            arg_metadata: $crate::__runme_argmeta_demo,
            ui_hint: ::std::option::Option::None,
        };

        ::rnme::inventory::submit! {
            ::rnme::task::TaskDefRef(&__RNME_TASKDEF_demo)
        }

        #[must_use = "task builders do nothing until `.await` or `.spawn()` — \
                      a bare call constructs the builder and drops it"]
        pub fn demo(
            ctx: &::rnme::task::TaskContext,
        ) -> ::rnme::execution::builder::TaskBuilder {
            ::rnme::execution::builder::TaskBuilder::from_factory(
                ctx,
                &__RNME_TASKDEF_demo,
                ::std::boxed::Box::new(
                    |body_ctx: &::rnme::task::TaskContext| {
                        ::std::boxed::Box::pin(async move {
                            $crate::__rnme_body_demo(body_ctx).await
                        })
                    },
                ),
            )
        }
    };
}

/// Linker anchor — required for `inventory` crates whose registrations
/// would otherwise be stripped. Note this anchor lives in the *library*
/// crate but the library doesn't `inventory::submit!` anything itself;
/// the `inventory::submit!` lives in the consumer-side stamp expansion.
/// So consumers don't need to call this anchor for inventory visibility
/// — but `compile.rs` in the rnme codegen emits `__rnme_link()` calls
/// per RUNME.rs crate already. The consumer's own RUNME.rs crate is what
/// gets its `__rnme_link()` called; that crate is where the stamped
/// `inventory::submit!` lives, so the consumer's link anchor pulls the
/// right symbols in.
///
/// Kept as a no-op here so hand-driven test harnesses that want to
/// link this crate's symbols (none, in practice) can call it.
pub fn __rnme_link() {}
