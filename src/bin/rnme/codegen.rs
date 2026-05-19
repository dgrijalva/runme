/// Information about a single RUNME.rs file to be compiled into the workspace.
pub(crate) struct CrateEntry {
    /// Valid Rust crate name derived from the relative path.
    pub(crate) crate_name: String,
    /// Group key for this file (relative directory path, e.g. "services/auth").
    /// Root is "". Currently used only in tests but preserved as metadata.
    #[allow(dead_code)]
    pub(crate) group_key: String,
    /// Transformed source code (shebang stripped, __RNME_GROUP injected, __rnme_link appended).
    pub(crate) lib_source: String,
    /// Cargo.toml content for this lib crate.
    pub(crate) cargo_toml: String,
    /// Transitive descendant crate names this entry depends on through the
    /// `mod subtasks { ... }` tree. Single source of truth for both the
    /// Cargo.toml path-deps and the acyclicity guard.
    pub(crate) descendant_crate_names: Vec<String>,
}

/// Generate the runner crate's main.rs source.
///
/// The runner main:
/// 1. Calls __rnme_link() on each lib crate to force linker inclusion of inventory registrations
/// 2. Runs init hooks (leaf-to-root, by counting `/` in group key)
/// 3. Builds a Registry from inventory
/// 4. Parses CLI args and dispatches
pub(crate) fn generate_runner_main(entries: &[CrateEntry]) -> String {
    let mut source = String::new();

    // __rnme_link calls
    source.push_str("fn main() {\n");
    for entry in entries {
        source.push_str(&format!("    {}::__rnme_link();\n", entry.crate_name));
    }
    source.push('\n');

    // Build tokio runtime, run init hooks, and hand off to cli::run()
    source.push_str(
        r#"    rnme::tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
        .block_on(async {
            // Run init hooks: collect from inventory, sort leaf-to-root
            // (deeper group keys first, by counting '/' separators)
            let mut inits: Vec<&rnme::init::InitDef> = rnme::inventory::iter::<rnme::init::InitDef>.into_iter().collect();
            inits.sort_by(|a, b| {
                let depth_a = if a.group.is_empty() { 0 } else { a.group.matches('/').count() + 1 };
                let depth_b = if b.group.is_empty() { 0 } else { b.group.matches('/').count() + 1 };
                depth_b.cmp(&depth_a)
            });

            // Collect group display names (start with key, overridable by init)
            let mut group_names: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
            for group in rnme::inventory::iter::<rnme::init::GroupDef> {
                group_names.insert(group.key, group.key.to_string());
            }

            let mut dynamic_tasks: Vec<&'static rnme::task::TaskDef> = Vec::new();
            for init in &inits {
                let default_name = group_names.get(init.group).cloned().unwrap_or_else(|| init.group.to_string());
                let mut ctx = rnme::init::InitContext::new(&default_name);
                (init.func)(&mut ctx);
                group_names.insert(init.group, ctx.group_name().to_string());
                dynamic_tasks.extend(ctx.drain_tasks());
            }

            // Build registry from inventory + dynamic tasks
            let mut registry = rnme::task::Registry::from_inventory();
            for task in dynamic_tasks {
                registry.register(task);
            }
            let registry = std::sync::Arc::new(registry);
            let group_names_owned: std::collections::HashMap<String, String> = group_names
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();

            rnme::cli::run(registry, group_names_owned).await;
        });
}
"#,
    );

    source
}
