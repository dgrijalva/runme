mod codegen;
mod compile;
mod crate_name;
mod discover;
mod frontmatter;
mod transform;

use compile::compile_workspace;
use discover::discover;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Discovery mode: walk up from cwd to find the nearest RUNME.rs
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("runme: could not determine current directory: {}", e);
        std::process::exit(1);
    });

    let discovery_result = discover(&cwd);
    if discovery_result.nearest.is_none() {
        eprintln!("runme: no RUNME.rs found (searched from {})", cwd.display());
        std::process::exit(1);
    }

    let pass_through_args = args[1..].to_vec();

    // Capture all RUNME.rs source paths for builtin tasks (fmt) before we move
    // discovery_result into compile_workspace.
    let runme_files: Vec<String> = std::iter::once(discovery_result.nearest.clone().unwrap())
        .chain(discovery_result.children.iter().cloned())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    // Compile the workspace
    let compiled = match compile_workspace(&discovery_result) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("runme: compilation failed: {}", e);
            std::process::exit(1);
        }
    };

    // Pass workspace metadata to the runner via env vars so builtin tasks
    // (fmt/check/clean) can find the source files and the generated workspace.
    // Safe: main.rs is single-threaded at this point.
    unsafe {
        std::env::set_var("RNME_CACHE_DIR", &compiled.cache_dir);
        std::env::set_var("RNME_RUNME_FILES", runme_files.join("\n"));
    }

    // Exec the compiled binary, replacing this process.
    let mut argv: Vec<&str> = Vec::with_capacity(pass_through_args.len() + 1);
    let binary_str = compiled
        .binary_path
        .to_str()
        .expect("binary path should be valid UTF-8");
    argv.push(binary_str);
    for arg in &pass_through_args {
        argv.push(arg.as_str());
    }

    let err = exec::execvp(&compiled.binary_path, &argv);
    eprintln!(
        "runme: failed to exec {}: {}",
        compiled.binary_path.display(),
        err
    );
    std::process::exit(1);
}
