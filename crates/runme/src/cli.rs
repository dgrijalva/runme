//! CLI argument model and dispatch logic.
//!
//! The two-stage model: `RunmeArgs` captures global flags (`--ui`, `--format`,
//! `--timeout`, `--filter`) and a trailing `rest` vector. The first element of
//! `rest` is the task name; everything after it is forwarded to the task's own
//! argument parser.
//!
//! `cli::run()` is the single entry point called by the generated runner binary.
//! All dispatch logic lives here rather than in generated code.

use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use crate::log::{LogEntry, Stream};
use crate::task::{Registry, TaskContext};
use crate::tui::App;

/// UI mode for task execution.
#[derive(Clone, Debug, clap::ValueEnum)]
pub enum UiMode {
    /// Interactive TUI with log viewer
    Tui,
    /// Direct CLI execution with stdio output
    Cli,
    /// Structured output for machine consumption
    Agent,
}

/// Output format for CLI and Agent modes.
#[derive(Clone, Debug, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable text
    Text,
    /// Structured JSON
    Json,
}

/// Top-level CLI arguments for the generated runner binary.
///
/// Global flags are parsed first; the trailing `rest` vector carries the task
/// name and any task-specific arguments that are forwarded to the task's own
/// parser.
#[derive(Parser)]
#[command(name = "runme")]
pub struct RunmeArgs {
    /// UI mode (defaults to tui when a terminal is available, or the task's default_ui)
    #[arg(long)]
    pub ui: Option<UiMode>,

    /// Output format (for cli and agent modes)
    #[arg(long, default_value = "text")]
    pub format: OutputFormat,

    /// Timeout (seconds, or with suffix: 10m, 1h)
    #[arg(long)]
    pub timeout: Option<String>,

    /// Log filter expression
    #[arg(long)]
    pub filter: Option<String>,

    /// Task name and task-specific arguments
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

/// Main dispatch function called by the generated runner binary.
///
/// Parses CLI arguments, resolves the task, and dispatches to the appropriate
/// UI mode (TUI, CLI, or Agent).
pub async fn run(registry: Arc<Registry>, group_names: HashMap<String, String>) {
    let args = RunmeArgs::parse();
    let has_terminal = std::io::IsTerminal::is_terminal(&std::io::stdout());

    if args.rest.is_empty() {
        // No task specified — resolve UI mode without a task hint
        let ui = resolve_ui_mode(args.ui, None, has_terminal);
        match ui {
            UiMode::Tui => {
                let tasks = registry.list().to_vec();
                let mut app = App::with_picker(tasks, group_names, registry.clone());
                if let Err(e) = app.run().await {
                    eprintln!("TUI error: {}", e);
                    std::process::exit(1);
                }
            }
            _ => {
                eprintln!("No task specified. Use --ui tui for the interactive picker.");
                std::process::exit(1);
            }
        }
        return;
    }

    let task_name = &args.rest[0];
    let task_args: Vec<String> = args.rest[1..].to_vec();

    let task = match registry.resolve(task_name) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    let ui = resolve_ui_mode(args.ui, task.ui_hint, has_terminal);

    match ui {
        UiMode::Tui => {
            let mut app = App::with_task(task, task_args, registry.clone());
            if let Err(e) = app.run().await {
                eprintln!("TUI error: {}", e);
                std::process::exit(1);
            }
        }
        UiMode::Cli => {
            run_cli(task, &task_args, &registry).await;
        }
        UiMode::Agent => {
            run_agent(task, &task_args, &registry, &args.format).await;
        }
    }
}

/// Run a task in CLI mode: direct execution with stdio output, no TUI.
///
/// Sets up a tracing subscriber for task log events (info!, warn!, etc.)
/// and forwards process output (from ctx.exec()) to stdout/stderr in real time.
async fn run_cli(
    task: &'static crate::task::TaskDef,
    args: &[String],
    registry: &Arc<Registry>,
) {
    // Install a simple tracing subscriber for task log events → stderr
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();

    let mut ctx = TaskContext::new(task.name);
    ctx.set_registry(registry.clone());

    // Subscribe to the exec output buffer BEFORE running the task so we
    // don't miss early output. Forward entries to stdout/stderr in real time.
    let rx = {
        let buf = ctx.output_buffer().lock().await;
        buf.subscribe()
    };
    let forwarder = tokio::spawn(forward_output_to_stdio(rx));

    let result = task.func.call(&ctx, args).await;

    // Give the forwarder a moment to drain, then drop it
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    forwarder.abort();

    match result {
        Ok(()) => {}
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(e.exit_code());
        }
    }
}

/// Forward log entries from a broadcast receiver to stdout/stderr.
async fn forward_output_to_stdio(mut rx: tokio::sync::broadcast::Receiver<LogEntry>) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    while let Ok(entry) = rx.recv().await {
        let line = &entry.raw;
        match entry.stream {
            Some(Stream::Stderr) => {
                let mut out = stderr.lock();
                let _ = writeln!(out, "{}", line);
            }
            _ => {
                let mut out = stdout.lock();
                let _ = writeln!(out, "{}", line);
            }
        }
    }
}

/// Run a task in Agent mode: structured output, minimal UI.
async fn run_agent(
    task: &'static crate::task::TaskDef,
    args: &[String],
    registry: &Arc<Registry>,
    format: &OutputFormat,
) {
    let mut ctx = TaskContext::new(task.name);
    ctx.set_registry(registry.clone());

    match task.func.call(&ctx, args).await {
        Ok(()) => {
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::json!({"status": "ok", "task": task.name}));
                }
                OutputFormat::Text => {
                    // Silent success in text mode
                }
            }
        }
        Err(e) => {
            match format {
                OutputFormat::Json => {
                    let output = serde_json::json!({
                        "status": "error",
                        "task": task.name,
                        "error": e.output(),
                    });
                    println!("{}", output);
                }
                OutputFormat::Text => {
                    eprintln!("Error: {}", e);
                }
            }
            std::process::exit(e.exit_code());
        }
    }
}

/// Resolve the effective UI mode from explicit flag, task hint, and terminal state.
///
/// Priority: explicit `--ui` flag > task's `ui_hint` > terminal detection.
pub fn resolve_ui_mode(
    explicit: Option<UiMode>,
    task_hint: Option<crate::task::UiHint>,
    has_terminal: bool,
) -> UiMode {
    use crate::task::UiHint;

    if let Some(mode) = explicit {
        // User explicitly chose — respect it, but fall back to CLI if no terminal
        return match mode {
            UiMode::Tui if !has_terminal => UiMode::Cli,
            other => other,
        };
    }

    // No explicit flag — check task hint
    if let Some(hint) = task_hint {
        return match hint {
            UiHint::Cli => UiMode::Cli,
            UiHint::Tui if !has_terminal => UiMode::Cli,
            UiHint::Tui => UiMode::Tui,
        };
    }

    // Default: TUI if terminal available, CLI otherwise
    if has_terminal { UiMode::Tui } else { UiMode::Cli }
}
