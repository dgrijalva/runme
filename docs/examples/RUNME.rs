use rnme::prelude::*;

#[rnme::init]
fn setup(ctx: &mut InitContext) {
    ctx.set_group_name("test");
}

/// Spawn a long-running process that ticks every second
#[rnme::task]
async fn ticker(ctx: &TaskContext) -> TaskResult {
    info!("starting ticker");
    let _h = ctx.spawn("bash -c 'i=0; while true; do echo \"tick $i\"; i=$((i+1)); sleep 1; done'").await?;
    // Process lives as long as `_h` is alive — i.e., until the task
    // body returns or is cancelled. Wait for cancellation so the
    // ticker keeps running until the user kills the task.
    ctx.cancellation_signal().await;
    Ok(())
}

/// Spawn multiple processes with interleaved output
#[rnme::task]
async fn multi(ctx: &TaskContext) -> TaskResult {
    info!("spawning 3 processes");
    let _fast = ctx.spawn(Cmd::from("bash -c 'while true; do echo \"[fast] $(date +%T)\"; sleep 0.5; done'").label("fast")).await?;
    let _slow = ctx.spawn(Cmd::from("bash -c 'while true; do echo \"[slow] $(date +%T)\"; sleep 2; done'").label("slow")).await?;
    let _finite = ctx.spawn(Cmd::from("bash -c 'for i in $(seq 1 5); do echo \"[finite] step $i\"; sleep 1; done; echo done'").label("finite")).await?;

    // Hold the handles alive until the task is cancelled.
    ctx.cancellation_signal().await;
    Ok(())
}

/// Emit structured JSON logs
#[rnme::task]
async fn json_logs(ctx: &TaskContext) -> TaskResult {
    let script = r#"
        while true; do
            printf '{"level":"info","msg":"heartbeat","service":"api","latency_ms":%d}\n' $((RANDOM % 500))
            sleep 1
        done
    "#;
    let _h = ctx.spawn(cmd!(bash -c {script})).await?;
    ctx.cancellation_signal().await;
    Ok(())
}

/// Task that fails
#[rnme::task]
async fn fail(ctx: &TaskContext) -> TaskResult {
    info!("about to fail");
    ctx.exec("bash -c 'echo \"something went wrong\" >&2; exit 1'").await?;
    Ok(())
}

/// Produce a burst of output then exit
#[rnme::task]
async fn burst(ctx: &TaskContext) -> TaskResult {
    ctx.exec("bash -c 'for i in $(seq 1 500); do echo \"line $i: $(head -c 80 /dev/urandom | base64)\"; done'").await?;
    info!("burst complete");
    Ok(())
}

/// Transient task: closes TUI on success, outputs task logs to stderr
///
/// NOTE: tui_wait / tui_output were removed in slice 2 of the multi-task
/// runtime (per design decision 7). This task is kept for the example
/// surface area; engine-driven completion semantics return in slices 3/4.
#[rnme::task]
async fn transient(ctx: &TaskContext) -> TaskResult {
    info!("starting work");
    ctx.exec("sleep 1").await?.ok()?;
    info!("done!");
    Ok(())
}

/// Build and rebuild on source changes
#[rnme::task]
async fn watch_build(ctx: &TaskContext) -> TaskResult {
    let mut w = ctx.watch("*.rs").label("rust sources");
    loop {
        info!("Building...");
        let result = ctx.exec("cargo build").await?;
        if result.success() {
            info!("Build succeeded");
        } else {
            error!("Build failed (exit {})", result.exit_code());
        }
        w.next().await;
    }
}

/// Watch with custom filter: separate handling for source vs config changes
#[rnme::task]
async fn watch_filtered(ctx: &TaskContext) -> TaskResult {
    let mut w = ctx.watch_with("../../crates/**/*", |changed| {
        let rs = glob_filter("**/*.rs", changed);
        let toml = glob_filter("**/Cargo.toml", changed);
        if rs.is_empty() && toml.is_empty() { None } else { Some((rs, toml)) }
    }).label("rust + manifests");

    loop {
        let (rs_files, toml_files) = w.next().await;
        if !toml_files.is_empty() {
            info!("Cargo.toml changed, updating deps");
            ctx.exec("cargo update").await?;
        }
        info!("{} file(s) changed, running tests", rs_files.len() + toml_files.len());
        ctx.exec("cargo test").await?.ok()?;
    }
}

/// Watch and restart: kill the server, rebuild, relaunch on file change
#[rnme::task]
async fn watch_restart(ctx: &TaskContext) -> TaskResult {
    let mut w = ctx.watch("src/**/*.rs").label("server sources");
    loop {
        info!("Starting server");
        let mut h = ctx.spawn("cargo run --example server").await?;
        w.next().await;
        info!("Source changed, restarting");
        h.stop(std::time::Duration::from_secs(5)).await?;
    }
}

/// Custom watch channel: poll an external condition.
///
/// The background poller uses `spawn!` (tracing-aware) and observes
/// `ctx.cancellation()` so it shuts down with the task instead of
/// leaking past task kill.
#[rnme::task]
async fn watch_channel_demo(ctx: &TaskContext) -> TaskResult {
    let (tx, w) = ctx.watch_channel::<String>();
    let mut w = w.label("health check");

    let cancel = ctx.cancellation();
    spawn!(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    let _ = tx.send("checked".to_string());
                }
            }
        }
    });

    loop {
        let status = w.next().await;
        info!("Health check: {}", status);
    }
}

/// Use Cmd builder with env and cwd
#[rnme::task]
async fn env_demo(ctx: &TaskContext) -> TaskResult {
    let cmd = cmd!(env)
        .env("RNME_DEMO", "hello")
        .env("RNME_MODE", "testing");
    ctx.exec(cmd).await?;
    info!("env printed");
    Ok(())
}

/// Exercise tracing at multiple levels
#[rnme::task]
fn log_levels(_ctx: &TaskContext) -> TaskResult {
    trace!("this is trace");
    debug!("this is debug");
    info!("this is info");
    warn!("this is a warning");
    error!("this is an error");
    Ok(())
}

/// Run burst then log_levels sequentially
#[rnme::task]
async fn sequential(ctx: &TaskContext) -> TaskResult {
    info!("running burst");
    burst(ctx).await?;
    info!("running log_levels");
    log_levels(ctx).await?;
    info!("sequential complete");
    Ok(())
}

/// Run ticker and json_logs concurrently
#[rnme::task]
async fn concurrent(ctx: &TaskContext) -> TaskResult {
    info!("launching concurrent tasks");
    let (a, b) = tokio::join!(ticker(ctx), json_logs(ctx));
    a?;
    b?;
    info!("concurrent setup complete");
    Ok(())
}

/// Spawn in-process background workers that participate in the task's
/// logging and cancellation.
///
/// Two things to notice:
///   1. `spawn!` (not bare `tokio::spawn`) re-enters the task's tracing
///      span inside the spawned future, so `info!` calls from each
///      worker are attributed to this task in the log viewer. A plain
///      `tokio::spawn` would drop the span and the events would
///      disappear.
///   2. Spawned futures are independent tokio tasks — they don't inherit
///      the body's cancellation. Clone `ctx.cancellation()` into each
///      worker and `select!` on `.cancelled()` so killing the task
///      shuts the workers down cleanly. Without this, workers leak past
///      task kill.
#[rnme::task]
async fn background_workers(ctx: &TaskContext) -> TaskResult {
    use std::time::Duration;

    info!("starting 3 background workers");
    let mut handles = Vec::new();
    for id in 0..3 {
        let cancel = ctx.cancellation();
        handles.push(spawn!(async move {
            let mut tick = 0u32;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("worker {id} stopping after {tick} ticks");
                        return;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        tick += 1;
                        info!("worker {id}: tick {tick}");
                    }
                }
            }
        }));
    }

    ctx.cancellation_signal().await;
    for h in handles {
        let _ = h.await;
    }
    info!("all workers shut down");
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 4: Task arguments, cross-file invocation, and steps
// ---------------------------------------------------------------------------

/// Deploy to an environment — demonstrates simple typed parameters.
///
/// Each parameter becomes a CLI flag:
///   rnme deploy --env staging --port 8080 --verbose
#[rnme::task]
async fn deploy(ctx: &TaskContext, env: String, port: u16, verbose: bool) -> TaskResult {
    info!("Deploying to {} on port {} (verbose: {})", env, port, verbose);
    if verbose {
        ctx.exec("echo 'Running pre-deploy checks...'").await?;
    }
    ctx.exec(cmd!(echo {format!("Deployed to {} on port {}", env, port)})).await?;
    Ok(())
}

/// Greet args — demonstrates a clap::Parser struct for full control.
///
/// `use rnme::clap;` brings clap into scope so derive macros resolve correctly.
use rnme::clap;

#[derive(clap::Parser)]
struct GreetArgs {
    /// Name to greet
    #[arg(long)]
    name: String,
    /// Number of times to greet
    #[arg(long, default_value = "1")]
    count: u32,
}

/// Greet someone — demonstrates clap::Parser struct for full CLI control.
///
/// Usage:
///   rnme greet --name world --count 3
#[rnme::task]
async fn greet(ctx: &TaskContext, args: GreetArgs) -> TaskResult {
    for i in 0..args.count {
        ctx.println(format!("[{}/{}] Hello, {}!", i + 1, args.count, args.name)).await;
    }
    Ok(())
}

/// Run all test tasks across groups — demonstrates cross-file invocation.
///
/// Uses `ctx.tasks()` to discover tasks matching a glob pattern, then
/// `ctx.run()` to invoke each one.
#[rnme::task]
async fn test_all(ctx: &TaskContext) -> TaskResult {
    if let Some(query) = ctx.tasks() {
        let test_tasks = query.matching("*:test");
        if test_tasks.is_empty() {
            info!("No test tasks found matching *:test");
        }
        for task in test_tasks {
            info!("Running {}", task.qualified_name);
            ctx.run(&task.qualified_name, &[]).await?;
        }
    } else {
        warn!("No registry available — running outside the full runtime?");
    }
    info!("test_all complete");
    Ok(())
}

/// Spawn a background server with a readiness probe and lifetime cap.
///
/// `ready_on_port` blocks the spawn until the server is actually listening,
/// `ready_timeout` bounds how long to wait for that probe, and `timeout`
/// hard-kills the process if it overstays its welcome.
#[rnme::task]
async fn server_ready(ctx: &TaskContext) -> TaskResult {
    use std::time::Duration;
    info!("starting server; waiting for port 8765");
    let _server = ctx.spawn("python3 -m http.server 8765")
        .ready_on_port(8765)
        .ready_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .await?;
    info!("server ready, hitting it");
    ctx.exec("curl -s http://127.0.0.1:8765/").await?.ok()?;
    Ok(())
}

/// Demonstrate cooperative soft restart.
///
/// Subscribe via `ctx.restart_handle()` and `select!` on the returned
/// handle alongside whatever else the task is doing. When the user
/// presses `r` in the TUI, calls the MCP `restart_task` tool with
/// `mode: "soft"`, or sends `SIGHUP` to a CLI-mode `rnme` process, the
/// handle fires and the task decides how to react. Here we treat it
/// like a config-reload — reset the uptime counter and keep going.
///
/// If the task hadn't called `restart_handle()`, the engine would
/// transparently fall back to a hard restart (kill + respawn).
///
/// Try it:
///   rnme soft_restart_demo            # then `kill -HUP <pid>` from another shell
///   rnme                              # press `r` (soft) or `R` (hard)
#[rnme::task]
async fn soft_restart_demo(ctx: &TaskContext) -> TaskResult {
    use std::time::Duration;
    let mut restart = ctx.restart_handle();
    let mut uptime = 0u32;
    info!("starting; uptime resets on soft restart");
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                uptime += 1;
                info!("uptime: {}s", uptime);
            }
            _ = restart.wait() => {
                info!("soft restart received — reloading (uptime was {}s)", uptime);
                uptime = 0;
            }
            _ = ctx.cancellation_signal() => break,
        }
    }
    Ok(())
}

/// Multi-phase pipeline — demonstrates `ctx.begin_step()` for labeled phases.
///
/// Each step is an RAII guard: when the guard is dropped the step is recorded
/// as complete (or failed, if `step.fail()` was called).
#[rnme::task]
async fn pipeline(ctx: &TaskContext) -> TaskResult {
    {
        let _step = ctx.begin_step("compile");
        info!("Compiling...");
        ctx.exec("echo 'compiling...'").await?;
    }
    {
        let _step = ctx.begin_step("test");
        info!("Testing...");
        ctx.exec("echo 'testing...'").await?;
    }
    {
        let _step = ctx.begin_step("package");
        info!("Packaging...");
        ctx.exec("echo 'packaging...'").await?;
    }
    info!("Pipeline complete");
    Ok(())
}
