use crate::{connection::LogSettings, sql_str::SqlStr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use futures_core::Stream;
use pin_project_lite::pin_project;
use tracing::Span;

// Yes these look silly. `tracing` doesn't currently support dynamic levels
// https://github.com/tokio-rs/tracing/issues/372
#[doc(hidden)]
#[macro_export]
macro_rules! private_tracing_dynamic_enabled {
    (target: $target:expr, $level:expr) => {{
        use ::tracing::Level;

        match $level {
            Level::ERROR => ::tracing::enabled!(target: $target, Level::ERROR),
            Level::WARN => ::tracing::enabled!(target: $target, Level::WARN),
            Level::INFO => ::tracing::enabled!(target: $target, Level::INFO),
            Level::DEBUG => ::tracing::enabled!(target: $target, Level::DEBUG),
            Level::TRACE => ::tracing::enabled!(target: $target, Level::TRACE),
        }
    }};
    ($level:expr) => {{
        $crate::private_tracing_dynamic_enabled!(target: module_path!(), $level)
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! private_tracing_dynamic_event {
    (target: $target:expr, $level:expr, $($args:tt)*) => {{
        use ::tracing::Level;

        match $level {
            Level::ERROR => ::tracing::event!(target: $target, Level::ERROR, $($args)*),
            Level::WARN => ::tracing::event!(target: $target, Level::WARN, $($args)*),
            Level::INFO => ::tracing::event!(target: $target, Level::INFO, $($args)*),
            Level::DEBUG => ::tracing::event!(target: $target, Level::DEBUG, $($args)*),
            Level::TRACE => ::tracing::event!(target: $target, Level::TRACE, $($args)*),
        }
    }};
}

#[doc(hidden)]
pub fn private_level_filter_to_levels(
    filter: log::LevelFilter,
) -> Option<(tracing::Level, log::Level)> {
    let tracing_level = match filter {
        log::LevelFilter::Error => Some(tracing::Level::ERROR),
        log::LevelFilter::Warn => Some(tracing::Level::WARN),
        log::LevelFilter::Info => Some(tracing::Level::INFO),
        log::LevelFilter::Debug => Some(tracing::Level::DEBUG),
        log::LevelFilter::Trace => Some(tracing::Level::TRACE),
        log::LevelFilter::Off => None,
    };

    tracing_level.zip(filter.to_level())
}

pub(crate) fn private_level_filter_to_trace_level(
    filter: log::LevelFilter,
) -> Option<tracing::Level> {
    private_level_filter_to_levels(filter).map(|(level, _)| level)
}

pub struct QueryLogger {
    sql: SqlStr,
    rows_returned: u64,
    rows_affected: u64,
    start: Instant,
    settings: LogSettings,
    span: Span,
}

impl QueryLogger {
    pub fn new(sql: SqlStr, settings: LogSettings) -> Self {
        // Hardcoded INFO level per maintainer review of #3313: libraries should pick a
        // level and let consumers filter via `EnvFilter`. Field names follow the OTel
        // database span semantic conventions
        // (https://opentelemetry.io/docs/specs/semconv/database/database-spans/).
        // `otel.kind = "client"` is the magic field that `tracing-opentelemetry` reads
        // to set the exported `SpanKind`. `db.system.name` is declared empty here and
        // filled in by drivers via `with_db_system_name`, so adding the field doesn't
        // force a signature break on `QueryLogger::new`.
        let summary = parse_query_summary(sql.as_str());
        let operation = summary
            .split_whitespace()
            .next()
            .map(str::to_owned)
            .unwrap_or_default();
        let span = tracing::info_span!(
            target: "sqlx::query",
            "db.query",
            "db.system.name" = tracing::field::Empty,
            "db.operation.name" = operation,
            "db.query.summary" = summary,
            "db.query.text" = sql.as_str(),
            "db.response.returned_rows" = tracing::field::Empty,
            "db.response.affected_rows" = tracing::field::Empty,
            "otel.kind" = "client",
        );

        Self {
            sql,
            rows_returned: 0,
            rows_affected: 0,
            start: Instant::now(),
            settings,
            span,
        }
    }

    /// Records the OTel `db.system.name` attribute on the query span.
    ///
    /// Drivers should call this with their canonical OTel system identifier
    /// (`"postgresql"`, `"mysql"`, `"sqlite"`, etc. — see the OTel database span
    /// semantic conventions). Separate from `new` so adding the field doesn't break
    /// callers that construct `QueryLogger` directly.
    pub fn with_db_system_name(self, name: &'static str) -> Self {
        self.span.record("db.system.name", name);
        self
    }

    pub fn increment_rows_returned(&mut self) {
        self.rows_returned += 1;
    }

    pub fn increase_rows_affected(&mut self, n: u64) {
        self.rows_affected += n;
    }

    pub fn sql(&self) -> &SqlStr {
        &self.sql
    }

    /// Clone the span attached to this query.
    ///
    /// Use with [`InstrumentedStream`] (or `Future::instrument` for plain futures) to
    /// attribute child events emitted during query execution to the query's span. The
    /// `Span` is `Send`; never store an `EnteredSpan` here (see #3176).
    pub fn span(&self) -> Span {
        self.span.clone()
    }

    pub fn finish(&self) {
        let elapsed = self.start.elapsed();

        // Record the per-query result counts on the span before it closes so OTel
        // exporters see them as span attributes.
        self.span
            .record("db.response.returned_rows", self.rows_returned);
        self.span
            .record("db.response.affected_rows", self.rows_affected);

        let was_slow = elapsed >= self.settings.slow_statements_duration;

        let lvl = if was_slow {
            self.settings.slow_statements_level
        } else {
            self.settings.statements_level
        };

        if let Some((tracing_level, log_level)) = private_level_filter_to_levels(lvl) {
            // The enabled level could be set from either tracing world or log world, so check both
            // to see if logging should be enabled for our level
            let log_is_enabled = log::log_enabled!(target: "sqlx::query", log_level)
                || private_tracing_dynamic_enabled!(target: "sqlx::query", tracing_level);
            if log_is_enabled {
                let mut summary = parse_query_summary(self.sql.as_str());

                let sql = if summary != self.sql.as_str() {
                    summary.push_str(" …");
                    format!("\n\n{}\n", self.sql.as_str())
                } else {
                    String::new()
                };

                // Emit the existing close-time event inside the query span so consumers
                // see both the span (for OTel correlation) and the event (for the
                // backwards-compatible `rows_affected`/`elapsed_secs` fields).
                self.span.in_scope(|| {
                    if was_slow {
                        private_tracing_dynamic_event!(
                            target: "sqlx::query",
                            tracing_level,
                            summary,
                            db.statement = sql,
                            rows_affected = self.rows_affected,
                            rows_returned = self.rows_returned,
                            // Human-friendly - includes units (usually ms). Also kept for backward compatibility
                            ?elapsed,
                            // Search friendly - numeric
                            elapsed_secs = elapsed.as_secs_f64(),
                            // When logging to JSON, one can trigger alerts from the presence of this field.
                            slow_threshold=?self.settings.slow_statements_duration,
                            // Make sure to use "slow" in the message as that's likely
                            // what people will grep for.
                            "slow statement: execution time exceeded alert threshold"
                        );
                    } else {
                        private_tracing_dynamic_event!(
                            target: "sqlx::query",
                            tracing_level,
                            summary,
                            db.statement = sql,
                            rows_affected = self.rows_affected,
                            rows_returned = self.rows_returned,
                            // Human-friendly - includes units (usually ms). Also kept for backward compatibility
                            ?elapsed,
                            // Search friendly - numeric
                            elapsed_secs = elapsed.as_secs_f64(),
                        );
                    }
                });
            }
        }
    }
}

impl Drop for QueryLogger {
    fn drop(&mut self) {
        self.finish();
    }
}

pin_project! {
    /// Wraps a [`Stream`] so each `poll_next` runs inside the given [`Span`].
    ///
    /// This is the `Stream` counterpart to `tracing::Instrument` for futures. It
    /// re-enters the span on every poll and drops the guard before yielding, so no
    /// `EnteredSpan` is ever held across an await point — fixing the `!Send` issue
    /// that sank #3176. The inner stream is projected via `pin-project-lite`, so this
    /// adds no allocation and keeps the module free of `unsafe` pin code.
    pub struct InstrumentedStream<S> {
        #[pin]
        inner: S,
        span: Span,
    }
}

impl<S> InstrumentedStream<S> {
    pub fn new(inner: S, span: Span) -> Self {
        Self { inner, span }
    }
}

impl<S: Stream> Stream for InstrumentedStream<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        let _enter = this.span.enter();
        this.inner.poll_next(cx)
    }
}

pub fn parse_query_summary(sql: &str) -> String {
    // For now, just take the first 4 words
    sql.split_whitespace()
        .take(4)
        .collect::<Vec<&str>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_str::SqlSafeStr;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Record};
    use tracing::subscriber::{with_default, Subscriber};
    use tracing::{Event, Id, Metadata};

    struct CapturedSpan {
        name: &'static str,
        target: String,
        level: tracing::Level,
        fields: std::collections::HashMap<String, String>,
        closed: bool,
        contained_events: usize,
    }

    #[derive(Default)]
    struct CaptureSubscriber {
        next_id: std::sync::atomic::AtomicU64,
        spans: Mutex<std::collections::HashMap<u64, CapturedSpan>>,
        current: Mutex<Vec<u64>>,
    }

    struct StringVisitor<'a>(&'a mut std::collections::HashMap<String, String>);
    impl Visit for StringVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    impl Subscriber for CaptureSubscriber {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, attrs: &Attributes<'_>) -> Id {
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let mut span = CapturedSpan {
                name: attrs.metadata().name(),
                target: attrs.metadata().target().to_string(),
                level: *attrs.metadata().level(),
                fields: std::collections::HashMap::new(),
                closed: false,
                contained_events: 0,
            };
            attrs.record(&mut StringVisitor(&mut span.fields));
            self.spans.lock().unwrap().insert(id, span);
            Id::from_u64(id)
        }
        fn record(&self, span: &Id, values: &Record<'_>) {
            if let Some(s) = self.spans.lock().unwrap().get_mut(&span.into_u64()) {
                values.record(&mut StringVisitor(&mut s.fields));
            }
        }
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, _event: &Event<'_>) {
            let current = self.current.lock().unwrap();
            if let Some(&id) = current.last() {
                if let Some(s) = self.spans.lock().unwrap().get_mut(&id) {
                    s.contained_events += 1;
                }
            }
        }
        fn enter(&self, span: &Id) {
            self.current.lock().unwrap().push(span.into_u64());
        }
        fn exit(&self, _span: &Id) {
            self.current.lock().unwrap().pop();
        }
        fn try_close(&self, id: Id) -> bool {
            if let Some(s) = self.spans.lock().unwrap().get_mut(&id.into_u64()) {
                s.closed = true;
            }
            true
        }
    }

    #[test]
    fn query_logger_opens_and_closes_span_with_expected_fields() {
        let subscriber = Arc::new(CaptureSubscriber::default());
        with_default(subscriber.clone(), || {
            let settings = LogSettings::default();
            let sql = "SELECT id, name FROM users WHERE id = 1".into_sql_str();
            let mut logger = QueryLogger::new(sql, settings).with_db_system_name("postgresql");
            logger.increment_rows_returned();
            logger.increment_rows_returned();
            logger.increase_rows_affected(2);
            drop(logger);
        });

        let spans = subscriber.spans.lock().unwrap();
        assert_eq!(spans.len(), 1, "exactly one span should be opened");
        let span = spans.values().next().unwrap();

        assert_eq!(span.name, "db.query");
        assert_eq!(span.target, "sqlx::query");
        assert_eq!(span.level, tracing::Level::INFO);
        assert!(span.closed, "span must close on QueryLogger drop");
        assert!(
            span.contained_events >= 1,
            "the close-time event should fire inside the span"
        );

        assert_eq!(
            span.fields.get("db.system.name").map(String::as_str),
            Some("postgresql")
        );
        assert_eq!(
            span.fields.get("db.operation.name").map(String::as_str),
            Some("SELECT")
        );
        assert_eq!(
            span.fields.get("otel.kind").map(String::as_str),
            Some("client")
        );
        assert!(span
            .fields
            .get("db.query.text")
            .is_some_and(|s| s.contains("SELECT id, name FROM users")));
        assert_eq!(
            span.fields
                .get("db.response.returned_rows")
                .map(String::as_str),
            Some("2"),
            "rows_returned must be recorded on the span before close"
        );
        assert_eq!(
            span.fields
                .get("db.response.affected_rows")
                .map(String::as_str),
            Some("2"),
            "rows_affected must be recorded on the span before close"
        );
    }
}
