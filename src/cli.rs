//! CLI argument model and dispatch logic.
//!
//! The two-stage model: `RnmeArgs` captures global flags (`--ui`, `--format`,
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
use crate::task::Registry;
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
    /// Human-readable text (structured columns)
    Text,
    /// Structured JSON
    Json,
    /// Raw process output (unformatted, good for piping)
    Raw,
}

/// Top-level CLI arguments for the generated runner binary.
///
/// Global flags are parsed first; the trailing `rest` vector carries the task
/// name and any task-specific arguments that are forwarded to the task's own
/// parser.
#[derive(Parser)]
#[command(name = "runme")]
pub struct RnmeArgs {
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
    let args = RnmeArgs::parse();
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

    // Force CLI mode when task args request help — clap's help output
    // should go straight to the terminal, not into the TUI.
    let task_wants_help = task_args.iter().any(|a| a == "-h" || a == "--help");
    let ui = if task_wants_help {
        UiMode::Cli
    } else {
        resolve_ui_mode(args.ui, task.ui_hint, has_terminal)
    };

    match ui {
        UiMode::Tui => {
            let mut app = App::with_task(task, task_args, registry.clone());
            if let Err(e) = app.run().await {
                eprintln!("TUI error: {}", e);
                std::process::exit(1);
            }
        }
        UiMode::Cli => {
            run_cli(task, &task_args, &registry, &args.format).await;
        }
        UiMode::Agent => {
            run_agent(task, &task_args, &registry, &args.format).await;
        }
    }
}

/// Run a task in CLI mode: direct execution with stdio output, no TUI.
///
/// Uses the shared execution layer (`TaskExecution`). All output flows
/// through the LogStore; we subscribe and forward entries to stdio.
async fn run_cli(
    task: &'static crate::task::TaskDef,
    args: &[String],
    registry: &Arc<Registry>,
    format: &OutputFormat,
) {
    use crate::execution::{LaunchConfig, TaskExecution};

    let mut exec = TaskExecution::new();
    exec.set_registry(registry.clone());

    // Subscribe to LogStore → stdio BEFORE launching the task.
    let rx = exec.subscribe().await;
    let use_raw = matches!(format, OutputFormat::Raw);
    let use_color = std::io::IsTerminal::is_terminal(&std::io::stdout());
    tokio::spawn(forward_output_to_stdio(rx, use_raw, use_color));

    exec.launch(task, args.to_vec(), LaunchConfig::default());

    // Wait for the task function to complete, or Ctrl-C.
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    tokio::select! {
        _ = exec.wait() => {}
        _ = &mut ctrl_c => {
            exec.shutdown(std::time::Duration::from_secs(5)).await;
            std::process::exit(130); // 128 + SIGINT
        }
    }

    // Task function returned. Stay alive while spawned processes are
    // still running — same as TUI staying open. Ctrl-C triggers shutdown.
    loop {
        if !exec.has_running_processes().await {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            _ = &mut ctrl_c => {
                exec.shutdown(std::time::Duration::from_secs(5)).await;
                std::process::exit(130);
            }
        }
    }

    let status = exec.task_status().lock().await.clone();
    if let crate::execution::TaskStatus::Failed(failure) = status {
        eprintln!("Error: {}", failure.message);
        std::process::exit(failure.exit_code);
    }
}

/// Forward log entries from a broadcast receiver to stdout/stderr.
///
/// Color handling by mode:
/// - Text (`raw=false`): colored columns if `color=true`, plain if not.
/// - Raw (`raw=true`): ANSI passthrough if `color=true`, stripped if not.
async fn forward_output_to_stdio(
    mut rx: tokio::sync::broadcast::Receiver<LogEntry>,
    raw: bool,
    color: bool,
) {
    use crate::log::format::{format_entry, format_entry_colored};
    use crate::theme::SourceColors;
    use std::io::Write;
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut source_colors = SourceColors::new();
    while let Ok(entry) = rx.recv().await {
        let formatted;
        let line: &str = if raw {
            if color {
                &entry.raw
            } else {
                formatted = crate::ansi::strip(&entry.raw);
                &formatted
            }
        } else if color {
            formatted = format_entry_colored(&entry, &mut source_colors);
            &formatted
        } else {
            formatted = format_entry(&entry);
            &formatted
        };
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
///
/// Uses the shared execution layer. No output subscription — only
/// the final result is reported.
async fn run_agent(
    task: &'static crate::task::TaskDef,
    args: &[String],
    registry: &Arc<Registry>,
    format: &OutputFormat,
) {
    use crate::execution::{LaunchConfig, TaskExecution, TaskStatus};

    let mut exec = TaskExecution::new();
    exec.set_registry(registry.clone());

    exec.launch(task, args.to_vec(), LaunchConfig::default());
    exec.wait().await;
    exec.shutdown(std::time::Duration::from_secs(5)).await;

    let status = exec.task_status().lock().await.clone();
    match status {
        TaskStatus::Done => {
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::json!({"status": "ok", "task": task.name}));
                }
                OutputFormat::Text | OutputFormat::Raw => {}
            }
        }
        TaskStatus::Failed(failure) => {
            match format {
                OutputFormat::Json => {
                    // Parse the stored JSON output back to a Value for structured output.
                    let error_output: serde_json::Value =
                        serde_json::from_str(&failure.output_json)
                            .unwrap_or_else(|_| serde_json::json!({"message": failure.message}));
                    let output = serde_json::json!({
                        "status": "error",
                        "task": task.name,
                        "error": error_output,
                    });
                    println!("{}", output);
                }
                OutputFormat::Text | OutputFormat::Raw => {
                    eprintln!("Error: {}", failure.message);
                }
            }
            std::process::exit(failure.exit_code);
        }
        _ => {}
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
