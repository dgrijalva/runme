//! Custom tracing layer that converts `tracing::Event`s into `LogEntry`s.
//!
//! When the TUI launches a task, it installs this layer so that `info!()`,
//! `error!()`, etc. inside task functions flow into the existing log pipeline
//! as `LogEntry` records pushed to an `OutputBuffer`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::execution::TaskId;
use crate::log::buffer::OutputBuffer;
use crate::log::{LogEntry, ParsedContent, SeqGen};

/// Per-task context attached to the per-task carrier span by the engine
/// (see `TaskExecution::spawn_body`). The global `LogEntryLayer` finds
/// this in the event's span scope and routes the event into the right
/// task's output buffer with the right source `TaskId`. Carried via
/// `tracing::Span::extensions()` so it propagates across `tokio::spawn`
/// boundaries whenever the spawned future is `.instrument(span)`'d
/// (use the `rnme::spawn!` macro for the common case).
#[derive(Clone)]
pub struct TaskTracingCtx {
    pub buffer: Arc<Mutex<OutputBuffer>>,
    pub source_label: String,
    pub source_id: TaskId,
}

/// A `tracing::Layer` that converts events into `LogEntry`s and pushes them
/// into the `OutputBuffer` selected by the nearest enclosing span carrying
/// a `TaskTracingCtx` in its extensions. When no such span is in scope
/// (events emitted outside any task body), the event is silently dropped.
///
/// The layer pulls its `SeqGen` from the `TaskTracingCtx`'s buffer so seqs
/// share the engine-global counter regardless of which engine generation
/// owns the task. The layer itself holds a fallback `SeqGen` used only
/// when the buffer is contended at event time.
pub struct LogEntryLayer {
    fallback_seq_gen: SeqGen,
}

impl LogEntryLayer {
    /// Create a layer with a fresh fallback `SeqGen`. Used by the globally-
    /// installed subscriber: at event time, the layer pulls the actual
    /// engine-global `SeqGen` from the per-task buffer reached via the
    /// `TaskTracingCtx` attached to the enclosing carrier span.
    pub fn new() -> Self {
        Self::with_seq_gen(SeqGen::new())
    }

    /// Create a layer with an explicit fallback `SeqGen` (for tests that
    /// want a known seq stream).
    pub fn with_seq_gen(seq_gen: SeqGen) -> Self {
        Self {
            fallback_seq_gen: seq_gen,
        }
    }
}

impl Default for LogEntryLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the per-task carrier span and attach `ctx` to its extensions so
/// the global `LogEntryLayer` can find it when events fire inside the
/// span's scope. Returns the span — the caller is responsible for
/// running the task body inside it via `Instrument::instrument`.
///
/// The span is created at INFO level with target `"rnme_task"`. The
/// engine adds a `rnme_task=info` directive to the env filter in
/// `install_global_tracing_subscriber` so this span is always enabled
/// regardless of the user's `RUST_LOG`.
///
/// If no `tracing_subscriber::Registry` is installed under the current
/// dispatch (e.g. a test with a custom subscriber that doesn't include
/// the registry), the attach is a silent no-op and events fired inside
/// the returned span will be dropped by `LogEntryLayer`.
pub fn attach_task_tracing_ctx(ctx: TaskTracingCtx) -> tracing::Span {
    let span = tracing::info_span!(target: "rnme_task", "task");
    span.with_subscriber(|(id, dispatch)| {
        if let Some(registry) = dispatch.downcast_ref::<tracing_subscriber::Registry>() {
            if let Some(span_ref) = registry.span(id) {
                span_ref.extensions_mut().insert(ctx);
            }
        }
    });
    span
}

/// Visitor that collects the formatted message and structured fields from a
/// tracing event.
struct FieldCollector {
    message: Option<String>,
    fields: HashMap<String, serde_json::Value>,
}

impl FieldCollector {
    fn new() -> Self {
        Self {
            message: None,
            fields: HashMap::new(),
        }
    }
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{:?}", value));
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(format!("{:?}", value)),
            );
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields.insert(
                field.name().to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .insert(field.name().to_string(), serde_json::json!(value));
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .insert(field.name().to_string(), serde_json::json!(value));
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .insert(field.name().to_string(), serde_json::json!(value));
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .insert(field.name().to_string(), serde_json::json!(value));
        }
    }
}

impl<S> Layer<S> for LogEntryLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        // Walk the event's span scope (innermost first) and find a span
        // whose extensions carry a `TaskTracingCtx`. The engine attaches
        // this in `spawn_body` to a per-task carrier span and runs the
        // body inside that span via `Instrument`; the `rnme::spawn!`
        // macro re-enters the same span in spawned children. If no span
        // in scope carries one, drop silently — this is the correct
        // behavior for engine bootstrap traces and tests using `tracing`
        // without an `Engine` wrapper.
        let Some(task_ctx) = ctx.event_scope(event).and_then(|scope| {
            scope.into_iter().find_map(|span| {
                span.extensions().get::<TaskTracingCtx>().cloned()
            })
        }) else {
            return;
        };

        let metadata = event.metadata();
        let level = match *metadata.level() {
            tracing::Level::ERROR => "error",
            tracing::Level::WARN => "warn",
            tracing::Level::INFO => "info",
            tracing::Level::DEBUG => "debug",
            tracing::Level::TRACE => "trace",
        };

        let mut collector = FieldCollector::new();
        event.record(&mut collector);

        let message = collector.message.clone().unwrap_or_default();

        let raw = if collector.fields.is_empty() {
            format!(
                "{} {}: {}",
                level.to_uppercase(),
                task_ctx.source_label,
                message
            )
        } else {
            let fields_str: Vec<String> = collector
                .fields
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect();
            format!(
                "{} {}: {} {{ {} }}",
                level.to_uppercase(),
                task_ctx.source_label,
                message,
                fields_str.join(", ")
            )
        };

        // Pull the engine-global SeqGen out of the per-task buffer so all
        // entries — including process output and println output — share one
        // monotonic counter. If the buffer is contended, fall back to the
        // layer's own SeqGen (which keeps stamping going at the cost of
        // local-only ordering for that one entry — vastly preferable to
        // pushing a seq=0 entry that would trip LogStore's debug_assert).
        let buffer = task_ctx.buffer.clone();
        let seq = match buffer.try_lock() {
            Ok(buf) => buf.seq_gen().next(),
            Err(_) => self.fallback_seq_gen.next(),
        };

        let entry = LogEntry::new(
            raw,
            ParsedContent::PlainText,
            task_ctx.source_id,
            seq,
            None,
            Some(level.to_string()),
            collector.message,
            collector.fields,
        );

        if let Ok(mut buf) = buffer.try_lock() {
            buf.push(entry);
        } else {
            tokio::spawn(async move {
                let mut buf = buffer.lock().await;
                buf.push(entry);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Instrument;
    use tracing_subscriber::layer::SubscriberExt;

    fn ctx(buf: Arc<Mutex<OutputBuffer>>, label: &str, id: TaskId) -> TaskTracingCtx {
        TaskTracingCtx {
            buffer: buf,
            source_label: label.to_string(),
            source_id: id,
        }
    }

    #[tokio::test]
    async fn test_tracing_layer_captures_info_with_fields() {
        let buffer = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let subscriber = tracing_subscriber::registry().with(LogEntryLayer::new());
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = attach_task_tracing_ctx(ctx(buffer.clone(), "task", TaskId(99)));
        async {
            tracing::info!(key = "val", "hello");
            tokio::task::yield_now().await;
        }
        .instrument(span)
        .await;

        let buf = buffer.lock().await;
        assert_eq!(buf.len(), 1);
        let entry = &buf.lines()[0];
        assert_eq!(entry.source, TaskId(99));
        assert_eq!(entry.level.as_deref(), Some("info"));
        assert_eq!(entry.message.as_deref(), Some("hello"));
        assert!(matches!(entry.parsed, ParsedContent::PlainText));
        // Seq is engine-global and monotonic; only assert it was stamped.
        assert!(entry.seq > 0);
        assert_eq!(
            entry.fields.get("key"),
            Some(&serde_json::Value::String("val".to_string()))
        );
    }

    #[tokio::test]
    async fn test_tracing_layer_multiple_levels() {
        // Share the SeqGen between the OutputBuffer and the layer so seqs are
        // strictly monotonic across emitted entries.
        let seq_gen = SeqGen::new();
        let buffer = Arc::new(Mutex::new(OutputBuffer::with_seq_gen(100, seq_gen.clone())));
        let subscriber =
            tracing_subscriber::registry().with(LogEntryLayer::with_seq_gen(seq_gen));
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = attach_task_tracing_ctx(ctx(buffer.clone(), "test_source", TaskId(100)));
        async {
            tracing::error!("err msg");
            tracing::warn!("warn msg");
            tracing::debug!("debug msg");
            tracing::trace!("trace msg");
            tokio::task::yield_now().await;
        }
        .instrument(span)
        .await;

        let buf = buffer.lock().await;
        assert_eq!(buf.len(), 4);
        let entries: Vec<_> = buf.lines().iter().collect();
        assert_eq!(entries[0].level.as_deref(), Some("error"));
        assert_eq!(entries[1].level.as_deref(), Some("warn"));
        assert_eq!(entries[2].level.as_deref(), Some("debug"));
        assert_eq!(entries[3].level.as_deref(), Some("trace"));
        // Strict monotonicity (engine-global seq).
        assert!(entries[0].seq > 0);
        assert!(entries[1].seq > entries[0].seq);
        assert!(entries[2].seq > entries[1].seq);
        assert!(entries[3].seq > entries[2].seq);
    }

    #[tokio::test]
    async fn test_tracing_layer_numeric_fields() {
        let buffer = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let subscriber = tracing_subscriber::registry().with(LogEntryLayer::new());
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = attach_task_tracing_ctx(ctx(buffer.clone(), "task", TaskId(99)));
        async {
            tracing::info!(count = 42_i64, ratio = 2.72_f64, active = true, "metrics");
            tokio::task::yield_now().await;
        }
        .instrument(span)
        .await;

        let buf = buffer.lock().await;
        assert_eq!(buf.len(), 1);
        let entry = &buf.lines()[0];
        assert_eq!(entry.fields.get("count"), Some(&serde_json::json!(42)));
        assert_eq!(entry.fields.get("ratio"), Some(&serde_json::json!(2.72)));
        assert_eq!(entry.fields.get("active"), Some(&serde_json::json!(true)));
    }

    #[tokio::test]
    async fn test_tracing_layer_raw_format() {
        let buffer = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let subscriber = tracing_subscriber::registry().with(LogEntryLayer::new());
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = attach_task_tracing_ctx(ctx(buffer.clone(), "task", TaskId(99)));
        async {
            tracing::info!("simple message");
            tokio::task::yield_now().await;
        }
        .instrument(span)
        .await;

        let buf = buffer.lock().await;
        let entry = &buf.lines()[0];
        assert!(entry.raw.contains("INFO"));
        assert!(entry.raw.contains("task"));
        assert!(entry.raw.contains("simple message"));
    }

    /// Events emitted from a `tokio::spawn`'d future inside a task body
    /// are attributed to the right `TaskId` when the spawned future is
    /// instrumented with the current span. This is the bug `rnme::spawn!`
    /// exists to prevent — without re-entering the span in the child
    /// task, the layer finds no `TaskTracingCtx` in scope and drops.
    #[tokio::test]
    async fn test_tracing_layer_propagates_to_spawned_future() {
        let buffer = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let subscriber = tracing_subscriber::registry().with(LogEntryLayer::new());
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = attach_task_tracing_ctx(ctx(buffer.clone(), "task", TaskId(7)));
        async {
            tracing::info!("from body");
            let handle = tokio::spawn(
                async {
                    tracing::info!("from child");
                }
                .instrument(tracing::Span::current()),
            );
            handle.await.unwrap();
        }
        .instrument(span)
        .await;

        let buf = buffer.lock().await;
        assert_eq!(buf.len(), 2, "body and child events both captured");
        assert!(
            buf.lines().iter().all(|e| e.source == TaskId(7)),
            "all events attributed to TaskId(7)",
        );
        let msgs: Vec<_> = buf.lines().iter().filter_map(|e| e.message.as_deref()).collect();
        assert!(msgs.contains(&"from body"));
        assert!(msgs.contains(&"from child"));
    }

    /// Counter-test: a plain `tokio::spawn` without `.instrument()` drops
    /// the span context, so the child's event is not routed to the task
    /// buffer. This documents the gap the `rnme::spawn!` macro closes.
    #[tokio::test]
    async fn test_tracing_layer_plain_spawn_drops_context() {
        let buffer = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let subscriber = tracing_subscriber::registry().with(LogEntryLayer::new());
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = attach_task_tracing_ctx(ctx(buffer.clone(), "task", TaskId(8)));
        async {
            tracing::info!("from body");
            let handle = tokio::spawn(async {
                tracing::info!("from orphan child");
            });
            handle.await.unwrap();
        }
        .instrument(span)
        .await;

        let buf = buffer.lock().await;
        assert_eq!(buf.len(), 1, "only the body event is captured");
        assert_eq!(buf.lines()[0].message.as_deref(), Some("from body"));
    }
}
