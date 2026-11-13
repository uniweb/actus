//! The access log ([`Server::with_access_log`](crate::Server::with_access_log)): one `tracing`
//! event per response, written by the server when the response ends.
//!
//! The server, not a middleware, writes it, because what it records is the server's to know:
//! the status actually sent, on every response, including those no `after` hook sees (a
//! request timeout's `504`, a CORS preflight, a WebSocket `101`, the reply an `after` hook's
//! error is turned into), and when the response *ended*, which for a stream is long after any
//! hook has run. The application's own facts reach it through the request's span.

use actus_reply::WebError;
use bytes::Bytes;
use http::{Method, StatusCode};
use http_body_util::combinators::BoxBody;
use hyper::body::{Body, Frame, SizeHint};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// The `tracing` target the access log writes under. Stable, so a filter can route or
/// silence the access log on its own (`actus::access=off`).
pub(crate) const TARGET: &str = "actus::access";

/// What a request's access-log event says about the request, gathered as the server takes it.
pub(crate) struct Entry {
    /// The request's span: the event is written inside it, with whatever the application
    /// recorded there.
    pub(crate) span: tracing::Span,
    pub(crate) method: Method,
    /// The routed path, after a host alias, without the query string.
    pub(crate) path: String,
    /// When the server took the request.
    pub(crate) started: Instant,
}

impl Entry {
    /// `body`, wrapped to write this entry once the response ends, with `status`, the status
    /// the response was sent with.
    pub(crate) fn attach(
        self,
        status: StatusCode,
        body: BoxBody<Bytes, WebError>,
    ) -> BoxBody<Bytes, WebError> {
        BoxBody::new(Logged {
            body,
            pending: Some((self, status)),
        })
    }

    fn write(self, status: StatusCode) {
        let elapsed_ms = millis(self.started.elapsed());
        self.span.in_scope(|| {
            tracing::info!(
                target: TARGET,
                method = %self.method,
                path = %self.path,
                status = status.as_u16(),
                elapsed_ms,
            );
        });
    }
}

/// Milliseconds, to the microsecond: most responses take under one.
fn millis(d: Duration) -> f64 {
    (d.as_secs_f64() * 1_000_000.0).round() / 1000.0
}

/// A response body that writes its [`Entry`] when it ends: when its last frame is taken, when
/// it fails, or — if hyper lets go of it first (the client left, the connection failed, the
/// drain deadline passed) — when it is dropped. Whichever comes first writes it, once.
///
/// `size_hint` and `is_end_stream` pass through untouched, so hyper frames the response exactly
/// as it would without the wrapper: a buffered body keeps its `Content-Length`.
struct Logged {
    body: BoxBody<Bytes, WebError>,
    /// Taken when written.
    pending: Option<(Entry, StatusCode)>,
}

impl Logged {
    fn end(&mut self) {
        if let Some((entry, status)) = self.pending.take() {
            entry.write(status);
        }
    }
}

impl Body for Logged {
    type Data = Bytes;
    type Error = WebError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, WebError>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.body).poll_frame(cx);
        let ended = match &polled {
            Poll::Ready(None) | Poll::Ready(Some(Err(_))) => true,
            // A buffered body says it is done as it yields its one frame; hyper may never
            // poll it again.
            Poll::Ready(Some(Ok(_))) => this.body.is_end_stream(),
            Poll::Pending => false,
        };
        if ended {
            this.end();
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

impl Drop for Logged {
    fn drop(&mut self) {
        self.end();
    }
}
