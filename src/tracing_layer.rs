//! Custom tracing layer that converts `tracing::Event`s into `LogEntry`s.
//!
//! When the TUI launches a task, it installs this layer so that `info!()`,
//! `error!()`, etc. inside task functions flow into the existing log pipeline
//! as `LogEntry` records pushed to an `OutputBuffer`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use crate::log::buffer::OutputBuffer;
use crate::log::{LogEntry, ParsedContent};

/// A `tracing::Layer` that converts events into `LogEntry`s and pushes them
/// into an `OutputBuffer`.
pub struct LogEntryLayer {
    buffer: Arc<Mutex<OutputBuffer>>,
    source: String,
    seq: AtomicU64,
}

impl LogEntryLayer {
    /// Create a new layer that pushes entries into the given buffer.
    ///
    /// `source` is the value written to `LogEntry.source` (typically `"task"`).
    pub fn new(buffer: Arc<Mutex<OutputBuffer>>, source: impl Into<String>) -> Self {
        Self {
            buffer,
            source: source.into(),
            seq: AtomicU64::new(0),
        }
    }
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

impl<S: Subscriber> Layer<S> for LogEntryLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();

        // Map tracing level to string
        let level = match *metadata.level() {
            tracing::Level::ERROR => "error",
            tracing::Level::WARN => "warn",
            tracing::Level::INFO => "info",
            tracing::Level::DEBUG => "debug",
            tracing::Level::TRACE => "trace",
        };

        // Collect message and fields
        let mut collector = FieldCollector::new();
        event.record(&mut collector);

        let message = collector.message.clone().unwrap_or_default();

        // Build a reasonable raw representation
        let raw = if collector.fields.is_empty() {
            format!("{} {}: {}", level.to_uppercase(), self.source, message)
        } else {
            let fields_str: Vec<String> = collector
                .fields
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect();
            format!(
                "{} {}: {} {{ {} }}",
                level.to_uppercase(),
                self.source,
                message,
                fields_str.join(", ")
            )
        };

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);

        let entry = LogEntry::new(
            raw,
            ParsedContent::PlainText,
            self.source.clone(),
            seq,
            None,
            Some(level.to_string()),
            collector.message,
            collector.fields,
        );

        // Push to buffer. We need to block on the async mutex since
        // tracing's Layer::on_event is synchronous. Use try_lock to avoid
        // blocking the tracing internals; if the lock is contended we
        // spawn a task to push asynchronously.
        let buffer = self.buffer.clone();
        if let Ok(mut buf) = buffer.try_lock() {
            buf.push(entry);
        } else {
            // Lock contended; push asynchronously to avoid blocking
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
    use tracing_subscriber::layer::SubscriberExt;

    #[tokio::test]
    async fn test_tracing_layer_captures_info_with_fields() {
        let buffer = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let layer = LogEntryLayer::new(buffer.clone(), "task");

        // Install as a scoped subscriber so we don't pollute other tests
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(key = "val", "hello");

        // Give async push a moment if it went through the spawn path
        tokio::task::yield_now().await;

        let buf = buffer.lock().await;
        assert_eq!(buf.len(), 1, "expected exactly one log entry");

        let entry = &buf.lines()[0];
        assert_eq!(entry.source, "task");
        assert_eq!(entry.level.as_deref(), Some("info"));
        assert_eq!(entry.message.as_deref(), Some("hello"));
        assert!(matches!(entry.parsed, ParsedContent::PlainText));
        assert_eq!(entry.seq, 0);

        // Check the structured field
        assert_eq!(
            entry.fields.get("key"),
            Some(&serde_json::Value::String("val".to_string())),
            "expected field key='val'"
        );
    }

    #[tokio::test]
    async fn test_tracing_layer_multiple_levels() {
        let buffer = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let layer = LogEntryLayer::new(buffer.clone(), "test_source");

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::error!("err msg");
        tracing::warn!("warn msg");
        tracing::debug!("debug msg");
        tracing::trace!("trace msg");

        tokio::task::yield_now().await;

        let buf = buffer.lock().await;
        assert_eq!(buf.len(), 4);

        let entries: Vec<_> = buf.lines().iter().collect();
        assert_eq!(entries[0].level.as_deref(), Some("error"));
        assert_eq!(entries[1].level.as_deref(), Some("warn"));
        assert_eq!(entries[2].level.as_deref(), Some("debug"));
        assert_eq!(entries[3].level.as_deref(), Some("trace"));

        // Verify monotonic seq
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(entries[2].seq, 2);
        assert_eq!(entries[3].seq, 3);
    }

    #[tokio::test]
    async fn test_tracing_layer_numeric_fields() {
        let buffer = Arc::new(Mutex::new(OutputBuffer::new(100)));
        let layer = LogEntryLayer::new(buffer.clone(), "task");

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(count = 42_i64, ratio = 2.72_f64, active = true, "metrics");

        tokio::task::yield_now().await;

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
        let layer = LogEntryLayer::new(buffer.clone(), "task");

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!("simple message");

        tokio::task::yield_now().await;

        let buf = buffer.lock().await;
        let entry = &buf.lines()[0];

        // Raw should contain the level, source, and message
        assert!(entry.raw.contains("INFO"));
        assert!(entry.raw.contains("task"));
        assert!(entry.raw.contains("simple message"));
    }
}
