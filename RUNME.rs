use runme::prelude::*;

#[runme::task(desc = "Install runme CLI via cargo install")]
fn install(_ctx: &TaskContext) {
    let status = std::process::Command::new("cargo")
        .args(["install", "--path", "crates/runme-cli"])
        .status()
        .expect("failed to run cargo install");

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[runme::main]
fn main() {}
