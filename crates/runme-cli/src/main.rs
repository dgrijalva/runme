mod compile;
mod crate_name;
mod discover;
mod frontmatter;
mod transform;

use std::path::PathBuf;

use compile::compile_workspace;
use discover::discover;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Determine discovery starting point and pass-through args.
    //
    // Two modes:
    //   1. Shebang invocation: `runme /path/to/RUNME.rs [args...]`
    //      The OS calls `runme` with the script path as the first argument.
    //      Discovery starts from the file's parent directory.
    //   2. Discovery mode: `runme [args...]`
    //      Walk up from cwd to find the nearest RUNME.rs.
    let (discovery_result, pass_through_args) = if args.len() > 1 && args[1].ends_with(".rs") {
        // Shebang mode: discover from the file's directory to find full tree
        let file = PathBuf::from(&args[1]);
        if !file.exists() {
            eprintln!("runme: file not found: {}", args[1]);
            std::process::exit(1);
        }
        let dir = file.parent().unwrap_or_else(|| {
            eprintln!("runme: could not determine parent directory of {}", args[1]);
            std::process::exit(1);
        });
        let result = discover(dir);
        if result.nearest.is_none() {
            eprintln!("runme: no RUNME.rs found (searched from {})", dir.display());
            std::process::exit(1);
        }
        (result, args[2..].to_vec())
    } else {
        // Discovery mode
        let cwd = std::env::current_dir().unwrap_or_else(|e| {
            eprintln!("runme: could not determine current directory: {}", e);
            std::process::exit(1);
        });

        let result = discover(&cwd);
        if result.nearest.is_none() {
            eprintln!("runme: no RUNME.rs found (searched from {})", cwd.display());
            std::process::exit(1);
        }
        (result, args[1..].to_vec())
    };

    // Compile the workspace
    let compiled = match compile_workspace(&discovery_result) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("runme: compilation failed: {}", e);
            std::process::exit(1);
        }
    };

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
