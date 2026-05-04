//! Scaffolding for `rnme --init`: writes a starter `RUNME.rs` into the
//! current directory if one doesn't already exist there.
//!
//! `--init` is handled early in `main.rs`, before the discovery /
//! compilation pipeline, so it works in directories that don't yet
//! contain a `RUNME.rs`. A `RUNME.rs` in a parent directory is not
//! consulted — the user explicitly asked for one *here*.

use std::fs;
use std::path::PathBuf;

const RUNME_FILENAME: &str = "RUNME.rs";

const TEMPLATE: &str = r#"//! Tasks runnable through `rnme`.
//!
//! Tasks are plain async Rust functions annotated with `#[rnme::task]`.
//! Run `rnme` with no arguments to open the task picker, or
//! `rnme <name>` to run a specific task. See: [api reference]

use rnme::prelude::*;

/// Called at startup. Configure behaviour and register dynamically
/// generated tasks.
#[rnme::init]
fn setup(_ctx: &mut InitContext) {
    // Register dynamic tasks here. See: [api reference]
}

/// Example task — say hello.
#[rnme::task]
async fn hello(ctx: &TaskContext) -> TaskResult {
    info!("hello from RUNME.rs");
    ctx.exec("echo hello").await?;
    Ok(())
}
"#;

pub enum InitOutcome {
    Created(PathBuf),
    AlreadyExists(PathBuf),
}

/// Write a starter `RUNME.rs` into `dir` if one isn't already there.
pub fn run_init(dir: &std::path::Path) -> std::io::Result<InitOutcome> {
    let target = dir.join(RUNME_FILENAME);
    if target.exists() {
        return Ok(InitOutcome::AlreadyExists(target));
    }
    fs::write(&target, TEMPLATE)?;
    Ok(InitOutcome::Created(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_template_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let outcome = run_init(tmp.path()).unwrap();
        match outcome {
            InitOutcome::Created(p) => {
                let written = fs::read_to_string(&p).unwrap();
                assert!(written.contains("#[rnme::init]"));
                assert!(written.contains("#[rnme::task]"));
                assert!(written.contains("async fn hello"));
            }
            InitOutcome::AlreadyExists(_) => panic!("expected Created"),
        }
    }

    #[test]
    fn no_op_when_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(RUNME_FILENAME);
        fs::write(&target, "existing content").unwrap();

        let outcome = run_init(tmp.path()).unwrap();
        assert!(matches!(outcome, InitOutcome::AlreadyExists(_)));

        // The existing file is left untouched.
        let after = fs::read_to_string(&target).unwrap();
        assert_eq!(after, "existing content");
    }
}
