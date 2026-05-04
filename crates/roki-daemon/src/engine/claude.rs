//! Typed `claude` CLI binary discovery and `tokio::process` spawn primitive.
//!
//! Two surfaces:
//! 1. [`ClaudeBinary::discover`] — config-override path > `PATH` search >
//!    actionable refusal. PATH search is implemented inline (no `which`
//!    crate dep) honoring the executable bit on Unix.
//! 2. [`ClaudeSpawn`] — builder over args/env/cwd/settings that produces a
//!    typed [`ClaudeProcess`] with piped stdin and line-oriented stdout/stderr
//!    readers wired up.
//!
//! Spec refs: requirements.md Req 1.3; design.md "engine adapters".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// Resolved location of the `claude` binary on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeBinary {
    path: PathBuf,
}

impl ClaudeBinary {
    /// Resolved binary path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolution order (first match wins):
    /// 1. Explicit override (validated as an executable file).
    /// 2. `PATH` search for `claude` (or `claude.exe` on Windows).
    /// 3. Hard refusal with an actionable remediation message.
    pub fn discover(config_override: Option<&Path>) -> Result<Self, ClaudeError> {
        if let Some(override_path) = config_override {
            return validate_executable(override_path).map(|path| Self { path });
        }
        if let Some(path) = search_path() {
            return Ok(Self { path });
        }
        Err(ClaudeError::NotFound)
    }

    /// Begin building a spawn invocation against this binary.
    pub fn spawn_builder(self) -> ClaudeSpawn {
        ClaudeSpawn {
            binary: self,
            settings: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }
}

/// Reasons binary discovery or spawn can fail.
#[derive(Debug, Error)]
pub enum ClaudeError {
    #[error(
        "configured `claude` binary path does not exist: {path}; set the \
         daemon's `claude_binary` config to a valid path or remove the \
         override and ensure `claude` is on PATH"
    )]
    OverrideMissing { path: PathBuf },

    #[error(
        "configured `claude` binary at {path} is not an executable file; \
         ensure the path points at the `claude` CLI and that it has the \
         executable bit set (chmod +x)"
    )]
    OverrideNotExecutable { path: PathBuf },

    #[error(
        "could not find `claude` on PATH and no `claude_binary` override is \
         configured; install Claude Code (https://docs.claude.com/claude-code) \
         or set `claude_binary` in the daemon config"
    )]
    NotFound,

    #[error("failed to spawn claude: {source}")]
    Spawn {
        #[source]
        source: std::io::Error,
    },

    #[error("claude spawned but {pipe} pipe was not captured")]
    MissingPipe { pipe: &'static str },
}

fn validate_executable(path: &Path) -> Result<PathBuf, ClaudeError> {
    if !path.exists() {
        return Err(ClaudeError::OverrideMissing {
            path: path.to_path_buf(),
        });
    }
    if !path.is_file() || !is_executable(path) {
        return Err(ClaudeError::OverrideNotExecutable {
            path: path.to_path_buf(),
        });
    }
    Ok(path.to_path_buf())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111) != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    // Non-unix platforms have no portable executable bit; fall back to
    // existence-as-file. Discovery semantics are documented unix-only.
    path.is_file()
}

fn search_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    search_dirs(std::env::split_paths(&path_var))
}

fn search_dirs<I>(dirs: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    let target = if cfg!(windows) { "claude.exe" } else { "claude" };
    for dir in dirs {
        let candidate = dir.join(target);
        if candidate.is_file() && is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Builder over a `claude` invocation. Constructed via
/// [`ClaudeBinary::spawn_builder`].
#[derive(Debug, Clone)]
pub struct ClaudeSpawn {
    binary: ClaudeBinary,
    settings: Option<PathBuf>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
}

impl ClaudeSpawn {
    /// Path to a `--settings` JSON file (the strict allowlist surface). The
    /// flag is appended at spawn time so callers cannot accidentally drop it
    /// when chaining `.arg(...)`.
    pub fn with_settings(mut self, path: impl Into<PathBuf>) -> Self {
        self.settings = Some(path.into());
        self
    }

    /// Append a single CLI argument.
    pub fn arg(mut self, value: impl Into<String>) -> Self {
        self.args.push(value.into());
        self
    }

    /// Append a sequence of CLI arguments.
    pub fn args<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(values.into_iter().map(Into::into));
        self
    }

    /// Set an environment variable on the child.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Set the child's working directory.
    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }

    /// Spawn `claude` with stdin/stdout/stderr piped and stdout/stderr wrapped
    /// in line-oriented readers.
    pub async fn spawn(self) -> Result<ClaudeProcess, ClaudeError> {
        let Self {
            binary,
            settings,
            args,
            env,
            cwd,
        } = self;

        let mut command = Command::new(binary.path());
        if let Some(settings_path) = settings {
            command.arg("--settings").arg(settings_path);
        }
        command.args(&args);
        for (key, value) in &env {
            command.env(key, value);
        }
        if let Some(working_dir) = cwd {
            command.current_dir(working_dir);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|source| ClaudeError::Spawn { source })?;
        let stdin = child
            .stdin
            .take()
            .ok_or(ClaudeError::MissingPipe { pipe: "stdin" })?;
        let stdout = child
            .stdout
            .take()
            .ok_or(ClaudeError::MissingPipe { pipe: "stdout" })?;
        let stderr = child
            .stderr
            .take()
            .ok_or(ClaudeError::MissingPipe { pipe: "stderr" })?;

        Ok(ClaudeProcess {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            stderr: BufReader::new(stderr).lines(),
        })
    }
}

/// Typed handle over a spawned `claude` child. Owns the child plus the three
/// captured pipes; the line readers permit straightforward `await
/// next_line()` consumption from the orchestrator-session and phase-subprocess
/// adapters.
#[derive(Debug)]
pub struct ClaudeProcess {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: Lines<BufReader<ChildStdout>>,
    pub stderr: Lines<BufReader<ChildStderr>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_fake_claude(dir: &Path, body: &str) -> PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("claude");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.sync_all().unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn discover_via_override_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fake_claude(tmp.path(), "#!/bin/sh\necho ok\n");
        let bin = ClaudeBinary::discover(Some(&path)).unwrap();
        assert_eq!(bin.path(), path);
    }

    #[cfg(unix)]
    #[test]
    fn discover_override_missing_yields_actionable_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        let err = ClaudeBinary::discover(Some(&missing)).unwrap_err();
        match err {
            ClaudeError::OverrideMissing { path } => assert_eq!(path, missing),
            other => panic!("expected OverrideMissing, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn discover_override_non_executable_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("claude");
        std::fs::write(&path, b"not exec").unwrap();
        let err = ClaudeBinary::discover(Some(&path)).unwrap_err();
        match err {
            ClaudeError::OverrideNotExecutable { path: p } => assert_eq!(p, path),
            other => panic!("expected OverrideNotExecutable, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn search_dirs_returns_none_when_no_dir_holds_claude() {
        // Empty directory list emulates "PATH search failed"; pairs with the
        // NotFound branch in `discover` since `discover(None)` flows through
        // the same `search_dirs` helper.
        let tmp = tempfile::tempdir().unwrap();
        assert!(super::search_dirs([tmp.path().to_path_buf()]).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn search_dirs_finds_executable_claude_in_listed_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fake_claude(tmp.path(), "#!/bin/sh\necho ok\n");
        assert_eq!(
            super::search_dirs([tmp.path().to_path_buf()]),
            Some(path)
        );
    }

    #[cfg(unix)]
    #[test]
    fn search_dirs_skips_non_executable_match() {
        let tmp = tempfile::tempdir().unwrap();
        // Same name, missing executable bit → must be skipped.
        let path = tmp.path().join("claude");
        std::fs::write(&path, b"shim").unwrap();
        assert!(super::search_dirs([tmp.path().to_path_buf()]).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_pipes_stdin_and_reads_stdout_line() {
        let tmp = tempfile::tempdir().unwrap();
        // Echo back any line written to stdin, prefixed with "echo:".
        let body = "#!/bin/sh\nread line\nprintf 'echo:%s\\n' \"$line\"\n";
        let path = write_fake_claude(tmp.path(), body);
        let bin = ClaudeBinary::discover(Some(&path)).unwrap();
        let mut proc = bin.spawn_builder().spawn().await.unwrap();

        use tokio::io::AsyncWriteExt;
        proc.stdin.write_all(b"hello\n").await.unwrap();
        proc.stdin.flush().await.unwrap();
        // Drop stdin so the subshell's `read` can complete EOF cleanly.
        drop(proc.stdin);

        let line = proc.stdout.next_line().await.unwrap();
        assert_eq!(line.as_deref(), Some("echo:hello"));

        let status = proc.child.wait().await.unwrap();
        assert!(status.success());
    }
}
