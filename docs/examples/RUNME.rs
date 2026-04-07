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
