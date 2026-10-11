//! Function log lines: `tracing` events emitted while a function runs are captured per execution.
//!
//! Inside a function, log with the `tracing` macros re-exported here:
//!
//! ```ignore
//! use ferrix::log;
//!
//! #[mutation]
//! pub async fn add(ctx: &MutationCtx, text: String) -> Result<Id<Todo>> {
//!     log::info!(len = text.len(), "adding a todo");      // captured as "[INFO] adding a todo len=5"
//!     ctx.db.insert(Todo { text, done: false }).await
//! }
//! ```
//!
//! They are the plain `tracing` macros, so `tracing::info!` works just as well, and the events still reach whatever
//! subscriber prints logs (the CLI installs one that writes to stdout). The prelude deliberately does not glob-export
//! `log` or the macros: a glob-imported `log` (or `info!`, `warn!`, ...) is ambiguous (E0659) with the `log` crate and
//! with `#[macro_use] extern crate log`, so `use ferrix::log;` is the one explicit import to add.
//!
//! How capture works: the runtime runs every function inside a task-local capture buffer ([`capture`]); the
//! [`layer`] (installed globally) appends each event it sees while a buffer is active, formatted as
//! `"[LEVEL] message key=value ..."`. Nested `ctx.run_*` calls get their own buffer, whose lines are also appended to
//! the caller's. Lines are always stored in the execution log; they are sent to clients (`logLines`) only in `--dev`.
//!
//! Events are captured at `DEBUG` and above. Events from a few chatty infrastructure crates (hyper, h2, reqwest,
//! rustls, tokio, tower, ...) are captured only at `WARN` and above, so an action making HTTP calls doesn't fill its
//! log with connection-pool chatter. Events from tasks the function spawns itself (`tokio::spawn`) are not captured.
//!
//! If the application installs its own global subscriber, it should add [`layer()`] to it (before opening the
//! [`App`](crate::App)). If no global subscriber is set when an `App` opens, ferrix installs one containing only the
//! capture layer.

use std::fmt::Write as _;
use std::future::Future;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, Filter, SubscriberExt as _};

pub use tracing::{debug, error, info, trace, warn};

/// Lines kept per execution; further lines are counted and summarized in one final line.
pub const MAX_LINES: usize = 256;
/// Bytes kept per line (longer lines are cut at a char boundary and end with `…`).
pub const MAX_LINE_BYTES: usize = 4096;

/// Targets whose events are only captured at WARN and above.
const NOISY_TARGETS: &[&str] =
    &["hyper", "hyper_util", "h2", "reqwest", "rustls", "tokio", "tower", "mio", "want", "tungstenite", "axum"];

tokio::task_local! {
    static CAPTURE: Arc<Buffer>;
}

#[derive(Default)]
struct Buffer {
    inner: Mutex<BufferInner>,
}

#[derive(Default)]
struct BufferInner {
    lines: Vec<String>,
    dropped: usize,
}

impl Buffer {
    fn push(&self, line: String) {
        let mut b = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if b.lines.len() < MAX_LINES {
            b.lines.push(line);
        } else {
            b.dropped += 1;
        }
    }

    fn extend(&self, lines: &[String]) {
        for l in lines {
            self.push(l.clone());
        }
    }

    fn take(&self) -> Vec<String> {
        let mut b = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut lines = std::mem::take(&mut b.lines);
        if b.dropped > 0 {
            lines.push(format!("[WARN] {} more log line(s) dropped (limit {MAX_LINES} per execution)", b.dropped));
            b.dropped = 0;
        }
        lines
    }
}

/// Run `fut` with a fresh capture buffer and return its output with the lines logged while it ran. If a capture is
/// already active (a nested `ctx.run_*` inside an action), the lines are also appended to the enclosing buffer.
pub(crate) async fn capture<F: Future>(fut: F) -> (F::Output, Vec<String>) {
    let parent = CAPTURE.try_with(Arc::clone).ok();
    let buffer = Arc::new(Buffer::default());
    let out = CAPTURE.scope(buffer.clone(), fut).await;
    let lines = buffer.take();
    if let Some(parent) = parent {
        parent.extend(&lines);
    }
    (out, lines)
}

/// Run `fut` with a fresh capture buffer and drop the lines it logs: they reach neither an enclosing buffer nor the
/// execution log (other subscribers, such as stdout, still see them). For reruns of a query (`crate::determinism`).
pub(crate) async fn discard<F: Future>(fut: F) -> F::Output {
    CAPTURE.scope(Arc::new(Buffer::default()), fut).await
}

/// The `logLines` to send to a client: all of them in `--dev`, none otherwise (they may contain data the client
/// must not see). The execution log always keeps them.
pub(crate) fn for_client(dev: bool, lines: Vec<String>) -> Vec<String> {
    if dev { lines } else { Vec::new() }
}

fn capturing() -> bool {
    CAPTURE.try_with(|_| ()).is_ok()
}

/// The capture layer, with its own per-layer filter (it never affects what other layers see). Add it to your
/// subscriber if you install one yourself:
///
/// ```ignore
/// use tracing_subscriber::prelude::*;
/// tracing_subscriber::registry()
///     .with(tracing_subscriber::fmt::layer().with_filter(tracing_subscriber::EnvFilter::new("info")))
///     .with(ferrix::log::layer())
///     .init();
/// ```
pub fn layer<S>() -> impl Layer<S> + Send + Sync + 'static
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    CaptureLayer.with_filter(CaptureFilter)
}

/// How the server's own log lines are written (`--log-format`, `FERRIX_LOG_FORMAT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// One JSON object per line: `timestamp` (RFC 3339, UTC), `level`, `target`, `fields` (`message` and the
    /// event's key-values), `span` (the innermost span, with its fields) and `spans` (every enclosing span, outermost
    /// first). For log shippers (Loki, Vector, CloudWatch, journald).
    Json,
    /// Human-readable, one line per event (`tracing`'s full format; colors on a terminal).
    Pretty,
    /// Like `pretty`, shorter: span names without their fields.
    Compact,
}

impl LogFormat {
    /// The default when none is given: `pretty` when stdout is a terminal, `json` otherwise (systemd, Docker,
    /// files, pipes).
    pub fn default_for_stdout() -> LogFormat {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() { LogFormat::Pretty } else { LogFormat::Json }
    }
}

/// The subscriber `ferrix serve` installs: `fmt` output to `writer` in `format`, filtered by `filter` (`RUST_LOG`),
/// plus the capture layer, each with its own filter so neither limits the other (function log lines are captured
/// the same way whatever the output format).
pub(crate) fn cli_subscriber<W>(
    filter: tracing_subscriber::EnvFilter,
    writer: W,
    format: LogFormat,
) -> impl Subscriber + Send + Sync + 'static
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::registry::Registry;
    let output: Box<dyn Layer<Registry> + Send + Sync> = match format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_ansi(false)
            .with_current_span(true)
            .with_span_list(true)
            .with_writer(writer)
            .boxed(),
        LogFormat::Pretty => tracing_subscriber::fmt::layer().with_writer(writer).boxed(),
        LogFormat::Compact => tracing_subscriber::fmt::layer().compact().with_writer(writer).boxed(),
    };
    tracing_subscriber::registry().with(output.with_filter(filter)).with(layer())
}

/// Install a global subscriber containing only the capture layer, unless a global subscriber is already set.
/// Called when an [`App`](crate::App) opens, so capture works in tests and embedders that set up no tracing at all.
///
/// Not `tracing::dispatcher::has_been_set()`: that also turns true once any *scoped* dispatcher has existed (e.g.
/// `tracing::subscriber::with_default`, even after it ended), which would leave capture silently uninstalled.
/// Setting the global default fails exactly when a global subscriber is already set, which is the right test.
pub(crate) fn ensure_installed() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer())).is_ok() {
            tracing::debug!("ferrix: installed the function log capture layer as the global subscriber");
        }
    });
}

struct CaptureFilter;

impl CaptureFilter {
    fn level_ok(meta: &Metadata<'_>) -> bool {
        let level = *meta.level();
        if level > Level::DEBUG {
            return false; // TRACE
        }
        let target = meta.target();
        let noisy = NOISY_TARGETS
            .iter()
            .any(|t| target.strip_prefix(t).is_some_and(|rest| rest.is_empty() || rest.starts_with("::")));
        !noisy || level <= Level::WARN
    }
}

impl<S> Filter<S> for CaptureFilter {
    fn enabled(&self, meta: &Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        meta.is_event() && Self::level_ok(meta) && capturing()
    }

    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        if meta.is_event() && Self::level_ok(meta) { Interest::sometimes() } else { Interest::never() }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::DEBUG)
    }
}

struct CaptureLayer;

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Format only if someone is listening (the filter already checked, but the buffer may be gone by now).
        let _ = CAPTURE.try_with(|buffer| buffer.push(format_event(event)));
    }
}

/// `"[INFO] message key=value key2=value2"`.
fn format_event(event: &Event<'_>) -> String {
    let mut v = LineVisitor { message: String::new(), fields: String::new() };
    event.record(&mut v);
    let mut line = format!("[{}]", event.metadata().level());
    if !v.message.is_empty() {
        line.push(' ');
        line.push_str(&v.message);
    }
    line.push_str(&v.fields);
    truncate_line(line)
}

fn truncate_line(mut line: String) -> String {
    if line.len() > MAX_LINE_BYTES {
        let mut cut = MAX_LINE_BYTES;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push('…');
    }
    line
}

struct LineVisitor {
    message: String,
    fields: String,
}

impl Visit for LineVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={}", field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_nested_and_formats() {
        ensure_installed();
        let ((), outer) = capture(async {
            info!(n = 3, name = "x", "hello {}", "world");
            let ((), inner) = capture(async {
                warn!("inner");
            })
            .await;
            assert_eq!(inner, vec!["[WARN] inner".to_string()]);
            tracing::trace!("not captured");
            tracing::debug!(target: "hyper::client", "noise");
            tracing::warn!(target: "hyper::client", "kept");
            tracing::debug!(target: "hyperion", "not noisy");
        })
        .await;
        assert_eq!(
            outer,
            vec![
                "[INFO] hello world n=3 name=x".to_string(),
                "[WARN] inner".to_string(),
                "[WARN] kept".to_string(),
                "[DEBUG] not noisy".to_string(),
            ]
        );
        // Outside a capture nothing is recorded (and nothing panics).
        info!("outside");
    }

    /// The CLI's subscriber: stdout keeps its own filter (INFO here) while capture still sees DEBUG.
    #[test]
    fn cli_subscriber_prints_and_captures() {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let out = Buf::default();
        let writer = out.clone();
        let subscriber =
            cli_subscriber(tracing_subscriber::EnvFilter::new("info"), move || writer.clone(), LogFormat::Pretty);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let ((), lines) = tracing::subscriber::with_default(subscriber, || {
            rt.block_on(capture(async {
                info!(target: "cli_test", "to both");
                debug!(target: "cli_test", "capture only");
            }))
        });
        assert_eq!(lines, vec!["[INFO] to both", "[DEBUG] capture only"]);
        let printed = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert!(printed.contains("to both") && !printed.contains("capture only"), "{printed}");
    }

    /// `--log-format json`: every line is a JSON object with the timestamp, level, target, fields and span context,
    /// and function log lines are still captured.
    #[test]
    fn json_lines() {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let out = Buf::default();
        let writer = out.clone();
        let subscriber =
            cli_subscriber(tracing_subscriber::EnvFilter::new("info"), move || writer.clone(), LogFormat::Json);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let ((), lines) = tracing::subscriber::with_default(subscriber, || {
            info!(target: "json_test", "outside");
            rt.block_on(capture(async {
                let span = tracing::info_span!("request", path = "/api/x", n = 3);
                let _g = span.enter();
                warn!(target: "json_test", user = "ann", count = 2, "quote \" and\nnewline");
                debug!(target: "json_test", "capture only");
            }))
        });
        assert_eq!(lines, vec!["[WARN] quote \" and\nnewline user=ann count=2", "[DEBUG] capture only"]);
        let printed = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let events: Vec<serde_json::Value> = printed
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
            .collect();
        assert_eq!(events.len(), 2, "one line per event, newlines escaped: {printed}");
        let first = &events[0];
        assert_eq!((first["level"].as_str(), first["target"].as_str()), (Some("INFO"), Some("json_test")));
        assert_eq!(first["fields"]["message"], "outside");
        assert!(chrono::DateTime::parse_from_rfc3339(first["timestamp"].as_str().unwrap()).is_ok(), "{first}");
        let second = &events[1];
        assert_eq!(second["level"], "WARN");
        assert_eq!(second["fields"]["message"], "quote \" and\nnewline");
        assert_eq!((second["fields"]["user"].as_str(), second["fields"]["count"].as_i64()), (Some("ann"), Some(2)));
        assert_eq!(
            (second["span"]["name"].as_str(), second["span"]["path"].as_str()),
            (Some("request"), Some("/api/x"))
        );
        assert_eq!(second["spans"][0]["n"], 3);
    }

    #[tokio::test]
    async fn limits() {
        ensure_installed();
        let ((), lines) = capture(async {
            for i in 0..MAX_LINES + 5 {
                info!("line {i}");
            }
            info!("{}", "é".repeat(MAX_LINE_BYTES));
        })
        .await;
        assert_eq!(lines.len(), MAX_LINES + 1);
        assert!(lines.last().unwrap().contains("6 more log line(s) dropped"), "{:?}", lines.last());
        let long = truncate_line(format!("[INFO] {}", "é".repeat(MAX_LINE_BYTES)));
        assert!(long.len() <= MAX_LINE_BYTES + '…'.len_utf8() && long.ends_with('…'));
    }
}
