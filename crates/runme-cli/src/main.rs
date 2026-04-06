mod compile;
mod discover;
mod frontmatter;

use std::path::PathBuf;

use compile::compile;
use discover::discover;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Determine the RUNME.rs file to run and the arguments to pass through.
    //
    // Two modes:
    //   1. Shebang invocation: `runme /path/to/RUNME.rs [args...]`
    //      The OS calls `runme` with the script path as the first argument.
    //   2. Discovery mode: `runme [args...]`
    //      Walk up from cwd to find the nearest RUNME.rs.
    let (runme_file, pass_through_args) = if args.len() > 1 && args[1].ends_with(".rs") {
        // Shebang mode
        let file = PathBuf::from(&args[1]);
        if !file.exists() {
            eprintln!("runme: file not found: {}", args[1]);
            std::process::exit(1);
        }
        (file, args[2..].to_vec())
    } else {
        // Discovery mode
        let cwd = std::env::current_dir().unwrap_or_else(|e| {
            eprintln!("runme: could not determine current directory: {}", e);
            std::process::exit(1);
        });

        let result = discover(&cwd);
        match result.nearest {
            Some(path) => (path, args[1..].to_vec()),
            None => {
                eprintln!("runme: no RUNME.rs found (searched from {})", cwd.display());
                std::process::exit(1);
            }
        }
    };

    // Compile (or use cached binary)
    let compiled = match compile(&runme_file) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("runme: compilation failed: {}", e);
            std::process::exit(1);
        }
    };

    // Exec the compiled binary, replacing this process.
    // Build the full argv including the binary path as argv[0].
    let mut argv: Vec<&str> = Vec::with_capacity(pass_through_args.len() + 1);
    let binary_str = compiled
        .binary_path
        .to_str()
        .expect("binary path should be valid UTF-8");
    argv.push(binary_str);
    for arg in &pass_through_args {
        argv.push(arg.as_str());
    }

    // Use exec crate for process replacement (replaces current process)
    let err = exec::execvp(&compiled.binary_path, &argv);
    eprintln!("runme: failed to exec {}: {}", compiled.binary_path.display(), err);
    std::process::exit(1);
}
