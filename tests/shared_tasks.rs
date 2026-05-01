//! Integration tests for cross-crate inventory visibility.
//!
//! These tests validate that tasks defined in a separate library crate
//! (`rnme-test-tasks`) are visible via `Registry::from_inventory()` when
//! the `__rnme_link()` pattern is used.
//!
//! ## The problem
//!
//! `inventory` uses linker sections (e.g., `__DATA,__inventory` on macOS,
//! `.init_array` on Linux) to register items at load time. When a library
//! crate contributes registrations but no symbols from that crate are
//! referenced by the binary, the linker strips the entire translation unit
//! — including the inventory sections. Tasks silently disappear.
//!
//! ## The solution
//!
//! Each shared task crate exports a no-op function:
//!
//! ```ignore
//! pub fn __rnme_link() {}
//! ```
//!
//! The consuming binary calls it before iterating inventory. This forces
//! the linker to include the crate, preserving the `TaskDef` registrations.
//!
//! In the generated runner binary, `compile.rs` emits a call to
//! `<crate>::__rnme_link()` for every RUNME.rs crate in the workspace.
//! For hand-authored shared crates, users must call it manually.
//!
//! ## Minimum pattern for a shared task library
//!
//! ```ignore
//! // shared_tasks/src/lib.rs
//! use rnme::prelude::*;
//!
//! const __RNME_GROUP: &str = "my_group";
//!
//! /// A reusable task
//! #[rnme::task]
//! async fn my_task(ctx: &TaskContext) -> TaskResult {
//!     // ...
//!     Ok(())
//! }
//!
//! pub fn __rnme_link() {}
//! ```
//!
//! ```ignore
//! // consumer/main.rs (or test)
//! fn main() {
//!     shared_tasks::__rnme_link();
//!     let reg = Registry::from_inventory();
//!     // Tasks from shared_tasks are now visible
//! }
//! ```

use rnme::prelude::*;

// Required: force the linker to include rnme-test-tasks' inventory sections.
// Without this call, the tasks from that crate would be silently dropped.
fn link_shared_crate() {
    rnme_test_tasks::__rnme_link();
}

// ============================================================
// Test 1: Tasks from the shared crate are visible after __rnme_link()
// ============================================================

#[tokio::test]
async fn test_shared_tasks_visible_with_link() {
    link_shared_crate();
    let reg = Registry::from_inventory();

    // The shared crate defines: greet, shared_echo, group_check
    assert!(
        reg.get("greet").is_some(),
        "greet task should be visible from shared crate"
    );
    assert!(
        reg.get("shared_echo").is_some(),
        "shared_echo task should be visible from shared crate"
    );
    assert!(
        reg.get("group_check").is_some(),
        "group_check task should be visible from shared crate"
    );
}

// ============================================================
// Test 2: Shared tasks have the correct group
// ============================================================

#[tokio::test]
async fn test_shared_tasks_have_correct_group() {
    link_shared_crate();
    let reg = Registry::from_inventory();

    let greet = reg.get("greet").expect("greet should exist");
    assert_eq!(
        greet.group, "shared",
        "shared crate tasks should have group 'shared'"
    );

    let echo = reg.get("shared_echo").expect("shared_echo should exist");
    assert_eq!(echo.group, "shared");

    let check = reg.get("group_check").expect("group_check should exist");
    assert_eq!(check.group, "shared");
}

// ============================================================
// Test 3: Shared tasks have descriptions
// ============================================================

#[tokio::test]
async fn test_shared_tasks_have_descriptions() {
    link_shared_crate();
    let reg = Registry::from_inventory();

    let greet = reg.get("greet").unwrap();
    assert_eq!(greet.description, Some("A shared task that greets"));

    let echo = reg.get("shared_echo").unwrap();
    assert_eq!(echo.description, Some("A shared task with args"));

    let check = reg.get("group_check").unwrap();
    assert_eq!(check.description, Some("Reports its own group"));
}

// ============================================================
// Test 4: Shared tasks can be executed
// ============================================================

#[tokio::test]
async fn test_shared_task_executes() {
    link_shared_crate();
    let reg = Registry::from_inventory();
    reg.run("greet").await.unwrap();
}

// ============================================================
// Test 5: Shared tasks with arguments work cross-crate
// ============================================================

#[tokio::test]
async fn test_shared_task_with_args() {
    link_shared_crate();
    let reg = Registry::from_inventory();
    let args: Vec<String> = vec!["--message".into(), "hello from test".into()];
    reg.run_with_args("shared_echo", &args).await.unwrap();
}

// ============================================================
// Test 6: Shared task arg metadata is available cross-crate
// ============================================================

#[tokio::test]
async fn test_shared_task_arg_metadata() {
    link_shared_crate();
    let reg = Registry::from_inventory();

    // greet has no args → metadata should be None
    let greet = reg.get("greet").unwrap();
    assert!(
        (greet.arg_metadata)().is_none(),
        "zero-arg shared task should have no arg metadata"
    );

    // shared_echo has a `message: String` arg → metadata should be Some
    let echo = reg.get("shared_echo").unwrap();
    let cmd = (echo.arg_metadata)();
    assert!(
        cmd.is_some(),
        "shared task with args should have arg metadata"
    );
    let cmd = cmd.unwrap();
    let arg_names: Vec<&str> = cmd.get_arguments().map(|a| a.get_id().as_str()).collect();
    assert!(
        arg_names.contains(&"message"),
        "shared_echo should have 'message' arg, found: {:?}",
        arg_names
    );
}

// ============================================================
// Test 7: Shared tasks coexist with local tasks
// ============================================================

const __RNME_GROUP: &str = "";

/// A local task in the test binary
#[rnme::task]
async fn local_test_task(ctx: &TaskContext) -> TaskResult {
    info!("local_test_task: {}", ctx.name);
    Ok(())
}

#[tokio::test]
async fn test_shared_and_local_tasks_coexist() {
    link_shared_crate();
    let reg = Registry::from_inventory();

    // Local task should be visible
    assert!(
        reg.get("local_test_task").is_some(),
        "local task should be visible"
    );

    // Shared tasks should also be visible
    assert!(
        reg.get("greet").is_some(),
        "shared greet task should be visible alongside local tasks"
    );
    assert!(
        reg.get("shared_echo").is_some(),
        "shared shared_echo task should be visible alongside local tasks"
    );
}

#[tokio::test]
async fn test_shared_and_local_have_different_groups() {
    link_shared_crate();
    let reg = Registry::from_inventory();

    let local = reg.get("local_test_task").unwrap();
    assert_eq!(local.group, "", "local task should have root group");

    let shared = reg.get("greet").unwrap();
    assert_eq!(
        shared.group, "shared",
        "shared task should have 'shared' group"
    );
}

// ============================================================
// Test 8: TaskInfo qualified names work for shared tasks
// ============================================================

#[tokio::test]
async fn test_shared_task_qualified_names() {
    link_shared_crate();
    let reg = Registry::from_inventory();

    let greet = reg.get("greet").unwrap();
    let info = TaskInfo::from_def(greet);
    assert_eq!(
        info.qualified_name, "shared:greet",
        "shared task should have qualified name 'shared:greet'"
    );

    let local = reg.get("local_test_task").unwrap();
    let info = TaskInfo::from_def(local);
    assert_eq!(
        info.qualified_name, "local_test_task",
        "root task should have unqualified name"
    );
}
