//! Per-file initialization hook.
//!
//! Each `RUNME.rs` may define one `#[rnme::init]` function that runs
//! before any task in that file. Use it to:
//!
//! - Override the display name for the file's task group
//! - Register dynamic tasks discovered at runtime (e.g., one task per
//!   workspace member)
//!
//! ```rust,ignore
//! use rnme::prelude::*;
//!
//! #[rnme::init]
//! fn setup(ctx: &mut InitContext) {
//!     ctx.set_group_name("Auth Service");
//!
//!     for sub in ["build", "test", "clippy"] {
//!         let sub = sub.to_string();
//!         ctx.register_task(&sub, Some(&format!("cargo {sub}")), move |ctx, _args| {
//!             let sub = sub.clone();
//!             Box::pin(async move {
//!                 ctx.exec(format!("cargo {sub}")).await?.ok()?;
//!                 Ok(())
//!             })
//!         });
//!     }
//! }
//! ```
//!
//! Init runs once per process at startup — there is no async runtime
//! available inside it. For async setup, do the work in a task instead.

/// GroupDef — registered via inventory, one per RUNME.rs file.
///
/// Each RUNME.rs file produces a `GroupDef` that maps the file's group key
/// (derived from its relative path) to a display name. The display name
/// defaults to the key but can be overridden via `InitContext::set_group_name`.
pub struct GroupDef {
    pub key: &'static str,
}

// Safety: GroupDef contains only 'static references, which are inherently Send + Sync.
unsafe impl Send for GroupDef {}
unsafe impl Sync for GroupDef {}

inventory::collect!(GroupDef);

/// InitDef — registered via inventory by `#[rnme::init]`.
///
/// Each RUNME.rs file can optionally define an init hook that runs before
/// tasks are dispatched. The init function receives an `InitContext` scoped
/// to the file's own configuration.
pub struct InitDef {
    pub group: &'static str,
    pub func: fn(&mut InitContext),
}

// Safety: InitDef contains only 'static references and function pointers,
// which are inherently Send + Sync.
unsafe impl Send for InitDef {}
unsafe impl Sync for InitDef {}

inventory::collect!(InitDef);

/// InitContext — passed to init functions registered via `#[rnme::init]`.
///
/// Pre-populated with the path-based group name. The init function can
/// override the display name, register dynamic tasks, or configure
/// other per-file settings.
pub struct InitContext {
    group_name: String,
    dynamic_tasks: Vec<&'static crate::task::TaskDef>,
}

impl InitContext {
    /// Create a new InitContext with the default group name.
    pub fn new(default_group: &str) -> Self {
        Self {
            group_name: default_group.to_string(),
            dynamic_tasks: Vec::new(),
        }
    }

    /// Override the display name for this file's group.
    pub fn set_group_name(&mut self, name: &str) {
        self.group_name = name.to_string();
    }

    /// Get the current group name (display name).
    pub fn group_name(&self) -> &str {
        &self.group_name
    }

    /// Register a dynamic task discovered at init time.
    ///
    /// The task's name, description, and group are leaked to `&'static str`
    /// so the resulting `TaskDef` has the same lifetime as macro-generated tasks.
    /// This is fine — dynamic tasks live for the entire process.
    ///
    /// # Example
    ///
    /// ```ignore
    /// #[rnme::init]
    /// fn setup(ctx: &mut InitContext) {
    ///     for cmd in ["build", "test", "clippy"] {
    ///         let cmd = cmd.to_string();
    ///         ctx.register_task(&cmd, Some(&format!("cargo {}", cmd)), move |ctx, _args| {
    ///             let cmd = cmd.clone();
    ///             Box::pin(async move {
    ///                 ctx.exec(format!("cargo {}", cmd)).await?.ok()?;
    ///                 Ok(())
    ///             })
    ///         });
    ///     }
    /// }
    /// ```
    pub fn register_task<F>(&mut self, name: &str, description: Option<&str>, func: F)
    where
        F: for<'a> Fn(
                &'a crate::task::TaskContext,
                &[String],
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<(), crate::error::TaskError>>
                        + Send
                        + 'a,
                >,
            > + Send
            + Sync
            + 'static,
    {
        let leaked_name: &'static str = Box::leak(name.to_string().into_boxed_str());
        let leaked_desc: Option<&'static str> =
            description.map(|d| &*Box::leak(d.to_string().into_boxed_str()));
        let leaked_group: &'static str = Box::leak(self.group_name.clone().into_boxed_str());

        let task_def = Box::leak(Box::new(crate::task::TaskDef {
            name: leaked_name,
            description: leaked_desc,
            group: leaked_group,
            func: crate::task::TaskFnKind::Dynamic(std::sync::Arc::new(func)),
            arg_metadata: || None,
            ui_hint: None,
        }));

        self.dynamic_tasks.push(task_def);
    }

    /// Drain collected dynamic tasks. Called by the runner after init completes.
    pub fn drain_tasks(&mut self) -> Vec<&'static crate::task::TaskDef> {
        std::mem::take(&mut self.dynamic_tasks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_context_default() {
        let ctx = InitContext::new("services/auth");
        assert_eq!(ctx.group_name(), "services/auth");
    }

    #[test]
    fn test_init_context_override() {
        let mut ctx = InitContext::new("services/auth");
        ctx.set_group_name("Auth Service");
        assert_eq!(ctx.group_name(), "Auth Service");
    }

    #[test]
    fn test_init_context_empty_default() {
        let ctx = InitContext::new("");
        assert_eq!(ctx.group_name(), "");
    }

    #[test]
    fn test_set_group_name_multiple_times_last_wins() {
        let mut ctx = InitContext::new("initial");
        ctx.set_group_name("second");
        ctx.set_group_name("third");
        assert_eq!(ctx.group_name(), "third");
    }

    #[test]
    fn test_group_name_returns_reference() {
        let ctx = InitContext::new("my-group");
        // group_name() returns &str — verify it's borrowing from the struct
        let name: &str = ctx.group_name();
        assert_eq!(name, "my-group");
    }

    #[test]
    fn test_new_with_empty_string() {
        let ctx = InitContext::new("");
        // Explicit test that empty-string construction produces an empty group name
        assert_eq!(ctx.group_name(), "");
        assert!(ctx.group_name().is_empty());
    }
}
