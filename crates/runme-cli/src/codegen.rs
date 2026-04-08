/// Information about a single RUNME.rs file to be compiled into the workspace.
pub(crate) struct CrateEntry {
    /// Valid Rust crate name derived from the relative path.
    pub(crate) crate_name: String,
    /// Group key for this file (relative directory path, e.g. "services/auth").
    /// Root is "". Currently used only in tests but preserved as metadata.
    #[allow(dead_code)]
    pub(crate) group_key: String,
    /// Transformed source code (shebang stripped, __RUNME_GROUP injected, __runme_link appended).
    pub(crate) lib_source: String,
    /// Cargo.toml content for this lib crate.
    pub(crate) cargo_toml: String,
}

/// Generate the runner crate's main.rs source.
///
/// The runner main:
/// 1. Calls __runme_link() on each lib crate to force linker inclusion of inventory registrations
/// 2. Runs init hooks (leaf-to-root, by counting `/` in group key)
/// 3. Builds a Registry from inventory
/// 4. Parses CLI args and dispatches
pub(crate) fn generate_runner_main(entries: &[CrateEntry]) -> String {
    let mut source = String::new();

    // __runme_link calls
    source.push_str("fn main() {\n");
    for entry in entries {
        source.push_str(&format!("    {}::__runme_link();\n", entry.crate_name));
    }
    source.push('\n');

    // Build tokio runtime and dispatch
    source.push_str(
        r#"    runme::tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
        .block_on(async {
            // Run init hooks: collect from inventory, sort leaf-to-root
            // (deeper group keys first, by counting '/' separators)
            let mut inits: Vec<&runme::init::InitDef> = runme::inventory::iter::<runme::init::InitDef>.into_iter().collect();
            inits.sort_by(|a, b| {
                let depth_a = if a.group.is_empty() { 0 } else { a.group.matches('/').count() + 1 };
                let depth_b = if b.group.is_empty() { 0 } else { b.group.matches('/').count() + 1 };
                depth_b.cmp(&depth_a)
            });

            // Collect group display names (start with key, overridable by init)
            let mut group_names: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
            for group in runme::inventory::iter::<runme::init::GroupDef> {
                group_names.insert(group.key, group.key.to_string());
            }

            for init in &inits {
                let default_name = group_names.get(init.group).cloned().unwrap_or_else(|| init.group.to_string());
                let mut ctx = runme::init::InitContext::new(&default_name);
                (init.func)(&mut ctx);
                group_names.insert(init.group, ctx.group_name().to_string());
            }

            // Build registry from inventory
            let registry = runme::task::Registry::from_inventory();
            let args: Vec<String> = std::env::args().collect();

            if args.iter().any(|a| a == "--list") {
                for task in registry.list() {
                    let group_display = group_names.get(task.group).map(|s| s.as_str()).unwrap_or(task.group);
                    if group_display.is_empty() {
                        println!("{}: {}", task.name, task.description.unwrap_or(""));
                    } else {
                        println!("[{}] {}: {}", group_display, task.name, task.description.unwrap_or(""));
                    }
                }
                return;
            }

            if let Some(task_name) = args.get(1) {
                // Resolve task: short name when unambiguous, "group:task" to disambiguate.
                // Group names come from group_names map (init hooks can override).
                let task_group_name = |t: &&runme::task::TaskDef| -> String {
                    group_names.get(t.group).cloned().unwrap_or_else(|| t.group.to_string())
                };

                let resolved = if let Some((group_query, short_name)) = task_name.split_once(':') {
                    registry.list().iter()
                        .find(|t| t.name == short_name && task_group_name(t) == group_query)
                        .copied()
                        .ok_or_else(|| format!("unknown task: {}", task_name))
                } else {
                    let matches: Vec<_> = registry.list().iter()
                        .filter(|t| t.name == task_name.as_str())
                        .collect();
                    match matches.len() {
                        0 => Err(format!("unknown task: {}", task_name)),
                        1 => Ok(*matches[0]),
                        _ => {
                            // Root tasks (empty group) win short names
                            if let Some(root_task) = matches.iter().find(|t| t.group.is_empty()) {
                                Ok(**root_task)
                            } else {
                                let qualified: Vec<String> = matches.iter().map(|t| {
                                    format!("{}:{}", task_group_name(t), t.name)
                                }).collect();
                                Err(format!("ambiguous task '{}', use: {}", task_name, qualified.join(", ")))
                            }
                        }
                    }
                };

                match resolved {
                    Ok(task) => {
                        let mut app = runme::tui::App::with_task(task);
                        if let Err(e) = app.run().await {
                            eprintln!("TUI error: {}", e);
                            std::process::exit(1);
                        }
                    }
                    Err(msg) => {
                        eprintln!("Error: {}", msg);
                        std::process::exit(1);
                    }
                }
            } else {
                // No task specified — launch TUI with task picker
                let tasks: Vec<&'static runme::task::TaskDef> = registry.list().to_vec();
                let group_names_owned: std::collections::HashMap<String, String> = group_names
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect();
                let mut app = runme::tui::App::with_picker(tasks, group_names_owned);
                if let Err(e) = app.run().await {
                    eprintln!("TUI error: {}", e);
                    std::process::exit(1);
                }
            }
        });
}
"#,
    );

    source
}
