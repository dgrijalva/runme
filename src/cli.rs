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
}

pub use crate::output::OutputFormat;

/// Top-level CLI arguments for the generated runner binary.
///
/// Global flags are parsed first; the trailing `rest` vector carries the task
/// name and any task-specific arguments that are forwarded to the task's own
/// parser.
///
/// UI mode is selected by bare flags (`--tui`, `--cli`). At most one may be
/// passed; if none is set, the mode is resolved from the task hint and terminal
/// state. (`--mcp` is handled by the outer driver in `bin/rnme/main.rs` and
/// never reaches this parser.)
#[derive(Parser)]
#[command(name = "runme")]
pub struct RnmeArgs {
    /// Force interactive TUI mode.
    #[arg(long, conflicts_with_all = ["cli", "engine"])]
    pub tui: bool,

    /// Force direct CLI execution with stdio output.
    #[arg(long, conflicts_with_all = ["tui", "engine"])]
    pub cli: bool,

    /// Run as headless engine daemon (TCP JSONL on 127.0.0.1:0).
    /// Prints `{"port": <u16>}` on stdout and accepts a single
    /// supervisor connection.
    #[arg(long, conflicts_with_all = ["tui", "cli"])]
    pub engine: bool,

    /// Starting TaskId counter for `--engine` (defaults to 1).
    #[arg(long, requires = "engine")]
    pub start_task_id: Option<u64>,

    /// Output format (for cli mode)
    #[arg(long)]
    pub format: Option<OutputFormat>,

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
/// UI mode (TUI or CLI).
pub async fn run(registry: Arc<Registry>, group_names: HashMap<String, String>) {
    let args = RnmeArgs::parse();

    // `--engine` short-circuits everything else: spawn the headless
    // engine daemon and never return.
    if args.engine {
        let _ = group_names; // engine mode doesn't render group display names
        crate::mcp::engine_server::run(registry, args.start_task_id.unwrap_or(1)).await;
    }

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
            run_cli(task, &task_args, &registry, args.format, timeout).await;
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
    explicit_format: Option<OutputFormat>,
    timeout: Option<Duration>,
) {
    use crate::execution::{Engine, TaskStatus};

    let (engine, handle) = Engine::start(registry.clone());

    // Subscribe to LogStore → stdio BEFORE launching the task. Format is
    // resolved per-entry: explicit `--format` wins; otherwise the task's
    // `ctx.default_format()` hint (if any) wins; otherwise Text.
    let rx = handle.subscribe_logs();
    let use_color = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let graph_rx = handle.graph.clone();
    let format_hint = handle.format_hint();
    tokio::spawn(forward_output_to_stdio(
        rx,
        explicit_format,
        format_hint,
        use_color,
        graph_rx,
    ));

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
    // SIGHUP triggers a cooperative soft restart on the running task —
    // if the task subscribed via `ctx.restart_handle()`, the signal is
    // delivered and the task body decides how to react; otherwise the
    // engine transparently falls back to hard restart.
    //
    // Two-phase wait, matching legacy CLI behavior:
    //   1. Wait for the task body to reach a terminal status.
    //   2. If Done/Failed and the task still has running processes,
    //      keep waiting until they exit (or Ctrl-C).
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .expect("install SIGHUP handler");
    let mut graph = handle.graph.clone();
    let mut task_id = task_id;
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
            _ = sighup.recv() => {
                match handle
                    .restart(task_id, crate::execution::RestartMode::Soft)
                    .await
                {
                    Ok(new_id) => task_id = new_id,
                    Err(e) => eprintln!("rnme: SIGHUP restart failed: {e}"),
                }
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
/// Format is resolved per-entry: `explicit_format` (from `--format`)
/// always wins; otherwise the task's `ctx.default_format()` hint wins
/// if set; otherwise Text. This lets a task call `default_format(Raw)`
/// at the top of its body and have output formatted accordingly, even
/// for entries already in flight.
///
/// Color handling:
/// - Text: colored columns if `color=true`, plain if not.
/// - Raw: ANSI passthrough if `color=true`, stripped if not.
///
/// `graph_rx` is read on every entry to resolve `TaskId -> [N] label` for
/// the source column. The graph is updated in-place by the engine; this
/// only borrows the latest snapshot.
async fn forward_output_to_stdio(
    mut rx: tokio::sync::broadcast::Receiver<LogEntry>,
    explicit_format: Option<OutputFormat>,
    format_hint: Arc<std::sync::OnceLock<OutputFormat>>,
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
        let format = explicit_format
            .or_else(|| format_hint.get().copied())
            .unwrap_or(OutputFormat::Text);
        let raw = matches!(format, OutputFormat::Raw);
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
