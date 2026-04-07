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

/// InitDef — registered via inventory by `#[runme::init]`.
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

/// InitContext — passed to init functions registered via `#[runme::init]`.
///
/// Pre-populated with the path-based group name. The init function can
/// override the display name or configure other per-file settings.
pub struct InitContext {
    group_name: String,
}

impl InitContext {
    /// Create a new InitContext with the default group name.
    pub fn new(default_group: &str) -> Self {
        Self {
            group_name: default_group.to_string(),
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
}
