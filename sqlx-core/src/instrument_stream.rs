//! Attach a [`tracing::Span`] to a [`Stream`] so the span stays open for the
//! whole stream rather than just its construction.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use pin_project_lite::pin_project;
use tracing::Span;

pin_project! {
    /// A [`Stream`] adapter that enters `span` for the duration of every
    /// [`poll_next`](Stream::poll_next).
    ///
    /// A plain `#[tracing::instrument]` on an `async fn` that *returns* a stream
    /// only keeps its span entered while the stream is being built; the span is
    /// then closed before the caller ever polls the stream. Wrapping the
    /// returned stream with this adapter instead keeps the span open across row
    /// fetching, so e.g. a `sqlx::query` span measures the whole query rather
    /// than just its setup.
    pub struct InstrumentedStream<S> {
        #[pin]
        stream: S,
        span: Span,
    }
}

impl<S: Stream> Stream for InstrumentedStream<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        let _entered = this.span.enter();
        this.stream.poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

/// Extension trait for attaching a [`Span`] to a [`Stream`].
pub trait InstrumentStream: Stream + Sized {
    /// Wrap this stream so that `span` is entered every time it is polled.
    ///
    /// See [`InstrumentedStream`].
    fn instrument_stream(self, span: Span) -> InstrumentedStream<Self> {
        InstrumentedStream { stream: self, span }
    }
}

impl<S: Stream> InstrumentStream for S {}
