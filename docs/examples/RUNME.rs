#!/usr/bin/env runme

use runme::prelude::*;

#[runme::init]
fn setup(ctx: &mut InitContext) {
    ctx.set_group_name("test");
}

/// Spawn a long-running process that ticks every second
#[runme::task]
async fn ticker(ctx: &TaskContext) -> TaskResult {
    info!("starting ticker");
    ctx.spawn("bash -c 'i=0; while true; do echo \"tick $i\"; i=$((i+1)); sleep 1; done'").await?;
    Ok(())
}

/// Spawn multiple processes with interleaved output
#[runme::task]
async fn multi(ctx: &TaskContext) -> TaskResult {
    info!("spawning 3 processes");
    ctx.spawn(Cmd::from("bash -c 'while true; do echo \"[fast] $(date +%T)\"; sleep 0.5; done'").label("fast")).await?;
    ctx.spawn(Cmd::from("bash -c 'while true; do echo \"[slow] $(date +%T)\"; sleep 2; done'").label("slow")).await?;
    ctx.spawn(Cmd::from("bash -c 'for i in $(seq 1 5); do echo \"[finite] step $i\"; sleep 1; done; echo done'").label("finite")).await?;

    Ok(())
}

/// Emit structured JSON logs
#[runme::task]
async fn json_logs(ctx: &TaskContext) -> TaskResult {
    ctx.spawn(r#"bash -c 'while true; do echo "{\"level\":\"info\",\"msg\":\"heartbeat\",\"service\":\"api\",\"latency_ms\":$((RANDOM % 500))}"; sleep 1; done'"#).await?;
    Ok(())
}

/// Task that fails
#[runme::task]
async fn fail(ctx: &TaskContext) -> TaskResult {
    info!("about to fail");
    ctx.exec("bash -c 'echo \"something went wrong\" >&2; exit 1'").await?;
    Ok(())
}

/// Produce a burst of output then exit
#[runme::task]
async fn burst(ctx: &TaskContext) -> TaskResult {
    ctx.exec("bash -c 'for i in $(seq 1 500); do echo \"line $i: $(head -c 80 /dev/urandom | base64)\"; done'").await?;
    info!("burst complete");
    Ok(())
}

/// Transient task: closes TUI on success, outputs task logs to stderr
#[runme::task]
async fn transient(ctx: &TaskContext) -> TaskResult {
    ctx.tui_wait(false);
    ctx.tui_output().stderr().subscribe(&ctx.task_output()).await;

    info!("starting work");
    ctx.exec("sleep 1").await?.ok()?;
    info!("done!");
    Ok(())
}

/// Build and rebuild on source changes
#[runme::task]
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
#[runme::task]
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
#[runme::task]
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

/// Custom watch channel: poll an external condition
#[runme::task]
async fn watch_channel_demo(ctx: &TaskContext) -> TaskResult {
    let (tx, w) = ctx.watch_channel::<String>();
    let mut w = w.label("health check");

    // Background poller
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let _ = tx.send("checked".to_string());
        }
    });

    loop {
        let status = w.next().await;
        info!("Health check: {}", status);
    }
}

/// Use Cmd builder with env and cwd
#[runme::task]
async fn env_demo(ctx: &TaskContext) -> TaskResult {
    let cmd = Cmd::new("env")
        .env("RUNME_DEMO", "hello")
        .env("RUNME_MODE", "testing");
    ctx.exec(cmd).await?;
    info!("env printed");
    Ok(())
}

/// Exercise tracing at multiple levels
#[runme::task]
fn log_levels(_ctx: &TaskContext) -> TaskResult {
    trace!("this is trace");
    debug!("this is debug");
    info!("this is info");
    warn!("this is a warning");
    error!("this is an error");
    Ok(())
}

/// Run burst then log_levels sequentially
#[runme::task]
async fn sequential(ctx: &TaskContext) -> TaskResult {
    info!("running burst");
    burst(ctx).await?;
    info!("running log_levels");
    log_levels(ctx)?;
    info!("sequential complete");
    Ok(())
}

/// Run ticker and json_logs concurrently
#[runme::task]
async fn concurrent(ctx: &TaskContext) -> TaskResult {
    info!("launching concurrent tasks");
    let (a, b) = tokio::join!(ticker(ctx), json_logs(ctx));
    a?;
    b?;
    info!("concurrent setup complete");
    Ok(())
}
