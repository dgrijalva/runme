//! CLI argument model and dispatch logic.
//!
//! The two-stage model: `RnmeArgs` captures global flags (`--tui`, `--cli`,
//! `--format`, `--timeout`, `--filter`) and a trailing `rest` vector. The first
//! element of `rest` is the task name; everything after it is forwarded to the
//! task's own argument parser.
//!
//! `cli::run()` is the single entry point called by the generated runner binary.
//! All dispatch logic lives here rather than in generated code.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::log::{LogEntry, Stream};
use crate::task::Registry;
use crate::tui::App;
use clap::Parser;

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
///
/// UI mode is selected by bare flags (`--tui`, `--cli`). At most one may be
/// passed; if none is set, the mode is resolved from the task hint and terminal
/// state. (`--mcp` is reserved for the upcoming MCP server mode.)
#[derive(Parser)]
#[command(name = "runme")]
pub struct RnmeArgs {
    /// Force interactive TUI mode.
    #[arg(long, conflicts_with_all = ["cli"])]
    pub tui: bool,

    /// Force direct CLI execution with stdio output.
    #[arg(long, conflicts_with_all = ["tui"])]
    pub cli: bool,

    /// Output format (for cli mode)
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

impl RnmeArgs {
    /// Translate the bare UI-mode flags into an explicit `UiMode` selection.
    ///
    /// Clap's `conflicts_with_all` already rejects multiple flags being set at
    /// parse time, so we just need to map a single set bit to a mode.
    pub fn explicit_ui_mode(&self) -> Option<UiMode> {
        match (self.tui, self.cli) {
            (true, false) => Some(UiMode::Tui),
            (false, true) => Some(UiMode::Cli),
            _ => None,
        }
    }
}

/// Main dispatch function called by the generated runner binary.
///
/// Parses CLI arguments, resolves the task, and dispatches to the appropriate
/// UI mode (TUI, CLI, or Agent).
pub async fn run(registry: Arc<Registry>, group_names: HashMap<String, String>) {
    let args = RnmeArgs::parse();
    let has_terminal = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let explicit_ui = args.explicit_ui_mode();

    if args.rest.is_empty() {
        // No task specified — resolve UI mode without a task hint
        let ui = resolve_ui_mode(explicit_ui.clone(), None, has_terminal);
        match ui {
            UiMode::Tui => {
                let tasks = registry.list().to_vec();
                let (engine, handle) = crate::execution::Engine::start(registry.clone());
                let mut app =
                    App::with_picker(tasks, group_names, registry.clone(), handle.clone());
                let result = app.run().await;
                let _ = handle.quit().await;
                engine.shutdown().await;
                if let Err(e) = result {
                    eprintln!("TUI error: {}", e);
                    std::process::exit(1);
                }
            }
            _ => {
                eprintln!("No task specified. Use --tui for the interactive picker.");
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
        resolve_ui_mode(explicit_ui, task.ui_hint, has_terminal)
    };

    // Parse `--timeout`. Accepts humantime strings ("30s", "5m", "1h30m")
    // and falls back to bare seconds (e.g., "30").
    let timeout = match args.timeout.as_deref() {
        None => None,
        Some(s) => match parse_timeout(s) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("Error: invalid --timeout value '{s}': {e}");
                std::process::exit(2);
            }
        },
    };

    match ui {
        UiMode::Tui => {
            let tasks = registry.list().to_vec();
            let (engine, handle) = crate::execution::Engine::start(registry.clone());
            let mut app = App::with_task(
                task,
                task_args,
                tasks,
                group_names,
                registry.clone(),
                handle.clone(),
            )
            .await;
            let result = app.run().await;
            let _ = handle.quit().await;
            engine.shutdown().await;
            if let Err(e) = result {
                eprintln!("TUI error: {}", e);
                std::process::exit(1);
            }
        }
        UiMode::Cli => {
            run_cli(task, &task_args, &registry, &args.format, timeout).await;
        }
        UiMode::Agent => {
            run_agent(task, &task_args, &registry, &args.format, timeout).await;
        }
    }
}

/// Parse a `--timeout` value. Tries `humantime::parse_duration` first, then
/// falls back to `s.parse::<u64>()` interpreted as whole seconds.
fn parse_timeout(s: &str) -> Result<Duration, String> {
    if let Ok(d) = humantime::parse_duration(s) {
        return Ok(d);
    }
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    Err("expected a humantime duration like '30s' or '5m', or a bare number of seconds".to_string())
}

/// Run a task in CLI mode: direct execution with stdio output, no TUI.
///
/// Slice 4: routes through `Engine` so the task graph, cancel ladder,
/// and per-task timeouts all work end-to-end. The CLI subscribes to
/// the engine's `LogStore` and forwards entries to stdio; a Ctrl-C
/// handler calls `engine_handle.quit()` for clean teardown.
async fn run_cli(
    task: &'static crate::task::TaskDef,
    args: &[String],
    registry: &Arc<Registry>,
    format: &OutputFormat,
    timeout: Option<Duration>,
) {
    use crate::execution::{Engine, TaskStatus};

    let (engine, handle) = Engine::start(registry.clone());

    // Subscribe to LogStore → stdio BEFORE launching the task.
    let rx = handle.subscribe_logs();
    let use_raw = matches!(format, OutputFormat::Raw);
    let use_color = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let graph_rx = handle.graph.clone();
    tokio::spawn(forward_output_to_stdio(rx, use_raw, use_color, graph_rx));

    // Spawn the task through the engine.
    let mut builder = handle.spawn_task(task, args.to_vec());
    if let Some(d) = timeout {
        builder = builder.timeout(d);
    }
    let task_id = match builder.await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    // Watch graph snapshots for terminal status on `task_id`. Ctrl-C
    // quits the engine cleanly (cancel subtree → root body returns).
    //
    // Two-phase wait, matching legacy CLI behavior:
    //   1. Wait for the task body to reach a terminal status.
    //   2. If Done/Failed and the task still has running processes,
    //      keep waiting until they exit (or Ctrl-C).
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut graph = handle.graph.clone();
    let final_status: TaskStatus = loop {
        let snap = graph.borrow().clone();
        if let Some(node) = snap.tasks.get(&task_id) {
            let body_done = matches!(
                &node.status,
                TaskStatus::Done
                    | TaskStatus::Failed(_)
                    | TaskStatus::Cancelled
                    | TaskStatus::Timeout
            );
            if body_done {
                let any_running = node
                    .processes
                    .iter()
                    .any(|p| matches!(p.status, crate::execution::ProcessStatus::Running));
                if !any_running {
                    break node.status.clone();
                }
            }
        }
        tokio::select! {
            res = graph.changed() => {
                if res.is_err() { break TaskStatus::Done; }
            }
            _ = &mut ctrl_c => {
                let _ = handle.quit().await;
                engine.shutdown().await;
                std::process::exit(130);
            }
        }
    };

    // Task reached terminal status. Drain & shut down.
    let _ = handle.quit().await;
    engine.shutdown().await;

    match final_status {
        TaskStatus::Done => {}
        TaskStatus::Failed(failure) => {
            eprintln!("Error: {}", failure.message);
            std::process::exit(failure.exit_code);
        }
        TaskStatus::Cancelled => std::process::exit(130),
        TaskStatus::Timeout => {
            eprintln!("Error: task timed out");
            std::process::exit(124);
        }
        _ => {}
    }
}

/// Forward log entries from a broadcast receiver to stdout/stderr.
///
/// Color handling by mode:
/// - Text (`raw=false`): colored columns if `color=true`, plain if not.
/// - Raw (`raw=true`): ANSI passthrough if `color=true`, stripped if not.
///
/// `graph_rx` is read on every entry to resolve `TaskId -> [N] label` for
/// the source column. The graph is updated in-place by the engine; this
/// only borrows the latest snapshot.
async fn forward_output_to_stdio(
    mut rx: tokio::sync::broadcast::Receiver<LogEntry>,
    raw: bool,
    color: bool,
    graph_rx: tokio::sync::watch::Receiver<crate::execution::GraphSnapshot>,
) {
    use crate::log::format::{format_entry, format_entry_colored};
    use crate::theme::SourceColors;
    use std::io::Write;
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut source_colors = SourceColors::new();
    while let Ok(entry) = rx.recv().await {
        // Build labels from the latest graph snapshot. Cheap — the
        // snapshot Arc-clones the task table and we just walk it once.
        let labels = graph_rx.borrow().source_labels();
        let formatted;
        let line: &str = if raw {
            if color {
                &entry.raw
            } else {
                formatted = crate::ansi::strip(&entry.raw);
                &formatted
            }
        } else if color {
            formatted = format_entry_colored(&entry, &mut source_colors, &labels);
            &formatted
        } else {
            formatted = format_entry(&entry, &labels);
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
/// Slice 4: routes through `Engine` like `run_cli` but skips stdio
/// forwarding. Only the final result is reported.
async fn run_agent(
    task: &'static crate::task::TaskDef,
    args: &[String],
    registry: &Arc<Registry>,
    format: &OutputFormat,
    timeout: Option<Duration>,
) {
    use crate::execution::{Engine, TaskStatus};

    let (engine, handle) = Engine::start(registry.clone());

    let mut builder = handle.spawn_task(task, args.to_vec());
    if let Some(d) = timeout {
        builder = builder.timeout(d);
    }
    let task_id = match builder.await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut graph = handle.graph.clone();
    let status: TaskStatus = loop {
        let snap = graph.borrow().clone();
        if let Some(node) = snap.tasks.get(&task_id) {
            match &node.status {
                TaskStatus::Done
                | TaskStatus::Failed(_)
                | TaskStatus::Cancelled
                | TaskStatus::Timeout => break node.status.clone(),
                _ => {}
            }
        }
        tokio::select! {
            res = graph.changed() => {
                if res.is_err() { break TaskStatus::Done; }
            }
            _ = &mut ctrl_c => {
                let _ = handle.quit().await;
                engine.shutdown().await;
                std::process::exit(130);
            }
        }
    };

    let _ = handle.quit().await;
    engine.shutdown().await;

    match status {
        TaskStatus::Done => match format {
            OutputFormat::Json => {
                println!("{}", serde_json::json!({"status": "ok", "task": task.name}));
            }
            OutputFormat::Text | OutputFormat::Raw => {}
        },
        TaskStatus::Failed(failure) => {
            match format {
                OutputFormat::Json => {
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
        TaskStatus::Cancelled => std::process::exit(130),
        TaskStatus::Timeout => {
            match format {
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "status": "timeout",
                            "task": task.name,
                        })
                    );
                }
                OutputFormat::Text | OutputFormat::Raw => {
                    eprintln!("Error: task timed out");
                }
            }
            std::process::exit(124);
        }
        _ => {}
    }
}

/// Resolve the effective UI mode from explicit flag, task hint, and terminal state.
///
/// Priority: explicit `--tui`/`--cli` flag > task's `ui_hint` > terminal detection.
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
    if has_terminal {
        UiMode::Tui
    } else {
        UiMode::Cli
    }
}
