//! Structured file logging for the server daemon.
//!
//! `crates/server` owns the logging sink: the subscriber, the on-disk format,
//! and the rotation policy. `crates/core` only generates semantic events
//! through the lightweight `tracing` facade and never depends on file output
//! or rotation, keeping the library-versus-daemon boundary intact.
//!
//! Records are written as one-line JSON documents with millisecond RFC-3339
//! timestamps in UTC, so the journal stays readable through plain console
//! tools (`grep`, `tail -f`, `less`) and structured parsers (`jq`, `lnav`).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use time::macros::format_description;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::EnvFilter;

/// Default rotation threshold: 10 MiB.
pub const DEFAULT_MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Number of archived files kept beside the active log.
pub const DEFAULT_ARCHIVE_COUNT: usize = 1;

/// Filter directive applied when `RUST_LOG` is unset.
const DEFAULT_LOG_LEVEL: &str = "info";

/// Repository-relative directory that stores the server journal.
const TAUQE_DIR: &str = ".tauqe";

/// Name of the active server log file.
const SERVER_LOG: &str = "server.log";

/// Initializes structured JSON file logging into `<repo_root>/.tauqe/server.log`.
///
/// The returned [`WorkerGuard`] must be held for the entire process lifetime:
/// it owns the non-blocking appender's buffer, and dropping it flushes every
/// pending record before the process exits.
pub fn init_logging(repo_root: &Path) -> io::Result<WorkerGuard> {
    init_logging_at(
        log_file_path(repo_root),
        DEFAULT_MAX_LOG_BYTES,
        DEFAULT_ARCHIVE_COUNT,
    )
}

/// Returns the path of the active server journal for a repository root.
///
/// This is the single source of truth for the journal location: both
/// [`init_logging`] and the daemon's startup log derive the path from here, so
/// the reported path can never drift from where records are actually written.
pub fn log_file_path(repo_root: &Path) -> PathBuf {
    repo_root.join(TAUQE_DIR).join(SERVER_LOG)
}

/// Installs a panic hook that records every uncaught panic into the journal.
///
/// The hook captures the source location, the textual payload, and — when
/// `RUST_BACKTRACE` enables it — the backtrace, then writes a single
/// `error`-level record with `target = "panic"` before the process unwinds.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|loc| loc.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let payload = panic_payload_message(info.payload());
        let backtrace = capture_backtrace();
        tracing::error!(
            target: "panic",
            location = %location,
            payload = %payload,
            backtrace = %backtrace,
            "Server panicked",
        );
    }));
}

/// Initializes file logging at an explicit path with explicit rotation limits.
fn init_logging_at(
    log_path: PathBuf,
    max_bytes: u64,
    archive_count: usize,
) -> io::Result<WorkerGuard> {
    let writer = SizeRotatingWriter::new(log_path, max_bytes, archive_count)?;
    let (non_blocking, guard) = tracing_appender::non_blocking(writer);
    let subscriber = build_subscriber(non_blocking, env_filter());
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|err| io::Error::other(format!("global subscriber already set: {err}")))?;
    Ok(guard)
}

/// Resolves the active filter, honoring `RUST_LOG` and defaulting to `info`.
fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_LEVEL))
}

/// Builds the shared JSON subscriber used by production startup and tests.
fn build_subscriber<W>(
    writer: W,
    filter: EnvFilter,
) -> impl tracing::Subscriber + Send + Sync + 'static
where
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        // The timestamp is rendered by the standard UTC timer, which delegates
        // calendar arithmetic to the `time` crate (Conventions §1.6). The
        // crate's RFC-3339 well-known format varies the subsecond width from
        // one instant to the next, so the format description fixes it at
        // exactly three digits: every record keeps the same byte layout and
        // millisecond precision.
        .with_timer(UtcTime::new(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        )))
        .with_writer(writer)
        .finish()
}

/// Extracts the human-readable text from a panic payload.
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Captures the current backtrace when `RUST_BACKTRACE` enables it.
fn capture_backtrace() -> String {
    let backtrace = std::backtrace::Backtrace::capture();
    match backtrace.status() {
        std::backtrace::BacktraceStatus::Captured => backtrace.to_string(),
        std::backtrace::BacktraceStatus::Disabled => {
            "backtrace disabled (set RUST_BACKTRACE=1)".to_string()
        }
        _ => "backtrace unavailable".to_string(),
    }
}

/// A file writer that rotates the active log into numbered archives once it
/// grows past `max_bytes`, keeping at most `archive_count` archived files.
struct SizeRotatingWriter {
    base_path: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    archive_count: usize,
}

impl SizeRotatingWriter {
    fn new(base_path: PathBuf, max_bytes: u64, archive_count: usize) -> io::Result<Self> {
        if let Some(parent) = base_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = open_log_file(&base_path)?;
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            base_path,
            file,
            written,
            max_bytes,
            archive_count,
        })
    }

    /// Moves the active log into `server.log.1`, shifts older archives upward,
    /// and starts a fresh active file, deleting anything beyond the retained
    /// count so the directory can never grow without bound.
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;

        let oldest = archive_path(&self.base_path, self.archive_count);
        if oldest.exists() {
            fs::remove_file(&oldest)?;
        }
        for index in (1..self.archive_count).rev() {
            let from = archive_path(&self.base_path, index);
            let to = archive_path(&self.base_path, index + 1);
            if from.exists() {
                fs::rename(&from, &to)?;
            }
        }
        if self.base_path.exists() {
            fs::rename(&self.base_path, archive_path(&self.base_path, 1))?;
        }

        self.file = open_log_file(&self.base_path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for SizeRotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.max_bytes > 0
            && self.written > 0
            && self.written + buf.len() as u64 > self.max_bytes
        {
            self.rotate()?;
        }
        let written = self.file.write(buf)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Opens the log file in append mode, creating it and its parents as needed.
fn open_log_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Derives the numbered archive path (`server.log.1`, `server.log.2`, ...).
fn archive_path(base: &Path, index: usize) -> PathBuf {
    let mut name = base
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{index}"));
    base.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use time::macros::format_description;
    use time::OffsetDateTime;

    fn unique_temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tauqe-server-log-{tag}-{nanos}-{id}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_bytes(writer: &mut SizeRotatingWriter, len: usize, byte: u8) {
        writer.write_all(&vec![byte; len]).unwrap();
        writer.flush().unwrap();
    }

    fn file_len(path: &Path) -> u64 {
        fs::metadata(path).unwrap().len()
    }

    #[test]
    fn rotation_moves_active_log_into_single_archive() {
        let dir = unique_temp_dir("rotate");
        let path = dir.join("server.log");
        let mut writer = SizeRotatingWriter::new(path.clone(), 100, 1).unwrap();

        write_bytes(&mut writer, 60, b'a');
        assert_eq!(file_len(&path), 60);
        assert!(!archive_path(&path, 1).exists());

        // Crossing the threshold rotates the active file before the next write.
        write_bytes(&mut writer, 60, b'b');
        write_bytes(&mut writer, 10, b'c');

        assert_eq!(file_len(&archive_path(&path, 1)), 60);
        assert_eq!(file_len(&path), 70);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotation_never_accumulates_archives_beyond_the_limit() {
        let dir = unique_temp_dir("archive-limit");
        let path = dir.join("server.log");
        let mut writer = SizeRotatingWriter::new(path.clone(), 50, 1).unwrap();

        write_bytes(&mut writer, 40, b'a');
        write_bytes(&mut writer, 40, b'b');
        write_bytes(&mut writer, 40, b'c');
        write_bytes(&mut writer, 40, b'd');

        assert!(archive_path(&path, 1).exists());
        assert!(!archive_path(&path, 2).exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);

        fs::remove_dir_all(&dir).ok();
    }

    /// Renders an explicit instant with the journal format description.
    ///
    /// The timer renders the current instant only, so pinning the rendering of
    /// known instants — the Unix epoch above all — needs the same format
    /// description the timer carries.
    fn render_journal_timestamp(instant: OffsetDateTime) -> String {
        let format = format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        );
        instant
            .format(format)
            .expect("the journal format description is valid")
    }

    /// The journal format renders UTC instants as RFC-3339 with exactly three
    /// subsecond digits, whatever the subsecond of the instant happens to be.
    #[test]
    fn journal_format_renders_rfc3339_with_millisecond_precision() {
        assert_eq!(
            render_journal_timestamp(OffsetDateTime::UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z"
        );

        let sample = OffsetDateTime::from(UNIX_EPOCH + Duration::from_millis(1_609_459_200_123));
        assert_eq!(render_journal_timestamp(sample), "2021-01-01T00:00:00.123Z");

        let now = render_journal_timestamp(OffsetDateTime::now_utc());
        assert_eq!(now.len(), 24);
        assert!(now.ends_with('Z'));
        assert_eq!(now.as_bytes()[10], b'T');
        assert_eq!(now.as_bytes()[19], b'.');
    }

    /// The timer installed on the subscriber — never a test-local formatter —
    /// writes the record's timestamp field in that same shape.
    #[test]
    fn journal_record_timestamp_uses_the_millisecond_format() {
        let sink = TestMakeWriter::default();
        let subscriber = build_subscriber(sink.clone(), EnvFilter::new("info"));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("timestamp shape");
        });

        let output = sink.contents();
        let line = output.lines().next().expect("one record");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid json");
        let timestamp = value["timestamp"].as_str().expect("timestamp string");
        assert_eq!(timestamp.len(), 24, "unexpected timestamp: {timestamp}");
        assert!(
            timestamp.ends_with('Z'),
            "timestamp is not UTC: {timestamp}"
        );
        assert_eq!(timestamp.as_bytes()[10], b'T');
        assert_eq!(timestamp.as_bytes()[19], b'.');
        assert!(
            timestamp.bytes().take(4).all(|byte| byte.is_ascii_digit()),
            "timestamp year is not numeric: {timestamp}"
        );
    }

    #[test]
    fn json_subscriber_emits_parseable_single_line_records() {
        let sink = TestMakeWriter::default();
        let subscriber = build_subscriber(sink.clone(), EnvFilter::new("info"));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(operation_id = 42, "hello world");
        });

        let output = sink.contents();
        let line = output.lines().next().expect("one record");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid json");
        assert_eq!(value["level"], "INFO");
        assert_eq!(value["message"], "hello world");
        assert_eq!(value["operation_id"], 42);
        assert!(value["target"].as_str().is_some());
        let timestamp = value["timestamp"].as_str().expect("timestamp string");
        assert!(timestamp.ends_with('Z'));
    }

    #[test]
    fn panic_payload_message_extracts_string_and_str_payloads() {
        let owned: Box<dyn std::any::Any + Send> = Box::new(String::from("boom"));
        assert_eq!(panic_payload_message(owned.as_ref()), "boom");

        let literal: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_payload_message(literal.as_ref()), "boom");

        let other: Box<dyn std::any::Any + Send> = Box::new(7_u32);
        assert_eq!(
            panic_payload_message(other.as_ref()),
            "<non-string panic payload>"
        );
    }

    #[derive(Clone, Default)]
    struct TestMakeWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl TestMakeWriter {
        fn contents(&self) -> String {
            let bytes = self
                .buffer
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TestMakeWriter {
        type Writer = TestWriter;

        fn make_writer(&'a self) -> Self::Writer {
            TestWriter {
                buffer: Arc::clone(&self.buffer),
            }
        }
    }

    struct TestWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl io::Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut guard = self
                .buffer
                .lock()
                .map_err(|_| io::Error::other("log sink poisoned"))?;
            guard.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
