//! Command values: a description of what to run.
//!
//! [`Cmd`] is a *value*. It carries a program (or shell string), arguments,
//! environment overlays, working directory, and an optional label — but it
//! does **not** describe runtime behavior like timeouts or readiness probes.
//! Those live on [`SpawnBuilder`](crate::process::SpawnBuilder), returned by
//! [`TaskContext::spawn`](crate::task::TaskContext::spawn).
//!
//! # Two flavors
//!
//! ```rust,ignore
//! // Structured — args go straight to the OS, no shell, no escaping bugs.
//! Cmd::new("cargo").args(["build", "--release"])
//!
//! // Shell — wrapped in `sh -c`, supports pipes, globs, redirects.
//! Cmd::shell("cargo build && cargo test")
//! ```
//!
//! `&str` and `String` convert to [`Cmd::shell`], so anywhere a `Cmd` is
//! expected you can pass a string literal:
//!
//! ```rust,ignore
//! ctx.exec("echo hello").await?;
//! ```
//!
//! For structured commands with interpolation, use the [`cmd!`](crate::cmd!) macro:
//!
//! ```rust,ignore
//! let url = "http://example.com";
//! ctx.exec(cmd!(curl -X POST {&url} -H "Content-Type: application/json")).await?;
//! ```

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::log::extract::FieldExtractor;
use crate::log::parse::RecordParser;

/// A command to execute.
///
/// Pure value describing what to run: program, arguments, environment overlays,
/// and working directory. Carries no runtime behavior (timeout, retry, etc.).
///
/// Two modes:
/// - **Structured**: `Cmd::new("cargo").args(["build"])` — args passed directly to the OS
/// - **Shell**: `Cmd::shell("cargo build && cargo test")` — wrapped in `sh -c`
///
/// `&str` converts to `Cmd::shell()` so `ctx.exec("echo hi")` still works.
///
/// Optionally carries a `RecordParser` and `FieldExtractor` for overriding
/// the default autodetection pipeline.
pub struct Cmd {
    inner: CmdKind,
    envs: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
    label: Option<String>,
    pub(crate) parser: Option<Box<dyn RecordParser>>,
    pub(crate) extractor: Option<Box<dyn FieldExtractor>>,
}

#[derive(Clone, Debug)]
enum CmdKind {
    Structured {
        program: OsString,
        args: Vec<OsString>,
    },
    Shell(String),
}

impl Cmd {
    /// Create a structured command (no shell involved).
    ///
    /// Arguments are passed directly to the OS — no shell escaping needed.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            inner: CmdKind::Structured {
                program: program.into(),
                args: Vec::new(),
            },
            envs: Vec::new(),
            cwd: None,
            label: None,
            parser: None,
            extractor: None,
        }
    }

    /// Create a shell command (wrapped in `sh -c`).
    ///
    /// Use this when you need pipes, globs, redirects, or other shell features.
    pub fn shell(command: impl Into<String>) -> Self {
        Self {
            inner: CmdKind::Shell(command.into()),
            envs: Vec::new(),
            cwd: None,
            label: None,
            parser: None,
            extractor: None,
        }
    }

    /// Override the default parser chain for this command's output.
    pub fn record_parser(mut self, parser: impl RecordParser + 'static) -> Self {
        self.parser = Some(Box::new(parser));
        self
    }

    /// Override the default field extractor for this command's output.
    pub fn field_extractor(mut self, extractor: impl FieldExtractor + 'static) -> Self {
        self.extractor = Some(Box::new(extractor));
        self
    }

    /// Add a single argument. Panics if called on a shell command.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        match &mut self.inner {
            CmdKind::Structured { args, .. } => args.push(arg.into()),
            CmdKind::Shell(_) => panic!("cannot add args to a shell command"),
        }
        self
    }

    /// Add multiple arguments. Panics if called on a shell command.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        match &mut self.inner {
            CmdKind::Structured { args: existing, .. } => {
                existing.extend(args.into_iter().map(|a| a.into()));
            }
            CmdKind::Shell(_) => panic!("cannot add args to a shell command"),
        }
        self
    }

    /// Set an environment variable overlay.
    ///
    /// The parent process environment is inherited; these are additions/overrides.
    pub fn env(mut self, key: impl Into<OsString>, val: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), val.into()));
        self
    }

    /// Set the working directory.
    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }

    /// Conditionally apply a builder transformation.
    ///
    /// Keeps the chain intact when you need to toggle settings based on a flag:
    /// ```ignore
    /// Cmd::shell("cargo run")
    ///     .env("PORT", "3000")
    ///     .when(cfg.dev_mode, |c| c.env("DEBUG", "1"))
    ///     .label("server")
    /// ```
    pub fn when(self, condition: bool, f: impl FnOnce(Self) -> Self) -> Self {
        if condition { f(self) } else { self }
    }

    /// Set a human-readable label for this command.
    ///
    /// The label is used as the source name in the log viewer and sidebar,
    /// replacing the auto-generated command string. Useful when the raw command
    /// is long or opaque (e.g., a shell one-liner).
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Get the label, falling back to the auto-generated display string.
    pub fn display_label(&self) -> String {
        self.label.clone().unwrap_or_else(|| self.to_string())
    }

    /// Whether this is a shell command.
    pub fn is_shell(&self) -> bool {
        matches!(self.inner, CmdKind::Shell(_))
    }

    /// Get the program name (structured) or shell string (shell).
    pub fn program(&self) -> &OsStr {
        match &self.inner {
            CmdKind::Structured { program, .. } => program,
            CmdKind::Shell(s) => OsStr::new(s.as_str()),
        }
    }

    /// Get the environment overlays.
    pub fn envs(&self) -> &[(OsString, OsString)] {
        &self.envs
    }

    /// Get the working directory, if set.
    pub fn get_cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// Build a `tokio::process::Command` from this Cmd.
    ///
    /// Sets up program, args, env overlays, and cwd. Does NOT set up
    /// stdio or process groups — that's the caller's responsibility.
    pub(crate) fn into_tokio_command(self) -> tokio::process::Command {
        let mut command = match self.inner {
            CmdKind::Shell(s) => {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c");
                c.arg(s);
                c
            }
            CmdKind::Structured { program, args } => {
                let mut c = tokio::process::Command::new(program);
                c.args(args);
                c
            }
        };

        for (key, val) in self.envs {
            command.env(key, val);
        }

        if let Some(cwd) = self.cwd {
            command.current_dir(cwd);
        }

        command
    }
}

impl fmt::Debug for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cmd")
            .field("inner", &self.inner)
            .field("envs", &self.envs)
            .field("cwd", &self.cwd)
            .field("label", &self.label)
            .field("parser", &self.parser.as_ref().map(|_| "..."))
            .field("extractor", &self.extractor.as_ref().map(|_| "..."))
            .finish()
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            CmdKind::Shell(s) => write!(f, "{}", s),
            CmdKind::Structured { program, args } => {
                write!(f, "{}", program.to_string_lossy())?;
                for arg in args {
                    write!(f, " {}", arg.to_string_lossy())?;
                }
                Ok(())
            }
        }
    }
}

/// `&str` → `Cmd::shell()` for convenience.
impl From<&str> for Cmd {
    fn from(s: &str) -> Self {
        Cmd::shell(s)
    }
}

/// `String` → `Cmd::shell()` for convenience.
impl From<String> for Cmd {
    fn from(s: String) -> Self {
        Cmd::shell(s)
    }
}

/// Convert from `std::process::Command`, extracting program, args, env, and cwd.
/// Parser and extractor are set to None (autodetect defaults).
impl From<std::process::Command> for Cmd {
    fn from(std_cmd: std::process::Command) -> Self {
        let program = std_cmd.get_program().to_owned();
        let args: Vec<OsString> = std_cmd.get_args().map(|a| a.to_owned()).collect();
        let envs: Vec<(OsString, OsString)> = std_cmd
            .get_envs()
            .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
            .collect();
        let cwd = std_cmd.get_current_dir().map(|p| p.to_owned());

        Cmd {
            inner: CmdKind::Structured { program, args },
            envs,
            cwd,
            label: None,
            parser: None,
            extractor: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_structured_command() {
        let cmd = Cmd::new("cargo").args(["build", "--release"]);
        assert!(!cmd.is_shell());
        assert_eq!(cmd.to_string(), "cargo build --release");
    }

    #[test]
    fn test_shell_command() {
        let cmd = Cmd::shell("cargo build && cargo test");
        assert!(cmd.is_shell());
        assert_eq!(cmd.to_string(), "cargo build && cargo test");
    }

    #[test]
    fn test_env_overlay() {
        let cmd = Cmd::new("cargo")
            .arg("build")
            .env("RUSTFLAGS", "-C target-cpu=native")
            .env("CARGO_INCREMENTAL", "0");
        assert_eq!(cmd.envs().len(), 2);
    }

    #[test]
    fn test_cwd() {
        let cmd = Cmd::new("ls").cwd("/tmp");
        assert_eq!(cmd.get_cwd(), Some(Path::new("/tmp")));
    }

    #[test]
    fn test_from_str() {
        let cmd: Cmd = "echo hello".into();
        assert!(cmd.is_shell());
        assert_eq!(cmd.to_string(), "echo hello");
    }

    #[test]
    fn test_from_string() {
        let cmd: Cmd = String::from("echo hello").into();
        assert!(cmd.is_shell());
    }

    #[test]
    fn test_from_std_command() {
        let mut std_cmd = std::process::Command::new("cargo");
        std_cmd.arg("build").env("CARGO_INCREMENTAL", "0");

        let cmd = Cmd::from(std_cmd);
        assert!(!cmd.is_shell());
        assert_eq!(cmd.to_string(), "cargo build");
        assert_eq!(cmd.envs().len(), 1);
    }

    #[test]
    fn test_from_std_command_with_cwd() {
        let mut std_cmd = std::process::Command::new("ls");
        std_cmd.current_dir("/tmp");

        let cmd = Cmd::from(std_cmd);
        assert_eq!(cmd.get_cwd(), Some(Path::new("/tmp")));
    }

    #[test]
    fn test_chaining_after_from_std() {
        let mut std_cmd = std::process::Command::new("cargo");
        std_cmd.arg("build");

        let cmd = Cmd::from(std_cmd).env("EXTRA", "yes").cwd("./subdir");
        assert_eq!(cmd.envs().len(), 1);
        assert_eq!(cmd.get_cwd(), Some(Path::new("./subdir")));
    }

    #[test]
    #[should_panic(expected = "cannot add args to a shell command")]
    fn test_shell_arg_panics() {
        Cmd::shell("echo").arg("hello");
    }

    #[test]
    #[should_panic(expected = "cannot add args to a shell command")]
    fn test_shell_args_panics() {
        Cmd::shell("echo").args(["hello"]);
    }

    #[test]
    fn test_display_structured_no_args() {
        let cmd = Cmd::new("ls");
        assert_eq!(cmd.to_string(), "ls");
    }

    #[test]
    fn test_record_parser_and_field_extractor() {
        use crate::log::extract::CommonJsonFieldExtractor;
        use crate::log::parse::PlainLineParser;

        let cmd = Cmd::new("my-app")
            .record_parser(PlainLineParser)
            .field_extractor(CommonJsonFieldExtractor::new());
        assert!(cmd.parser.is_some());
        assert!(cmd.extractor.is_some());
    }

    #[test]
    fn test_from_str_has_no_parser() {
        let cmd: Cmd = "echo hi".into();
        assert!(cmd.parser.is_none());
        assert!(cmd.extractor.is_none());
    }

    #[test]
    fn test_cmd_macro_simple() {
        let cmd = rnme::cmd!(cargo build --release);
        assert!(!cmd.is_shell());
        assert_eq!(cmd.to_string(), "cargo build --release");
    }

    #[test]
    fn test_cmd_macro_interpolation() {
        let url = "http://example.com";
        let cmd = rnme::cmd!(curl -X POST {url});
        assert_eq!(cmd.to_string(), "curl -X POST http://example.com");
    }

    #[test]
    fn test_cmd_macro_string_literal() {
        let cmd = rnme::cmd!(curl -H "Content-Type: application/json");
        assert_eq!(cmd.to_string(), "curl -H Content-Type: application/json");
    }

    #[test]
    fn test_cmd_macro_mixed() {
        let url = String::from("http://example.com");
        let path = "snapshot=@/tmp/file";
        let cmd = rnme::cmd! {
            curl -X POST {&url}
                 -H "Content-Type: multipart/form-data"
                 -F {path}
        };
        assert!(!cmd.is_shell());
        assert_eq!(
            cmd.to_string(),
            "curl -X POST http://example.com -H Content-Type: multipart/form-data -F snapshot=@/tmp/file"
        );
    }

    #[test]
    fn test_cmd_macro_ref_interpolation() {
        let name = String::from("world");
        let cmd = rnme::cmd!(echo {&name});
        assert_eq!(cmd.to_string(), "echo world");
    }
}
