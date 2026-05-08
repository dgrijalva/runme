//! rmcp tool surface for the MCP supervisor.
//!
//! Wraps a [`Supervisor`] in a `Clone`-able rmcp service object and exposes
//! every tool from `docs/mcp_design.md` § "Tool surface" via the
//! `#[tool_router]` / `#[tool]` macro pair.
//!
//! # Concurrency
//!
//! `Supervisor::spawn_task` takes `&mut self` because it mutates the
//! generation map. rmcp invokes tool handlers concurrently from
//! `Arc<Self>`, so we wrap the supervisor in a `tokio::sync::Mutex` and
//! lock it for the duration of each call. v1 simplicity over fine-grained
//! locking — every tool call is short-lived; the long-running waits in
//! `run_task` happen *outside* the mutex (see `run_task` implementation).
//!
//! # Output shape
//!
//! - Tools that return a small structured payload use
//!   `CallToolResult::structured(serde_json::Value)` so MCP clients see
//!   structured-content output.
//! - Tools that return a rendered human-readable report (`run_task`,
//!   `get_task`) return `Result<String, McpError>`; rmcp wraps the string
//!   as text content automatically.
//! - Acks (`kill_*`) return a tiny `{ "ok": true }` structured payload so
//!   the agent has *something* to read back.

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ErrorCode, ErrorData as McpError, Implementation, ServerCapabilities,
    ServerInfo,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::execution::{KillSignal, SpawnOptions, TaskId};
use crate::mcp::routing::Address;
use crate::mcp::supervisor::Supervisor;
use crate::mcp::wire::{GrepScope, Request, Response, RpcError};

// ---------------------------------------------------------------------------
// Server holder
// ---------------------------------------------------------------------------

/// rmcp service object. Wraps the supervisor in an `Arc<Mutex<>>` so tool
/// handlers can take `&self` while still mutating supervisor state.
#[derive(Clone)]
pub struct McpServer {
    supervisor: Arc<Mutex<Supervisor>>,
}

impl McpServer {
    /// Build a service from an existing supervisor.
    pub fn new(supervisor: Supervisor) -> Self {
        Self {
            supervisor: Arc::new(Mutex::new(supervisor)),
        }
    }

    /// Direct accessor for tests + the run loop that needs to drive
    /// rebuild signals on the same supervisor.
    pub fn supervisor(&self) -> Arc<Mutex<Supervisor>> {
        Arc::clone(&self.supervisor)
    }
}

// ---------------------------------------------------------------------------
// Parameter and output types
// ---------------------------------------------------------------------------

/// Parameters for [`McpServer::spawn_task`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SpawnTaskParams {
    /// Task name (e.g. `web` or `:list`).
    pub name: String,
    /// Arguments forwarded to the task.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional spawn-side timeout in seconds.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// Parameters for [`McpServer::run_task`]. Same shape as `spawn_task`
/// plus `tail_n` for the report tail.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunTaskParams {
    /// Task name (e.g. `web` or `:list`).
    pub name: String,
    /// Arguments forwarded to the task.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional spawn-side timeout in seconds.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Number of trailing log lines to include in the rendered report.
    /// Defaults to 50.
    #[serde(default)]
    pub tail_n: Option<usize>,
}

/// Parameters for [`McpServer::kill_task`] / [`McpServer::kill_process`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KillParams {
    /// Dotted task or process address (e.g. `42` or `42.7`).
    pub id: String,
    /// Signal: `term` (default, soft kill) or `kill` (immediate SIGKILL).
    #[serde(default)]
    pub signal: Option<String>,
}

/// Parameters for [`McpServer::get_task`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetTaskParams {
    /// Dotted task address.
    pub id: String,
    /// Number of trailing log lines to include in the report. Defaults to 50.
    #[serde(default)]
    pub tail_n: Option<usize>,
}

/// Parameters for [`McpServer::get_logs`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetLogsParams {
    /// Dotted task address whose subtree the logs are drawn from.
    pub task_id: String,
    /// Lower-bound seq cursor (exclusive). Use `next_seq` from a prior call.
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Upper-bound seq cursor (inclusive).
    #[serde(default)]
    pub until_seq: Option<u64>,
    /// Maximum number of entries to return.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional filter expression (see `src/log/filter.rs`).
    #[serde(default)]
    pub filter: Option<String>,
}

/// Parameters for [`McpServer::grep_logs`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GrepLogsParams {
    /// Dotted task address whose subtree the search runs over.
    pub task_id: String,
    /// Substring or regex pattern.
    pub pattern: String,
    /// Maximum number of matches to return.
    #[serde(default)]
    pub limit: Option<u32>,
    /// `descendants` (default) or `self_only`.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Parameters for [`McpServer::install_skills`] (Phase 7 stub).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InstallSkillsParams {
    /// Target directory under which a `rnme/` namespace is created.
    pub target_dir: String,
}

// ---------------------------------------------------------------------------
// Tool router impl
// ---------------------------------------------------------------------------

#[tool_router]
impl McpServer {
    /// List every registered task across the latest engine generation.
    /// Returns task name, group, description, and rendered argument help.
    #[tool]
    pub async fn list_tasks(&self) -> Result<CallToolResult, McpError> {
        let sup = self.supervisor.lock().await;
        let resp = sup
            .list_tasks()
            .await
            .map_err(map_rpc_error)?;
        let value = serde_json::to_value(&resp)
            .map_err(|e| internal(format!("serialize ListTasks: {e}")))?;
        Ok(CallToolResult::structured(value))
    }

    /// Spawn a top-level task on the latest engine generation. Returns the
    /// dotted address and the engine's `initial_seq` cursor (use as the
    /// `since_seq` argument on a follow-up `get_logs` call to avoid the
    /// spawn-then-subscribe race).
    #[tool]
    pub async fn spawn_task(
        &self,
        Parameters(params): Parameters<SpawnTaskParams>,
    ) -> Result<CallToolResult, McpError> {
        let opts = SpawnOptions {
            timeout: params.timeout_seconds.map(Duration::from_secs),
        };
        let mut sup = self.supervisor.lock().await;
        sup.check_can_spawn().await.map_err(map_rpc_error)?;
        let (task_id, initial_seq) = sup
            .spawn_task(params.name, params.args, opts)
            .await
            .map_err(map_rpc_error)?;
        let value = serde_json::json!({
            "task_id": task_id,
            "initial_seq": initial_seq,
        });
        Ok(CallToolResult::structured(value))
    }

    /// Compound: spawn a task, wait for it to reach a terminal state, and
    /// return the rendered task report (see `docs/mcp_design.md`
    /// § "Task report").
    #[tool]
    pub async fn run_task(
        &self,
        Parameters(params): Parameters<RunTaskParams>,
    ) -> Result<String, McpError> {
        let opts = SpawnOptions {
            timeout: params.timeout_seconds.map(Duration::from_secs),
        };
        // Spawn under the lock, then drop the lock for the wait + render.
        let (dotted, _initial_seq) = {
            let mut sup = self.supervisor.lock().await;
            sup.check_can_spawn().await.map_err(map_rpc_error)?;
            sup.spawn_task(params.name, params.args, opts)
                .await
                .map_err(map_rpc_error)?
        };

        // Poll the supervisor's cached snapshot until the top-task reaches
        // a terminal status. Cheap at human-scale poll intervals (~100ms).
        let addr = parse_address(&dotted)?;
        let tail_n = params.tail_n.unwrap_or(crate::mcp::report::DEFAULT_TAIL_N);
        loop {
            let snapshot = self.supervisor.lock().await.graph().await;
            let Some(node) = snapshot
                .top_tasks
                .iter()
                .find(|t| t.id.parse::<Address>().map(|a| a.top == addr.top).unwrap_or(false))
            else {
                // Top-task hasn't shown up yet — graph snapshots arrive
                // asynchronously over Event::Graph.
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            if is_terminal(&node.status) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        render_task_report(&self.supervisor, &dotted, tail_n).await
    }

    /// Kill a task and its subtree.
    #[tool]
    pub async fn kill_task(
        &self,
        Parameters(params): Parameters<KillParams>,
    ) -> Result<CallToolResult, McpError> {
        let signal = parse_signal(params.signal.as_deref())?;
        let sup = self.supervisor.lock().await;
        let resp = sup
            .request_addr(&params.id, |a| Request::KillTask {
                task_id: TaskId(a.task),
                signal,
            })
            .await
            .map_err(map_rpc_error)?;
        match resp {
            Response::KillTask => Ok(ack()),
            other => Err(internal(format!("unexpected response to KillTask: {other:?}"))),
        }
    }

    /// Signal a single spawned process. Does not change task status.
    #[tool]
    pub async fn kill_process(
        &self,
        Parameters(params): Parameters<KillParams>,
    ) -> Result<CallToolResult, McpError> {
        let signal = parse_signal(params.signal.as_deref())?;
        let sup = self.supervisor.lock().await;
        let resp = sup
            .request_addr(&params.id, |a| Request::KillProcess {
                process_id: TaskId(a.task),
                signal,
            })
            .await
            .map_err(map_rpc_error)?;
        match resp {
            Response::KillProcess => Ok(ack()),
            other => Err(internal(format!(
                "unexpected response to KillProcess: {other:?}"
            ))),
        }
    }

    /// Cancel every direct child of the latest engine's root task.
    #[tool]
    pub async fn kill_all(&self) -> Result<CallToolResult, McpError> {
        let sup = self.supervisor.lock().await;
        sup.kill_all().await.map_err(map_rpc_error)?;
        Ok(ack())
    }

    /// Return the current merged task graph across every live generation,
    /// with engine-internal ids rewritten to dotted addresses.
    #[tool]
    pub async fn get_graph(&self) -> Result<CallToolResult, McpError> {
        let sup = self.supervisor.lock().await;
        let snap = sup.graph().await;
        let value = serde_json::to_value(&snap)
            .map_err(|e| internal(format!("serialize graph: {e}")))?;
        Ok(CallToolResult::structured(value))
    }

    /// Render the human-readable task report for an existing task. Works
    /// on running and completed tasks alike.
    #[tool]
    pub async fn get_task(
        &self,
        Parameters(params): Parameters<GetTaskParams>,
    ) -> Result<String, McpError> {
        let tail_n = params.tail_n.unwrap_or(crate::mcp::report::DEFAULT_TAIL_N);
        render_task_report(&self.supervisor, &params.id, tail_n).await
    }

    /// Cursor-paged log entries for the given task (and its descendants).
    /// Returns `{ entries, next_seq, has_more }`.
    #[tool]
    pub async fn get_logs(
        &self,
        Parameters(params): Parameters<GetLogsParams>,
    ) -> Result<CallToolResult, McpError> {
        let sup = self.supervisor.lock().await;
        let resp = sup
            .request_addr(&params.task_id, |a| Request::GetLogs {
                task_id: TaskId(a.task),
                since_seq: params.since_seq,
                until_seq: params.until_seq,
                limit: params.limit,
                filter: params.filter.clone(),
            })
            .await
            .map_err(map_rpc_error)?;
        match resp {
            Response::GetLogs {
                entries,
                next_seq,
                has_more,
            } => {
                let value = serde_json::json!({
                    "entries": entries,
                    "next_seq": next_seq,
                    "has_more": has_more,
                });
                Ok(CallToolResult::structured(value))
            }
            other => Err(internal(format!("unexpected response to GetLogs: {other:?}"))),
        }
    }

    /// Search log entries for a substring or regex match. `scope` is
    /// `descendants` (default) or `self_only`.
    #[tool]
    pub async fn grep_logs(
        &self,
        Parameters(params): Parameters<GrepLogsParams>,
    ) -> Result<CallToolResult, McpError> {
        let scope = parse_scope(params.scope.as_deref())?;
        let sup = self.supervisor.lock().await;
        let resp = sup
            .request_addr(&params.task_id, |a| Request::GrepLogs {
                task_id: TaskId(a.task),
                pattern: params.pattern.clone(),
                limit: params.limit,
                scope,
            })
            .await
            .map_err(map_rpc_error)?;
        match resp {
            Response::GrepLogs { matches } => {
                let value = serde_json::json!({ "matches": matches });
                Ok(CallToolResult::structured(value))
            }
            other => Err(internal(format!(
                "unexpected response to GrepLogs: {other:?}"
            ))),
        }
    }

    /// Snapshot of the supervisor's build state machine (idle / rebuilding
    /// / last_build_failed) plus the captured cargo output of the most
    /// recent failure, when present.
    #[tool]
    pub async fn get_build_status(&self) -> Result<CallToolResult, McpError> {
        let sup = self.supervisor.lock().await;
        let info = sup.build_status().await;
        let value = serde_json::json!({
            "state": info.state,
            "last_failure_output": info.last_failure_output,
        });
        Ok(CallToolResult::structured(value))
    }

    /// (Phase 7 stub.) Install agent-facing skill docs into a target dir.
    #[tool]
    pub async fn install_skills(
        &self,
        Parameters(_params): Parameters<InstallSkillsParams>,
    ) -> Result<CallToolResult, McpError> {
        Err(McpError::new(
            ErrorCode::INTERNAL_ERROR,
            "install_skills will be implemented in Phase 7",
            None,
        ))
    }
}

// ---------------------------------------------------------------------------
// ServerHandler — only override get_info() to set instructions / capabilities.
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rnme", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

const INSTRUCTIONS: &str = "rnme MCP server. Tools: list_tasks, spawn_task, run_task, kill_task, kill_process, kill_all, get_graph, get_task, get_logs, grep_logs, get_build_status. To install agent skills (RUNME.rs authoring + tool usage docs), call install_skills(target_dir) where target_dir is your framework's skill location (Claude Code: <project>/.claude/skills/).";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ack() -> CallToolResult {
    CallToolResult::structured(serde_json::json!({ "ok": true }))
}

fn internal(msg: String) -> McpError {
    McpError::new(ErrorCode::INTERNAL_ERROR, msg, None)
}

fn invalid_params(msg: String) -> McpError {
    McpError::new(ErrorCode::INVALID_PARAMS, msg, None)
}

fn map_rpc_error(e: RpcError) -> McpError {
    match e {
        RpcError::Engine(inner) => internal(format!("engine error: {inner}")),
        RpcError::BadRequest(s) => invalid_params(s),
        RpcError::FilterParse(s) => invalid_params(format!("filter parse: {s}")),
        RpcError::Internal(s) => internal(s),
        RpcError::NotFound(s) => invalid_params(format!("not found: {s}")),
    }
}

fn parse_address(s: &str) -> Result<Address, McpError> {
    s.parse::<Address>()
        .map_err(|e| invalid_params(format!("bad address: {e}")))
}

fn parse_signal(s: Option<&str>) -> Result<KillSignal, McpError> {
    match s {
        None => Ok(KillSignal::Term),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "term" | "sigterm" | "soft" | "" => Ok(KillSignal::Term),
            "kill" | "sigkill" | "hard" => Ok(KillSignal::Kill),
            other => Err(invalid_params(format!(
                "unknown signal {other:?}; expected `term` or `kill`"
            ))),
        },
    }
}

fn parse_scope(s: Option<&str>) -> Result<GrepScope, McpError> {
    match s {
        None => Ok(GrepScope::Descendants),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "descendants" | "" => Ok(GrepScope::Descendants),
            "self" | "self_only" | "selfonly" => Ok(GrepScope::SelfOnly),
            other => Err(invalid_params(format!(
                "unknown scope {other:?}; expected `descendants` or `self_only`"
            ))),
        },
    }
}

fn is_terminal(status: &crate::execution::execution::TaskStatus) -> bool {
    use crate::execution::execution::TaskStatus::*;
    matches!(status, Done | Failed(_) | Cancelled | Timeout)
}

/// Fetch the snapshot + tail entries for a dotted address and render the
/// human-readable task report.
async fn render_task_report(
    supervisor: &Arc<Mutex<Supervisor>>,
    address: &str,
    tail_n: usize,
) -> Result<String, McpError> {
    let addr = parse_address(address)?;
    let sup = supervisor.lock().await;

    // Fetch the engine's snapshot for this task's owning gen. Uses the
    // public route helper that resolves the gen and forwards a request.
    // We need the *raw* GraphSnapshot for `report::render`, not the
    // rewritten supervisor-level snapshot. Get it via Request::GetLogs +
    // a fresh graph snapshot probe; the supervisor's cached
    // per-gen snapshot is the right input.
    let snapshot = sup
        .latest_snapshot_for(&addr)
        .await
        .ok_or_else(|| invalid_params(format!("not found: {address}")))?;

    // Fetch tail entries via Request::GetLogs against the owning gen.
    let resp = sup
        .request_addr(address, |a| Request::GetLogs {
            task_id: TaskId(a.task),
            since_seq: None,
            until_seq: None,
            limit: Some(tail_n as u32),
            filter: None,
        })
        .await
        .map_err(map_rpc_error)?;

    let entries = match resp {
        Response::GetLogs { entries, .. } => entries,
        other => return Err(internal(format!(
            "unexpected response while building report: {other:?}"
        ))),
    };

    drop(sup);

    Ok(crate::mcp::report::render(
        &snapshot,
        TaskId(addr.task),
        &entries,
        tail_n,
    ))
}
