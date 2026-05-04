//! Tracing pipeline + redaction layer + per-issue debug capture sink.
//!
//! Boundary: this module is the only place in the daemon that installs the
//! global `tracing-subscriber` and the only writer of per-issue debug log
//! files. All other modules emit through the `tracing` macros and rely on
//! this module's redaction layer to scrub secrets before egress.
//!
//! Design references:
//! - design.md File Structure Plan line 279 (logging.rs scope)
//! - design.md "Daemon bootstrap" step 2 (redaction-aware reinit)
//! - Requirements 1.5, 7.4, 11.2, 11.3, 11.4, 11.6, 11.7
//!
//! Comments here call out non-obvious "why", never restate the rule.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Errors surfaced from `init`. File-destination errors carry the offending
/// path so the operator's first log line points at the misconfigured value.
#[derive(Debug, Error)]
pub enum LoggingError {
    #[error("invalid log level `{level}`: expected one of trace|debug|info|warn|error")]
    InvalidLevel { level: String },

    #[error("failed to open log file at {path}: {source}")]
    OpenLogFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("global tracing subscriber already installed")]
    AlreadyInstalled,
}

#[derive(Debug, Clone)]
pub enum LogDestination {
    Stdout,
    File(PathBuf),
    Both(PathBuf),
}

#[derive(Debug, Clone)]
pub struct LoggingConfig {
    pub level: String,
    pub destination: LogDestination,
    pub json: bool,
    /// Plaintext secret strings to scrub from every emitted event before
    /// egress. The list is matched verbatim — no length floor, no regex.
    pub redaction_secrets: Vec<String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            destination: LogDestination::Stdout,
            json: false,
            redaction_secrets: Vec::new(),
        }
    }
}

/// Holds resources whose lifetime must match the subscriber's. Drop flushes
/// the file destination by releasing the file handle.
#[derive(Debug)]
pub struct LoggingGuard {
    _file: Option<File>,
}

impl LoggingGuard {
    /// Construct a no-op guard. Used by the runtime when the global
    /// subscriber was already installed (e.g., test harness re-entry) so
    /// the bootstrap does not have to expose its own sentinel.
    pub fn sentinel() -> Self {
        Self { _file: None }
    }
}

// ---------------------------------------------------------------------------
// Field context helpers
// ---------------------------------------------------------------------------

/// Standardized fields attached to events that have them, per Req 11.2.
#[derive(Debug, Clone)]
pub struct FieldContext {
    pub issue: String,
    pub repo: Option<String>,
    pub correlation_id: String,
    pub role: RoleTag,
}

impl FieldContext {
    /// Build a span carrying the documented fields. Call sites enter the
    /// returned span before emitting events that should inherit the context.
    pub fn into_span(self) -> tracing::Span {
        let role = self.role.to_string();
        let repo = self.repo.unwrap_or_default();
        tracing::info_span!(
            "ctx",
            issue = %self.issue,
            repo = %repo,
            correlation_id = %self.correlation_id,
            role = %role,
        )
    }
}

/// Produce a fresh correlation identifier for one subprocess invocation.
pub fn correlation_id_new() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// Per-issue debug sink
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTag {
    Stdout,
    Stderr,
}

impl fmt::Display for StreamTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdout => f.write_str("STDOUT"),
            Self::Stderr => f.write_str("STDERR"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleTag {
    Orchestrator,
    Phase(String),
}

impl fmt::Display for RoleTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Orchestrator => f.write_str("orchestrator"),
            Self::Phase(name) => write!(f, "phase:{name}"),
        }
    }
}

/// Factory binding the per-issue debug directory. Cheap to clone — no I/O
/// happens until a sink is materialized and `append` is called.
#[derive(Debug, Clone)]
pub struct DebugSinkFactory {
    dir: PathBuf,
}

impl DebugSinkFactory {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn for_issue(&self, issue: &str) -> PerIssueDebugSink {
        PerIssueDebugSink {
            path: self.dir.join(format!("{issue}.log")),
            file: None,
            broken: false,
        }
    }
}

/// Append-only per-issue debug log writer. Lazy on first write; failures are
/// logged via `tracing::warn!` once and then silently no-op so the owning
/// subprocess launch never aborts on debug-sink trouble (Req 11.7).
#[derive(Debug)]
pub struct PerIssueDebugSink {
    path: PathBuf,
    file: Option<File>,
    broken: bool,
}

impl PerIssueDebugSink {
    pub fn append(&mut self, stream: StreamTag, role: &RoleTag, line: &str) {
        if self.broken {
            return;
        }

        if self.file.is_none() {
            match Self::open(&self.path) {
                Ok(f) => self.file = Some(f),
                Err(e) => {
                    self.mark_broken(&e);
                    return;
                }
            }
        }

        let now = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "0000-00-00T00:00:00Z".to_owned());
        // RFC 3339 from `time` may emit either fractional or no fractional
        // depending on the underlying clock. The spec demands nanosecond
        // resolution, so we normalize to a 9-digit fraction.
        let now = ensure_nanosecond_fraction(&now);

        let formatted = format!("{now} [{stream}] {role} {line}\n");

        if let Some(file) = self.file.as_mut()
            && let Err(e) = file.write_all(formatted.as_bytes())
        {
            self.mark_broken(&e);
        }
    }

    fn open(path: &Path) -> io::Result<File> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        OpenOptions::new().create(true).append(true).open(path)
    }

    fn mark_broken(&mut self, err: &io::Error) {
        self.broken = true;
        // The first failure surfaces structurally; subsequent appends are
        // silent no-ops so the subprocess launch can keep going.
        tracing::warn!(
            path = %self.path.display(),
            error = %err,
            "per-issue debug log unavailable; subsequent lines for this issue will be dropped"
        );
    }
}

/// `time` returns RFC 3339 with as many fractional digits as the platform
/// provides. Pad to 9 (or insert `.000000000`) so every line we emit matches
/// the format documented in design.md.
fn ensure_nanosecond_fraction(formatted: &str) -> String {
    // The timezone marker (`Z`, `+`, or `-`) is always the last character of
    // an RFC 3339 date with optional fraction. The numeric date portion runs
    // through index 18 (`...:56`); the marker therefore sits at index ≥19.
    let (head, tz) = match formatted.rfind(['Z', '+', '-']) {
        Some(idx) if idx >= 19 => formatted.split_at(idx),
        _ => return formatted.to_owned(),
    };

    if let Some(dot) = head.find('.') {
        let (prefix, frac) = head.split_at(dot);
        let digits = &frac[1..];
        let padded = format!("{digits:0<9}");
        let truncated = &padded[..9];
        format!("{prefix}.{truncated}{tz}")
    } else {
        format!("{head}.000000000{tz}")
    }
}

// ---------------------------------------------------------------------------
// Redaction layer
// ---------------------------------------------------------------------------

/// `MakeWriter` wrapper that rewrites every byte chunk emitted by the
/// formatter, replacing each configured secret with `***REDACTED***`.
///
/// Using a writer-level scrub (rather than a tracing `Layer` that intercepts
/// raw fields) is the only way to catch secrets that appear in `Display`
/// output of structured fields, in formatted message strings, and in nested
/// field rendering — all of them bottom out in the same byte stream.
#[derive(Clone)]
struct RedactingWriter<W> {
    inner: W,
    secrets: Arc<Vec<String>>,
}

impl<W: io::Write> io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let scrubbed = redact_bytes(buf, &self.secrets);
        self.inner.write_all(&scrubbed)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Clone)]
struct RedactingMakeWriter<M> {
    inner: M,
    secrets: Arc<Vec<String>>,
}

impl<'a, M> MakeWriter<'a> for RedactingMakeWriter<M>
where
    M: MakeWriter<'a> + 'a,
{
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer(),
            secrets: self.secrets.clone(),
        }
    }
}

fn redact_bytes(buf: &[u8], secrets: &[String]) -> Vec<u8> {
    if secrets.is_empty() {
        return buf.to_vec();
    }
    // Scrubbing the textual rendering is sufficient because every formatter
    // path (json or pretty) ultimately writes UTF-8. Non-UTF-8 input is
    // pass-through; we accept that only ASCII / UTF-8 secrets are scrubbed.
    match std::str::from_utf8(buf) {
        Ok(text) => {
            let mut out = text.to_owned();
            for secret in secrets {
                if !secret.is_empty() {
                    out = out.replace(secret, "***REDACTED***");
                }
            }
            out.into_bytes()
        }
        Err(_) => buf.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

/// Install a global tracing subscriber per `config`. Returns a guard whose
/// drop flushes any owned file destination. Re-init within a single process
/// fails with `LoggingError::AlreadyInstalled`; tests requiring isolated
/// subscribers should use `tracing::subscriber::with_default` against a
/// per-test subscriber instead.
pub fn init(config: LoggingConfig) -> Result<LoggingGuard, LoggingError> {
    let level = parse_level(&config.level)?;

    let secrets = Arc::new(config.redaction_secrets.clone());

    let file_handle = match &config.destination {
        LogDestination::Stdout => None,
        LogDestination::File(path) | LogDestination::Both(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(|source| LoggingError::OpenLogFile {
                    path: path.clone(),
                    source,
                })?;
            }
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|source| LoggingError::OpenLogFile {
                    path: path.clone(),
                    source,
                })?;
            Some(f)
        }
    };

    install_subscriber(level, &config, file_handle.as_ref(), secrets)?;

    Ok(LoggingGuard { _file: file_handle })
}

fn install_subscriber(
    level: tracing::Level,
    config: &LoggingConfig,
    file: Option<&File>,
    secrets: Arc<Vec<String>>,
) -> Result<(), LoggingError> {
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::registry::Registry;

    let level_filter = LevelFilter::from_level(level);

    let stdout_layer = match config.destination {
        LogDestination::Stdout | LogDestination::Both(_) => Some(make_layer(
            RedactingMakeWriter {
                inner: std::io::stdout,
                secrets: secrets.clone(),
            },
            config.json,
        )),
        LogDestination::File(_) => None,
    };

    let file_layer = file.map(|f| {
        let arc = Arc::new(Mutex::new(f.try_clone().expect("file handle clonable")));
        make_layer(
            RedactingMakeWriter {
                inner: SharedFileWriter(arc),
                secrets: secrets.clone(),
            },
            config.json,
        )
    });

    Registry::default()
        .with(level_filter)
        .with(stdout_layer)
        .with(file_layer)
        .try_init()
        .map_err(|_| LoggingError::AlreadyInstalled)
}

fn make_layer<S, M>(
    writer: M,
    json: bool,
) -> Box<dyn Layer<S> + Send + Sync + 'static>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    M: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    // JSON output is selected at layer-build time because `fmt::layer()` and
    // `fmt::layer().json()` produce different concrete `Layer` types; we
    // erase to a trait object so both branches share one return path.
    if json {
        Box::new(tracing_subscriber::fmt::layer().json().with_writer(writer))
    } else {
        Box::new(tracing_subscriber::fmt::layer().with_writer(writer))
    }
}

#[derive(Clone)]
struct SharedFileWriter(Arc<Mutex<File>>);

impl<'a> MakeWriter<'a> for SharedFileWriter {
    type Writer = SharedFileWriterGuard;

    fn make_writer(&'a self) -> Self::Writer {
        SharedFileWriterGuard(self.0.clone())
    }
}

struct SharedFileWriterGuard(Arc<Mutex<File>>);

impl io::Write for SharedFileWriterGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .0
            .lock()
            .map_err(|e| io::Error::other(format!("file lock poisoned: {e}")))?;
        guard.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self
            .0
            .lock()
            .map_err(|e| io::Error::other(format!("file lock poisoned: {e}")))?;
        guard.flush()
    }
}

fn parse_level(value: &str) -> Result<tracing::Level, LoggingError> {
    value
        .parse::<tracing::Level>()
        .map_err(|_| LoggingError::InvalidLevel {
            level: value.to_owned(),
        })
}

// ---------------------------------------------------------------------------
// Test-facing redaction layer
// ---------------------------------------------------------------------------

/// Build a tracing `Layer` with the redaction writer, suitable for use with
/// `tracing::subscriber::with_default` in tests. Production code should call
/// [`init`] which wires the same writer into the global subscriber.
pub fn redacting_fmt_layer<S, W>(
    make_writer: W,
    secrets: Vec<String>,
) -> Box<dyn Layer<S> + Send + Sync + 'static>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let arc = Arc::new(secrets);
    Box::new(
        tracing_subscriber::fmt::layer()
            .with_writer(RedactingMakeWriter {
                inner: make_writer,
                secrets: arc,
            })
            .without_time(),
    )
}


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use tracing::{Level, info};
    use tracing_subscriber::fmt::MakeWriter;

    /// In-memory writer for isolating tracing output per test.
    #[derive(Clone, Default)]
    struct CaptureBuffer(Arc<Mutex<Vec<u8>>>);

    impl CaptureBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl<'a> MakeWriter<'a> for CaptureBuffer {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            CaptureWriter(self.0.clone())
        }
    }

    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn run_with_layer<F: FnOnce()>(secrets: Vec<String>, body: F) -> CaptureBuffer {
        use tracing_subscriber::layer::SubscriberExt;
        let buf = CaptureBuffer::default();
        let layer =
            redacting_fmt_layer::<tracing_subscriber::Registry, _>(buf.clone(), secrets);
        let subscriber = tracing_subscriber::Registry::default().with(layer);
        tracing::subscriber::with_default(subscriber, body);
        buf
    }

    /// Mirror of `make_layer`'s json branch for testing the JSON formatter
    /// path without installing a global subscriber. Kept inline (rather than
    /// promoting `make_layer` to `pub(crate)`) so the production helper stays
    /// fully private and the test owns its own surface.
    fn json_layer_for_test(
        buf: CaptureBuffer,
        secrets: Vec<String>,
    ) -> Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static>
    {
        let writer = RedactingMakeWriter {
            inner: buf,
            secrets: Arc::new(secrets),
        };
        Box::new(tracing_subscriber::fmt::layer().json().with_writer(writer))
    }

    #[test]
    fn parse_level_accepts_known_levels() {
        for (input, expected) in [
            ("trace", Level::TRACE),
            ("debug", Level::DEBUG),
            ("info", Level::INFO),
            ("warn", Level::WARN),
            ("error", Level::ERROR),
        ] {
            assert_eq!(parse_level(input).unwrap(), expected);
        }
    }

    #[test]
    fn init_rejects_invalid_level_string() {
        let err = init(LoggingConfig {
            level: "not-a-level".to_owned(),
            ..Default::default()
        })
        .unwrap_err();
        assert!(matches!(err, LoggingError::InvalidLevel { ref level } if level == "not-a-level"));
    }

    #[test]
    fn redaction_replaces_secret_in_field_value() {
        let buf = run_with_layer(vec!["deadbeef".to_owned()], || {
            info!(token = "deadbeef", "loaded");
        });
        let captured = buf.contents();
        assert!(
            !captured.contains("deadbeef"),
            "raw secret leaked: {captured}"
        );
        assert!(
            captured.contains("***REDACTED***"),
            "redaction marker missing: {captured}"
        );
    }

    #[test]
    fn redaction_replaces_secret_in_message_string() {
        let buf = run_with_layer(vec!["s3cr3t".to_owned()], || {
            // Field value, not interpolation, so the layer sees the secret
            // bytes when the formatter renders the event.
            info!(secret_field = "s3cr3t", "auth done");
        });
        let captured = buf.contents();
        assert!(!captured.contains("s3cr3t"), "leak: {captured}");
        assert!(captured.contains("***REDACTED***"));
    }

    #[test]
    fn redaction_handles_multiple_secrets() {
        let buf = run_with_layer(vec!["abc123".to_owned(), "xyz789".to_owned()], || {
            info!(a = "abc123", b = "xyz789", "both");
        });
        let captured = buf.contents();
        assert!(!captured.contains("abc123"));
        assert!(!captured.contains("xyz789"));
        let redacted_count = captured.matches("***REDACTED***").count();
        assert_eq!(redacted_count, 2, "expected 2 redactions in: {captured}");
    }

    #[test]
    fn redaction_handles_short_secret_no_length_floor() {
        // Even 3-character secrets are scrubbed per task spec.
        let buf = run_with_layer(vec!["abc".to_owned()], || {
            info!(field = "abc", "tiny");
        });
        let captured = buf.contents();
        assert!(!captured.contains("\"abc\""));
        assert!(!captured.contains("=abc "));
    }

    #[test]
    fn correlation_id_is_unique() {
        let a = correlation_id_new();
        let b = correlation_id_new();
        assert_ne!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn stream_tag_display_matches_documented_format() {
        assert_eq!(StreamTag::Stdout.to_string(), "STDOUT");
        assert_eq!(StreamTag::Stderr.to_string(), "STDERR");
    }

    #[test]
    fn role_tag_display_matches_documented_format() {
        assert_eq!(RoleTag::Orchestrator.to_string(), "orchestrator");
        assert_eq!(
            RoleTag::Phase("implement".to_owned()).to_string(),
            "phase:implement"
        );
    }

    #[test]
    fn per_issue_sink_writes_documented_format() {
        let dir = TempDir::new().unwrap();
        let factory = DebugSinkFactory::new(dir.path());
        let mut sink = factory.for_issue("ENG-1");

        sink.append(
            StreamTag::Stdout,
            &RoleTag::Phase("implement".to_owned()),
            "hello world",
        );
        sink.append(
            StreamTag::Stderr,
            &RoleTag::Phase("implement".to_owned()),
            "an error",
        );

        let mut contents = String::new();
        File::open(dir.path().join("ENG-1.log"))
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();

        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines, got: {contents}");

        let pattern = regex::Regex::new(
            r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{9}Z \[(STDOUT|STDERR)\] (orchestrator|phase:[^ ]+) .*$",
        )
        .unwrap();

        for line in &lines {
            assert!(pattern.is_match(line), "line did not match format: {line}");
        }
        assert!(lines[0].contains("[STDOUT] phase:implement hello world"));
        assert!(lines[1].contains("[STDERR] phase:implement an error"));
    }

    #[test]
    fn per_issue_sink_uses_orchestrator_role_tag() {
        let dir = TempDir::new().unwrap();
        let factory = DebugSinkFactory::new(dir.path());
        let mut sink = factory.for_issue("ENG-2");
        sink.append(StreamTag::Stdout, &RoleTag::Orchestrator, "boot");

        let mut contents = String::new();
        File::open(dir.path().join("ENG-2.log"))
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert!(
            contents.contains("[STDOUT] orchestrator boot"),
            "missing orchestrator tag: {contents}"
        );
    }

    #[test]
    fn per_issue_sink_creates_missing_directory() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("nested").join("debug");
        let factory = DebugSinkFactory::new(&nested);
        let mut sink = factory.for_issue("ENG-3");
        sink.append(StreamTag::Stdout, &RoleTag::Orchestrator, "ok");

        assert!(nested.join("ENG-3.log").exists());
    }

    #[test]
    fn per_issue_sink_open_failure_does_not_panic() {
        // `/dev/null/forbidden` cannot be a directory: `/dev/null` is a char
        // device, so create_dir_all + open both fail. Sink must swallow.
        let factory = DebugSinkFactory::new(PathBuf::from("/dev/null/forbidden"));
        let mut sink = factory.for_issue("ENG-X");
        sink.append(StreamTag::Stdout, &RoleTag::Orchestrator, "first");
        // Second append must also be silent no-op.
        sink.append(StreamTag::Stderr, &RoleTag::Orchestrator, "second");
        assert!(sink.broken);
    }

    #[test]
    fn ensure_nanosecond_fraction_pads_short_fraction() {
        let padded = ensure_nanosecond_fraction("2026-05-05T12:34:56.123Z");
        assert_eq!(padded, "2026-05-05T12:34:56.123000000Z");
    }

    #[test]
    fn ensure_nanosecond_fraction_inserts_when_missing() {
        let padded = ensure_nanosecond_fraction("2026-05-05T12:34:56Z");
        assert_eq!(padded, "2026-05-05T12:34:56.000000000Z");
    }

    #[test]
    fn ensure_nanosecond_fraction_truncates_overlong() {
        let padded = ensure_nanosecond_fraction("2026-05-05T12:34:56.1234567890123Z");
        assert_eq!(padded, "2026-05-05T12:34:56.123456789Z");
    }

    #[test]
    fn json_layer_emits_parseable_json_with_documented_fields() {
        use tracing_subscriber::layer::SubscriberExt;

        let buf = CaptureBuffer::default();
        let subscriber = tracing_subscriber::Registry::default()
            .with(json_layer_for_test(buf.clone(), vec![]));
        tracing::subscriber::with_default(subscriber, || {
            info!(target: "roki::test", phase = "implement", "json line");
        });

        let captured = buf.contents();
        // The JSON formatter writes one event per line; pick the first
        // non-empty line so we don't depend on trailing newlines.
        let line = captured
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_else(|| panic!("no captured output: {captured:?}"));

        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("captured line is not JSON ({e}): {line}"));

        assert_eq!(value["level"], "INFO", "missing/wrong level: {line}");
        assert_eq!(value["target"], "roki::test", "missing/wrong target: {line}");
        let fields = value
            .get("fields")
            .unwrap_or_else(|| panic!("missing fields object: {line}"));
        assert_eq!(fields["message"], "json line", "missing message: {line}");
        assert_eq!(fields["phase"], "implement", "missing structured field: {line}");
    }

    #[test]
    fn json_layer_redacts_secrets_in_json_output() {
        use tracing_subscriber::layer::SubscriberExt;

        let buf = CaptureBuffer::default();
        let subscriber = tracing_subscriber::Registry::default().with(json_layer_for_test(
            buf.clone(),
            vec!["topsecret".to_owned()],
        ));
        tracing::subscriber::with_default(subscriber, || {
            info!(token = "topsecret", "auth");
        });

        let captured = buf.contents();
        assert!(
            !captured.contains("topsecret"),
            "secret leaked into json output: {captured}"
        );
        assert!(
            captured.contains("***REDACTED***"),
            "redaction marker missing: {captured}"
        );
    }

    #[test]
    fn field_context_into_span_records_documented_fields() {
        let buf = run_with_layer(vec![], || {
            let ctx = FieldContext {
                issue: "ENG-1".to_owned(),
                repo: Some("owner/repo".to_owned()),
                correlation_id: "corr-abc".to_owned(),
                role: RoleTag::Phase("implement".to_owned()),
            };
            let span = ctx.into_span();
            let _enter = span.enter();
            info!(event = "phase_start", "spawning");
        });
        let captured = buf.contents();
        assert!(captured.contains("ENG-1"), "missing issue: {captured}");
        assert!(captured.contains("owner/repo"), "missing repo: {captured}");
        assert!(
            captured.contains("corr-abc"),
            "missing correlation_id: {captured}"
        );
        assert!(
            captured.contains("phase:implement"),
            "missing role: {captured}"
        );
    }
}
