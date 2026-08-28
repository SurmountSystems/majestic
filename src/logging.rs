//! Console and systemd journal logging for `memex`.
//!
//! Default terminal: ERROR and INFO only (not WARN, DEBUG, or TRACE). The
//! systemd journal keeps the full log (TRACE) under syslog identifier
//! [`SYSLOG_IDENTIFIER`] (`memex`). If journald is missing, the terminal
//! still works and the journal layer is skipped.
//!
//! Console override order: `MEMEX_LOG`, then `RUST_LOG`, then `[log] filter`
//! from memex.toml. The values `info`, `error+info`, and `error,info` mean
//! that quiet terminal. Other values are a `tracing_subscriber` env filter
//! for the console only. The journal stays verbose.

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::layer::{Context, Filter, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

/// systemd `SYSLOG_IDENTIFIER`. `journalctl --user -t memex` and
/// `journalctl --user SYSLOG_IDENTIFIER=memex`.
pub const SYSLOG_IDENTIFIER: &str = "memex";

/// First permission-denied event may print on the terminal. Further events
/// stay in the journal until [`emit_permission_denied_summary`].
const CONSOLE_DENIED_CAP: usize = 1;

static PERMISSION_DENIED: AtomicUsize = AtomicUsize::new(0);
static LAST_PERMISSION_DENIED: AtomicUsize = AtomicUsize::new(0);

/// Whether the default terminal should print this level.
pub fn default_console_allows(level: Level) -> bool {
    matches!(level, Level::ERROR | Level::INFO)
}

/// `MEMEX_LOG` if set and non-empty, else `RUST_LOG` if set and non-empty.
pub fn env_console_directive() -> Option<String> {
    first_nonempty_env(&["MEMEX_LOG", "RUST_LOG"])
}

fn first_nonempty_env(keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    None
}

fn is_quiet_console_directive(directive: &str) -> bool {
    matches!(
        directive.trim().to_ascii_lowercase().as_str(),
        "" | "info" | "error+info" | "error,info"
    )
}

/// Console filter from `MEMEX_LOG` / `RUST_LOG` / `[log] filter`.
pub fn console_filter_from_directive(directive: Option<&str>) -> ConsoleFilter {
    match directive.map(str::trim).filter(|value| !value.is_empty()) {
        None => ConsoleFilter::Quiet,
        Some(value) if is_quiet_console_directive(value) => ConsoleFilter::Quiet,
        Some(value) => match EnvFilter::try_new(value) {
            Ok(filter) => ConsoleFilter::Env(Box::new(filter)),
            Err(_) => ConsoleFilter::Quiet,
        },
    }
}

/// Filter for the stderr layer.
#[derive(Debug)]
pub enum ConsoleFilter {
    /// ERROR and INFO only. Not WARN.
    Quiet,
    /// User override (`MEMEX_LOG`, `RUST_LOG`, or `[log] filter`).
    Env(Box<EnvFilter>),
}

impl ConsoleFilter {
    fn allows_level(&self, level: Level) -> bool {
        match self {
            Self::Quiet => default_console_allows(level),
            Self::Env(_) => true,
        }
    }
}

impl<S> Filter<S> for ConsoleFilter
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn enabled(&self, meta: &Metadata<'_>, cx: &Context<'_, S>) -> bool {
        match self {
            Self::Quiet => default_console_allows(*meta.level()),
            Self::Env(filter) => Filter::<S>::enabled(filter.as_ref(), meta, cx),
        }
    }

    fn event_enabled(&self, event: &Event<'_>, cx: &Context<'_, S>) -> bool {
        if console_field_is_false(event) {
            return false;
        }
        match self {
            Self::Quiet => self.allows_level(*event.metadata().level()),
            Self::Env(filter) => Filter::<S>::event_enabled(filter.as_ref(), event, cx),
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        match self {
            Self::Quiet => Some(LevelFilter::INFO),
            Self::Env(filter) => Filter::<S>::max_level_hint(filter.as_ref()),
        }
    }
}

fn console_field_is_false(event: &Event<'_>) -> bool {
    struct ConsoleFalse(bool);
    impl Visit for ConsoleFalse {
        fn record_bool(&mut self, field: &Field, value: bool) {
            if field.name() == "console" && !value {
                self.0 = true;
            }
        }

        fn record_debug(&mut self, _field: &Field, _value: &dyn fmt::Debug) {}
    }
    let mut visitor = ConsoleFalse(false);
    event.record(&mut visitor);
    visitor.0
}

/// Install the console (compact stderr) and journal layers.
///
/// `config_filter` is `[log] filter` after env overlays. `MEMEX_LOG` and
/// `RUST_LOG` win over that string.
pub fn init_from_directive(config_filter: Option<&str>) {
    let env = env_console_directive();
    let directive = env.as_deref().or(config_filter);
    let _ = try_init(directive);
}

/// Same as [`init_from_directive`] with no config string.
pub fn init() {
    init_from_directive(None);
}

fn try_init(directive: Option<&str>) -> Result<(), tracing_subscriber::util::TryInitError> {
    let console = tracing_subscriber::fmt::layer()
        .compact()
        .with_writer(io::stderr)
        .with_filter(console_filter_from_directive(directive));
    // Journal filter is TRACE so the combined subscriber hint stays TRACE.
    // A bare journal layer hints None, which would combine with the console
    // INFO hint and drop DEBUG/TRACE before the journal saw them.
    let journal = tracing_journald::layer().ok().map(|layer| {
        layer
            .with_syslog_identifier(SYSLOG_IDENTIFIER.to_owned())
            .with_filter(LevelFilter::TRACE)
    });
    tracing_subscriber::registry()
        .with(console)
        .with(journal)
        .try_init()
}

/// Reset the permission-denied counter (start of a home scan).
pub fn reset_permission_denied_count() {
    PERMISSION_DENIED.store(0, Ordering::Relaxed);
    LAST_PERMISSION_DENIED.store(0, Ordering::Relaxed);
}

/// Permission-denied events from the last finished home scan (after summary).
pub fn last_permission_denied_count() -> usize {
    LAST_PERMISSION_DENIED.load(Ordering::Relaxed)
}

/// Permission denied is ERROR. Further events after the first stay in the
/// journal (`console = false`) so a skip-list miss does not flood stderr.
pub fn log_permission_denied(path: &Path, err: &io::Error) {
    if err.kind() != io::ErrorKind::PermissionDenied {
        log_io_on_walk(path, err);
        return;
    }
    emit_permission_denied(path, err);
}

fn emit_permission_denied(path: &Path, err: &io::Error) {
    let n = PERMISSION_DENIED.fetch_add(1, Ordering::Relaxed) + 1;
    let show_on_console = n <= CONSOLE_DENIED_CAP;
    tracing::error!(
        path = %path.display(),
        error = %err,
        console = show_on_console,
        "permission denied while reading a path"
    );
}

/// If many paths were denied, print one ERROR with a count and point at
/// `journalctl`. No-op when the count is 0 or 1 (the first path already
/// printed).
pub fn emit_permission_denied_summary() {
    let n = PERMISSION_DENIED.swap(0, Ordering::Relaxed);
    LAST_PERMISSION_DENIED.store(n, Ordering::Relaxed);
    if n > CONSOLE_DENIED_CAP {
        tracing::error!(
            count = n,
            "permission denied on {n} paths; see journalctl --user SYSLOG_IDENTIFIER=memex"
        );
    }
}

/// Expected skip (system trash, sandbox-blocked dir, empty sqlite). DEBUG.
pub fn log_expected_skip(path: &Path, reason: &str) {
    tracing::debug!(
        path = %path.display(),
        reason,
        "skipped a path that is not a memex source"
    );
}

/// Walk I/O: permission denied is ERROR; not found is TRACE; other is DEBUG.
pub fn log_io_on_walk(path: &Path, err: &io::Error) {
    match err.kind() {
        io::ErrorKind::PermissionDenied => emit_permission_denied(path, err),
        io::ErrorKind::NotFound => {
            tracing::trace!(
                path = %path.display(),
                error = %err,
                "path not found while walking"
            );
        }
        _ => {
            tracing::debug!(
                path = %path.display(),
                error = %err,
                "skipped an unreadable path"
            );
        }
    }
}

/// [`crate::Error`] from a walk or zip open: Io permission denied is ERROR.
pub fn log_error_on_path(path: &Path, err: &crate::Error) {
    match err {
        crate::Error::Io(io_err) => log_io_on_walk(path, io_err),
        other => {
            tracing::debug!(
                path = %path.display(),
                error = %other,
                "skipped an unreadable path"
            );
        }
    }
}

/// Unreadable mmap archive during search.
pub fn log_unreadable_archive(path: &Path, err: &crate::Error) {
    match err {
        crate::Error::Io(io_err) if io_err.kind() == io::ErrorKind::PermissionDenied => {
            log_permission_denied(path, io_err);
        }
        other => {
            tracing::warn!(
                archive = %path.display(),
                error = %other,
                "skipped an unreadable archive"
            );
        }
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
struct CaptureLayer {
    events: std::sync::Arc<std::sync::Mutex<Vec<(Level, String)>>>,
}

#[cfg(test)]
struct MessageVisitor {
    message: String,
}

#[cfg(test)]
impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" && self.message.is_empty() {
            self.message = format!("{value:?}");
        }
    }
}

#[cfg(test)]
impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor {
            message: String::new(),
        };
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("capture mutex")
            .push((*event.metadata().level(), visitor.message));
    }
}

/// Capture tracing events from `f` on this thread. Used by ingest skip tests.
#[cfg(test)]
pub(crate) fn capture_events(f: impl FnOnce()) -> Vec<(Level, String)> {
    use tracing_subscriber::layer::SubscriberExt;
    let cap = CaptureLayer::default();
    let subscriber = tracing_subscriber::registry().with(cap.clone());
    tracing::subscriber::with_default(subscriber, f);
    cap.events.lock().expect("capture mutex").clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(f: impl FnOnce()) -> Vec<(Level, String)> {
        capture_events(f)
    }

    fn permission_denied_error() -> io::Error {
        io::Error::from_raw_os_error(13)
    }

    #[test]
    fn permission_denied_logs_at_error() {
        reset_permission_denied_count();
        let err = permission_denied_error();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let events = capture(|| {
            log_permission_denied(Path::new("/denied/path"), &err);
        });
        assert!(
            events.iter().any(|(level, message)| {
                *level == Level::ERROR && message.contains("permission denied")
            }),
            "permission denied must log at error, got {events:?}"
        );
        assert!(
            !events.iter().any(|(level, _)| *level == Level::INFO),
            "permission denied must not log at info, got {events:?}"
        );
    }

    #[test]
    fn default_console_filter_rejects_debug_and_warn() {
        assert!(default_console_allows(Level::ERROR));
        assert!(default_console_allows(Level::INFO));
        assert!(!default_console_allows(Level::WARN));
        assert!(!default_console_allows(Level::DEBUG));
        assert!(!default_console_allows(Level::TRACE));
        match console_filter_from_directive(None) {
            ConsoleFilter::Quiet => {}
            ConsoleFilter::Env(_) => panic!("default console filter must be quiet"),
        }
        match console_filter_from_directive(Some("info")) {
            ConsoleFilter::Quiet => {}
            ConsoleFilter::Env(_) => {
                panic!("config [log] filter = info must stay the quiet terminal")
            }
        }
        match console_filter_from_directive(Some("debug")) {
            ConsoleFilter::Env(_) => {}
            ConsoleFilter::Quiet => panic!("debug override must not be the quiet terminal"),
        }
    }

    #[test]
    fn expected_skip_of_trash_is_not_info() {
        let events = capture(|| {
            log_expected_skip(Path::new("/home/u/.local/share/Trash"), "system trash");
        });
        assert!(
            events.iter().any(|(level, _)| *level == Level::DEBUG),
            "expected trash skip must log at debug, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|(level, _)| *level == Level::INFO || *level == Level::ERROR),
            "expected trash skip must not log at info or error, got {events:?}"
        );
    }

    #[test]
    fn syslog_identifier_is_memex() {
        assert_eq!(SYSLOG_IDENTIFIER, "memex");
    }

    #[test]
    fn summary_records_last_permission_denied_count() {
        reset_permission_denied_count();
        emit_permission_denied_summary();
        assert_eq!(last_permission_denied_count(), 0);
    }

    #[test]
    fn init_does_not_panic_when_called_twice() {
        init_from_directive(Some("info"));
        init_from_directive(Some("info"));
    }
}
