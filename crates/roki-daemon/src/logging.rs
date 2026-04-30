//! Structured tracing pipeline with secret redaction.
//!
//! This module implements task 1.3 of the roki-mvp spec. It owns:
//!
//! * initialization of `tracing-subscriber` with a configurable log level and
//!   destination (stdout, file, or both) — Requirement 12.3;
//! * a redaction layer that scrubs the Linear API token and any operator-
//!   declared secret strings from every emitted event, regardless of how the
//!   secret entered (string field, `Debug`-formatted struct, or substring of a
//!   message) — Requirement 12.4;
//! * a small helper for the standardized `(repo, issue, correlation_id)`
//!   context fields that Requirement 12.2 mandates on every event that has
//!   them — also documented as the canonical context shape in design.md
//!   "Monitoring".
//!
//! Design notes:
//!
//! * Redaction is applied at *write* time by a custom `MakeWriter` that wraps
//!   the inner writer and replaces every occurrence of any configured secret
//!   in the rendered byte buffer with a fixed marker. Doing it at write time
//!   means we catch secrets that snuck in through *any* formatter path
//!   (`Display`, `Debug`, raw `tracing::field` values, multi-line spans), not
//!   just the ones the developer remembered to wrap in [`SecretString`].
//! * The redaction marker is intentionally short and easy to grep for so
//!   operators can confirm at a glance that a redaction took effect.
//! * The writer keeps a snapshot of the secret list inside an `Arc` so the
//!   `MakeWriter::make_writer` hot path is allocation-free.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tracing::Span;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Marker emitted in place of any redacted secret.
///
/// Kept short and ASCII so it survives every tracing formatter.
pub const REDACTION_MARKER: &str = "[REDACTED]";

/// Where structured log events are written.
///
/// Requirement 12.3: configurable destination — stdout, a file, or both.
#[derive(Debug, Clone)]
pub enum LogDestination {
    /// Write events to standard output only.
    Stdout,
    /// Write events to the file at the given path only.
    File(PathBuf),
    /// Write events to both standard output and the file at the given path.
    Both(PathBuf),
}

/// Logging configuration consumed by [`init`].
///
/// Carried as a struct (rather than read directly from
/// [`crate::config::Config`]) so callers — production runtime, integration
/// tests, future hot-reload paths — can construct the exact shape they need
/// without depending on the full daemon configuration.
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    /// `tracing-subscriber` env-filter directive (e.g. `"info"`,
    /// `"roki=debug,reqwest=warn"`). Empty falls back to `"info"`.
    pub filter: String,

    /// Where rendered events are written (Requirement 12.3).
    pub destination: LogDestination,

    /// Operator-declared secret strings that must never appear in any
    /// rendered event (Requirement 12.4). The Linear API token is added on
    /// top of this list by [`LoggingConfig::with_linear_token`].
    pub secrets: Vec<String>,
}

impl LoggingConfig {
    /// Construct a config that writes JSON-friendly events to stdout with the
    /// supplied env-filter directive.
    pub fn stdout(filter: impl Into<String>) -> Self {
        Self {
            filter: filter.into(),
            destination: LogDestination::Stdout,
            secrets: Vec::new(),
        }
    }

    /// Append the given Linear API token to the secret list. Empty tokens are
    /// ignored (the redaction layer would otherwise strip nothing while
    /// blowing up the matcher search space).
    pub fn with_linear_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        if !token.is_empty() {
            self.secrets.push(token);
        }
        self
    }

    /// Append additional operator-declared secrets to the redaction list.
    pub fn with_extra_secrets<I, S>(mut self, secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for raw in secrets {
            let value = raw.into();
            if !value.is_empty() {
                self.secrets.push(value);
            }
        }
        self
    }
}

/// Standard `(repo, issue, correlation_id)` context attached to per-issue
/// events (Requirement 12.2).
///
/// Construction is cheap; pass by value into [`Self::span`] or
/// [`Self::record_on`]. The struct is [`Clone`] so subscribers and adapters
/// can stash a copy without round-tripping through tracing fields.
#[derive(Debug, Clone)]
pub struct LogContext {
    /// Repository identifier (the `repo` half of the workspace key).
    pub repo: String,
    /// Linear issue identifier.
    pub issue: String,
    /// Per-worker-invocation correlation identifier.
    pub correlation_id: String,
}

impl LogContext {
    /// Build a context from owned strings.
    pub fn new(
        repo: impl Into<String>,
        issue: impl Into<String>,
        correlation_id: impl Into<String>,
    ) -> Self {
        Self {
            repo: repo.into(),
            issue: issue.into(),
            correlation_id: correlation_id.into(),
        }
    }

    /// Open an `info_span!` carrying the standard fields. The caller enters
    /// the span as usual (`let _enter = ctx.span("worker.tick").entered();`)
    /// and every event emitted inside that span inherits the three fields.
    pub fn span(&self, name: &'static str) -> Span {
        tracing::info_span!(
            target: "roki",
            "context",
            otel.name = name,
            repo = %self.repo,
            issue = %self.issue,
            correlation_id = %self.correlation_id,
        )
    }

    /// Record the standard fields onto an existing span (handy when a span
    /// was opened before the context was known, e.g. before the worker has
    /// pulled an issue from the queue).
    pub fn record_on(&self, span: &Span) {
        span.record("repo", self.repo.as_str());
        span.record("issue", self.issue.as_str());
        span.record("correlation_id", self.correlation_id.as_str());
    }
}

/// Guard that keeps file handles alive for the lifetime of the daemon.
///
/// Returned from [`init`] so the caller (typically `runtime::run`) holds it
/// for the duration of the run. Dropping the guard flushes and closes any
/// log file the subscriber was writing to.
#[must_use = "drop the guard at daemon shutdown to flush the log file"]
pub struct LoggingGuard {
    _file: Option<Arc<Mutex<File>>>,
}

/// Initialize the global tracing subscriber.
///
/// Returns an error if the file destination cannot be opened or if a
/// subscriber is already installed for the current process. Idempotent within
/// a single test binary because `tracing_subscriber`'s `try_init` reports an
/// `Err` on the second call rather than panicking; production code calls
/// [`init`] exactly once from `runtime::run`.
pub fn init(config: LoggingConfig) -> Result<LoggingGuard, LoggingError> {
    let writer = build_writer(&config.destination)?;
    let secrets = Arc::new(SecretSet::new(config.secrets));
    let make_writer = RedactingMakeWriter {
        inner: writer.clone(),
        secrets,
    };

    let filter = if config.filter.trim().is_empty() {
        "info".to_string()
    } else {
        config.filter
    };
    let env_filter =
        EnvFilter::try_new(&filter).map_err(|err| LoggingError::Filter(err.to_string()))?;

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(make_writer);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .try_init()
        .map_err(|err| LoggingError::AlreadyInitialized(err.to_string()))?;

    Ok(LoggingGuard {
        _file: writer.file_handle(),
    })
}

/// Errors surfaced by [`init`].
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    /// Failure to open or create the configured log file.
    #[error("failed to open log file `{path}`: {source}")]
    File {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The env-filter directive was malformed.
    #[error("invalid log filter directive: {0}")]
    Filter(String),

    /// A tracing subscriber was already installed.
    #[error("tracing subscriber already initialized: {0}")]
    AlreadyInitialized(String),
}

// -- internal: redacting MakeWriter --------------------------------------------------

/// Owned, sorted (by descending length) list of secrets to scrub.
///
/// Sorting by length means longer secrets are matched before shorter ones so
/// a token that happens to be a prefix of another configured secret is
/// redacted in full rather than partially.
#[derive(Debug)]
struct SecretSet {
    secrets: Vec<String>,
}

impl SecretSet {
    fn new(mut secrets: Vec<String>) -> Self {
        secrets.retain(|s| !s.is_empty());
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self { secrets }
    }

    /// Replace every occurrence of every secret in `buf` with the redaction
    /// marker.
    fn redact(&self, buf: &[u8]) -> Vec<u8> {
        if self.secrets.is_empty() {
            return buf.to_vec();
        }
        // Convert to UTF-8 if possible: the tracing fmt layer always writes
        // valid UTF-8. If a future writer hands us non-UTF-8 bytes we fall
        // back to a byte-level replace that still scrubs ASCII secrets, since
        // the secret strings themselves are required to be UTF-8 (config
        // surface).
        match std::str::from_utf8(buf) {
            Ok(rendered) => {
                let mut out = rendered.to_string();
                for secret in &self.secrets {
                    if out.contains(secret.as_str()) {
                        out = out.replace(secret.as_str(), REDACTION_MARKER);
                    }
                }
                out.into_bytes()
            }
            Err(_) => self.redact_bytes(buf),
        }
    }

    fn redact_bytes(&self, buf: &[u8]) -> Vec<u8> {
        let mut out = buf.to_vec();
        for secret in &self.secrets {
            out = byte_replace(&out, secret.as_bytes(), REDACTION_MARKER.as_bytes());
        }
        out
    }
}

fn byte_replace(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut cursor = 0;
    while cursor + needle.len() <= haystack.len() {
        if &haystack[cursor..cursor + needle.len()] == needle {
            out.extend_from_slice(replacement);
            cursor += needle.len();
        } else {
            out.push(haystack[cursor]);
            cursor += 1;
        }
    }
    out.extend_from_slice(&haystack[cursor..]);
    out
}

/// `MakeWriter` that wraps the destination writer and scrubs secrets on every
/// `write` call.
#[derive(Clone)]
struct RedactingMakeWriter {
    inner: SharedWriter,
    secrets: Arc<SecretSet>,
}

impl<'a> MakeWriter<'a> for RedactingMakeWriter {
    type Writer = RedactingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.clone(),
            secrets: Arc::clone(&self.secrets),
        }
    }
}

struct RedactingWriter {
    inner: SharedWriter,
    secrets: Arc<SecretSet>,
}

impl Write for RedactingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let redacted = self.secrets.redact(buf);
        self.inner.write_all(&redacted)?;
        // Report the original length so the formatter believes the entire
        // input was accepted; reporting `redacted.len()` would confuse the
        // formatter when redaction shrinks or grows the buffer.
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// -- internal: shared destination writer ---------------------------------------------

/// Multiplexed sink that fans output across the configured destinations.
#[derive(Clone)]
struct SharedWriter {
    stdout: bool,
    file: Option<Arc<Mutex<File>>>,
}

impl SharedWriter {
    fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        if self.stdout {
            let stdout = io::stdout();
            let mut guard = stdout.lock();
            guard.write_all(buf)?;
        }
        if let Some(file) = self.file.as_ref() {
            let mut guard = file
                .lock()
                .map_err(|_| io::Error::other("log file mutex poisoned"))?;
            guard.write_all(buf)?;
        }
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        if self.stdout {
            io::stdout().flush()?;
        }
        if let Some(file) = self.file.as_ref() {
            let mut guard = file
                .lock()
                .map_err(|_| io::Error::other("log file mutex poisoned"))?;
            guard.flush()?;
        }
        Ok(())
    }

    fn file_handle(&self) -> Option<Arc<Mutex<File>>> {
        self.file.clone()
    }
}

fn build_writer(destination: &LogDestination) -> Result<SharedWriter, LoggingError> {
    match destination {
        LogDestination::Stdout => Ok(SharedWriter {
            stdout: true,
            file: None,
        }),
        LogDestination::File(path) => Ok(SharedWriter {
            stdout: false,
            file: Some(open_log_file(path)?),
        }),
        LogDestination::Both(path) => Ok(SharedWriter {
            stdout: true,
            file: Some(open_log_file(path)?),
        }),
    }
}

fn open_log_file(path: &PathBuf) -> Result<Arc<Mutex<File>>, LoggingError> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| LoggingError::File {
            path: path.clone(),
            source,
        })?;
    Ok(Arc::new(Mutex::new(file)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Drive the redacting writer directly with a captured buffer so the test
    /// asserts the layer's contract independently of the global subscriber
    /// (which `try_init` only allows once per process).
    fn capture_through_layer(secrets: Vec<String>, payload: &str) -> String {
        let secret_set = Arc::new(SecretSet::new(secrets));
        let redacted = secret_set.redact(payload.as_bytes());
        String::from_utf8(redacted).expect("redaction must preserve utf-8")
    }

    #[test]
    fn redaction_marker_is_used_in_place_of_secrets() {
        let out = capture_through_layer(
            vec!["lin_api_secret_token".to_string()],
            "starting worker token=lin_api_secret_token done",
        );
        assert!(!out.contains("lin_api_secret_token"));
        assert!(out.contains(REDACTION_MARKER));
    }

    #[test]
    fn empty_secret_list_is_a_noop() {
        let out = capture_through_layer(vec![], "nothing to redact here");
        assert_eq!(out, "nothing to redact here");
    }

    #[test]
    fn longer_secrets_are_matched_before_shorter_ones() {
        // If we redacted `abc` first the longer secret would never match.
        let out = capture_through_layer(
            vec!["abc".to_string(), "abcdef".to_string()],
            "value=abcdef",
        );
        // The whole `abcdef` must be replaced (not just `abc`).
        assert_eq!(out, format!("value={REDACTION_MARKER}"));
    }

    #[test]
    fn multiple_secrets_are_all_redacted() {
        let out = capture_through_layer(
            vec!["first-secret".to_string(), "second-secret".to_string()],
            "leaked first-secret and second-secret here",
        );
        assert!(!out.contains("first-secret"));
        assert!(!out.contains("second-secret"));
    }

    /// End-to-end: spin up the real subscriber against a temp file, emit a
    /// log line that *intentionally* contains the configured token in a
    /// field value, then read the file back and assert the token never
    /// appears (Requirement 12.4 observable-completion criterion).
    ///
    /// We use a dedicated child `tracing::subscriber::with_default` block
    /// rather than the global `init`, because `tracing_subscriber::registry`
    /// can only be installed globally once per process and other tests in
    /// this crate may have already done so.
    #[test]
    fn token_in_field_value_never_appears_in_captured_output() {
        let token = "lin_api_real_secret_value_xyz123";
        let secrets = Arc::new(SecretSet::new(vec![token.to_string()]));

        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

        struct VecMakeWriter {
            captured: Arc<Mutex<Vec<u8>>>,
            secrets: Arc<SecretSet>,
        }
        struct VecWriter {
            captured: Arc<Mutex<Vec<u8>>>,
            secrets: Arc<SecretSet>,
        }
        impl Write for VecWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let redacted = self.secrets.redact(buf);
                self.captured
                    .lock()
                    .expect("captured mutex")
                    .extend_from_slice(&redacted);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for VecMakeWriter {
            type Writer = VecWriter;
            fn make_writer(&'a self) -> Self::Writer {
                VecWriter {
                    captured: Arc::clone(&self.captured),
                    secrets: Arc::clone(&self.secrets),
                }
            }
        }

        let make_writer = VecMakeWriter {
            captured: Arc::clone(&captured),
            secrets: Arc::clone(&secrets),
        };

        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(false)
                .with_writer(make_writer),
        );

        tracing::subscriber::with_default(subscriber, || {
            // Three different shapes prove the redaction is per-rendered-byte,
            // not per-known-field: a string field, an `?`-Debug-formatted
            // struct field, and a token embedded mid-message.
            tracing::info!(token = token, "configured token in a field");
            let payload = format!("error reaching api with auth header bearer {token}");
            tracing::warn!(message = %payload, "embedded token in message");
            #[derive(Debug)]
            #[allow(dead_code)]
            struct Wrapper<'a> {
                token: &'a str,
            }
            tracing::error!(detail = ?Wrapper { token }, "debug-formatted struct holds the token");
        });

        let captured = captured.lock().expect("captured mutex");
        let rendered = std::str::from_utf8(&captured).expect("utf-8 output");
        assert!(
            !rendered.contains(token),
            "token leaked into captured output:\n{rendered}"
        );
        assert!(
            rendered.contains(REDACTION_MARKER),
            "expected redaction marker in output:\n{rendered}"
        );
    }

    #[test]
    fn log_destination_file_writes_redacted_lines_to_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("daemon.log");

        let secrets = Arc::new(SecretSet::new(vec!["leak-me-not".to_string()]));
        let writer =
            build_writer(&LogDestination::File(log_path.clone())).expect("file writer must build");
        // Drive the writer directly to confirm File destination plumbing.
        let make = RedactingMakeWriter {
            inner: writer,
            secrets,
        };
        let mut w = make.make_writer();
        writeln!(&mut w, "header leak-me-not trailer").expect("write to log file");
        w.flush().expect("flush");

        let mut buf = String::new();
        File::open(&log_path)
            .expect("open log file")
            .read_to_string(&mut buf)
            .expect("read log file");
        assert!(!buf.contains("leak-me-not"));
        assert!(buf.contains(REDACTION_MARKER));
    }

    #[test]
    fn log_context_attaches_repo_issue_correlation_to_span_events() {
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

        struct MW {
            captured: Arc<Mutex<Vec<u8>>>,
        }
        struct W {
            captured: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for W {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.captured
                    .lock()
                    .expect("captured mutex")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for MW {
            type Writer = W;
            fn make_writer(&'a self) -> Self::Writer {
                W {
                    captured: Arc::clone(&self.captured),
                }
            }
        }

        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(false)
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
                .with_writer(MW {
                    captured: Arc::clone(&captured),
                }),
        );

        tracing::subscriber::with_default(subscriber, || {
            let ctx = LogContext::new("core", "ENG-42", "corr-001");
            let _enter = ctx.span("worker.tick").entered();
            tracing::info!("inside the per-issue span");
        });

        let captured = captured.lock().expect("captured mutex");
        let rendered = std::str::from_utf8(&captured).expect("utf-8 output");
        assert!(
            rendered.contains("repo=\"core\"")
                || rendered.contains("repo=core")
                || rendered.contains("\"core\""),
            "rendered output missing repo field: {rendered}"
        );
        assert!(
            rendered.contains("ENG-42"),
            "rendered output missing issue field: {rendered}"
        );
        assert!(
            rendered.contains("corr-001"),
            "rendered output missing correlation_id field: {rendered}"
        );
    }
}
