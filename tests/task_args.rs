//! Integration tests for #[rnme::task] argument forms.
//!
//! Tests the three argument forms:
//! - Form 1: Zero args (ctx only)
//! - Form 2: Simple args (ctx + primitives)
//! - Form 3: Parser struct (ctx + clap derive struct)

use rnme::prelude::*;

const __RNME_GROUP: &str = "";

// ============================================================
// Form 1: Zero args
// ============================================================

#[rnme::task(desc = "A zero-arg task")]
async fn zero_arg_task(ctx: &TaskContext) -> TaskResult {
    info!("running zero_arg_task: {}", ctx.name);
    Ok(())
}

#[tokio::test]
async fn test_zero_arg_task_runs() {
    let reg = Registry::from_inventory();
    // Run with no args
    reg.run_with_args("zero_arg_task", &[]).await.unwrap();
}

#[tokio::test]
async fn test_zero_arg_metadata_is_none() {
    let reg = Registry::from_inventory();
    let task = reg.get("zero_arg_task").unwrap();
    assert!(
        (task.arg_metadata)().is_none(),
        "zero-arg task should have no arg metadata"
    );
}

// ============================================================
// Form 2: Simple args — multiple params
// ============================================================

#[rnme::task(desc = "A task with simple args")]
async fn simple_args_task(
    ctx: &TaskContext,
    env: String,
    port: u16,
    verbose: bool,
) -> TaskResult {
    info!(
        "simple_args_task: env={}, port={}, verbose={}",
        env, port, verbose
    );
    let _ = ctx;
    Ok(())
}

#[tokio::test]
async fn test_simple_args_parses_correctly() {
    let reg = Registry::from_inventory();
    let args: Vec<String> = vec![
        "--env".into(),
        "production".into(),
        "--port".into(),
        "8080".into(),
        "--verbose".into(),
    ];
    reg.run_with_args("simple_args_task", &args).await.unwrap();
}

#[tokio::test]
async fn test_simple_args_missing_required() {
    let reg = Registry::from_inventory();
    // Missing --env and --port
    let args: Vec<String> = vec!["--verbose".into()];
    let result = reg.run_with_args("simple_args_task", &args).await;
    assert!(result.is_err(), "should fail when required args are missing");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("required"),
        "error should mention 'required': {}",
        err_msg
    );
}

#[tokio::test]
async fn test_simple_args_invalid_type() {
    let reg = Registry::from_inventory();
    let args: Vec<String> = vec![
        "--env".into(),
        "staging".into(),
        "--port".into(),
        "not_a_number".into(),
    ];
    let result = reg.run_with_args("simple_args_task", &args).await;
    assert!(result.is_err(), "should fail on invalid type for port");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("invalid") || err_msg.contains("parse"),
        "error should mention invalid parsing: {}",
        err_msg
    );
}

#[tokio::test]
async fn test_simple_args_metadata_returns_command() {
    let reg = Registry::from_inventory();
    let task = reg.get("simple_args_task").unwrap();
    let cmd = (task.arg_metadata)();
    assert!(cmd.is_some(), "simple-args task should have arg metadata");
    let cmd = cmd.unwrap();
    assert_eq!(cmd.get_name(), "simple_args_task");
    // Check that the command has the expected args
    let arg_names: Vec<&str> = cmd.get_arguments().map(|a| a.get_id().as_str()).collect();
    assert!(arg_names.contains(&"env"), "should have env arg");
    assert!(arg_names.contains(&"port"), "should have port arg");
    assert!(arg_names.contains(&"verbose"), "should have verbose arg");
}

// ============================================================
// Form 2: Simple args — single primitive param
// ============================================================

#[rnme::task(desc = "A task with a single string arg")]
async fn single_string_task(ctx: &TaskContext, name: String) -> TaskResult {
    info!("single_string_task: name={}", name);
    let _ = ctx;
    Ok(())
}

#[tokio::test]
async fn test_single_string_is_form2() {
    let reg = Registry::from_inventory();
    let args: Vec<String> = vec!["--name".into(), "hello".into()];
    reg.run_with_args("single_string_task", &args).await.unwrap();
}

#[tokio::test]
async fn test_single_string_metadata() {
    let reg = Registry::from_inventory();
    let task = reg.get("single_string_task").unwrap();
    let cmd = (task.arg_metadata)();
    assert!(cmd.is_some());
}

// ============================================================
// Form 2: Simple args — Optional and Vec params
// ============================================================

#[rnme::task(desc = "A task with optional and vec args")]
async fn optional_vec_task(
    ctx: &TaskContext,
    label: String,
    count: Option<u32>,
    tags: Vec<String>,
) -> TaskResult {
    info!(
        "optional_vec_task: label={}, count={:?}, tags={:?}",
        label, count, tags
    );
    let _ = ctx;
    Ok(())
}

#[tokio::test]
async fn test_optional_present() {
    let reg = Registry::from_inventory();
    let args: Vec<String> = vec![
        "--label".into(),
        "test".into(),
        "--count".into(),
        "5".into(),
        "--tags".into(),
        "a".into(),
        "--tags".into(),
        "b".into(),
    ];
    reg.run_with_args("optional_vec_task", &args).await.unwrap();
}

#[tokio::test]
async fn test_optional_absent() {
    let reg = Registry::from_inventory();
    // count and tags omitted
    let args: Vec<String> = vec!["--label".into(), "test".into()];
    reg.run_with_args("optional_vec_task", &args).await.unwrap();
}

// ============================================================
// Form 2: Underscore-to-dash conversion
// ============================================================

#[rnme::task(desc = "A task with underscored param names")]
async fn underscore_task(ctx: &TaskContext, my_flag: bool, my_value: String) -> TaskResult {
    info!("underscore_task: my_flag={}, my_value={}", my_flag, my_value);
    let _ = ctx;
    Ok(())
}

#[tokio::test]
async fn test_underscore_to_dash_in_flags() {
    let reg = Registry::from_inventory();
    let args: Vec<String> = vec![
        "--my-flag".into(),
        "--my-value".into(),
        "hello".into(),
    ];
    reg.run_with_args("underscore_task", &args).await.unwrap();
}

// ============================================================
// Form 3: Parser struct
// ============================================================

#[derive(clap::Parser, Debug)]
struct DeployArgs {
    /// Target environment
    #[arg(long)]
    env: String,

    /// Port number
    #[arg(long, default_value = "3000")]
    port: u16,
}

#[rnme::task(desc = "A task with a parser struct")]
async fn parser_struct_task(ctx: &TaskContext, args: DeployArgs) -> TaskResult {
    info!(
        "parser_struct_task: env={}, port={}",
        args.env, args.port
    );
    let _ = ctx;
    Ok(())
}

#[tokio::test]
async fn test_parser_struct_runs() {
    let reg = Registry::from_inventory();
    let args: Vec<String> = vec!["--env".into(), "staging".into(), "--port".into(), "9090".into()];
    reg.run_with_args("parser_struct_task", &args).await.unwrap();
}

#[tokio::test]
async fn test_parser_struct_default_values() {
    let reg = Registry::from_inventory();
    // port has a default_value in the derive, so omitting it should work
    let args: Vec<String> = vec!["--env".into(), "dev".into()];
    reg.run_with_args("parser_struct_task", &args).await.unwrap();
}

#[tokio::test]
async fn test_parser_struct_missing_required() {
    let reg = Registry::from_inventory();
    // Missing --env which is required
    let args: Vec<String> = vec![];
    let result = reg.run_with_args("parser_struct_task", &args).await;
    assert!(result.is_err(), "should fail when required arg is missing");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("required") || err_msg.contains("env"),
        "error should mention the missing arg: {}",
        err_msg
    );
}

#[tokio::test]
async fn test_parser_struct_metadata_returns_command() {
    let reg = Registry::from_inventory();
    let task = reg.get("parser_struct_task").unwrap();
    let cmd = (task.arg_metadata)();
    assert!(
        cmd.is_some(),
        "parser struct task should have arg metadata"
    );
    let cmd = cmd.unwrap();
    // The command should have the args defined in DeployArgs
    let arg_names: Vec<&str> = cmd.get_arguments().map(|a| a.get_id().as_str()).collect();
    assert!(arg_names.contains(&"env"), "should have env arg");
    assert!(arg_names.contains(&"port"), "should have port arg");
}

#[tokio::test]
async fn test_parser_struct_help_error() {
    let reg = Registry::from_inventory();
    let args: Vec<String> = vec!["--help".into()];
    let result = reg.run_with_args("parser_struct_task", &args).await;
    // --help causes clap to exit with an error (or print help)
    assert!(result.is_err(), "help flag should produce an error result");
}
