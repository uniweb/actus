//! WebSocket support (RFC 6455). Behind the `websocket` feature.
//!
//! A route handler that wants to serve a WebSocket validates the request as
//! usual (origin, auth, subprotocol — whatever it needs), then returns
//! [`upgrade`]`(...)` instead of an ordinary reply. The server completes the
//! handshake (`101 Switching Protocols`), upgrades the connection, and runs
//! the closure you supplied on the resulting [`WebSocket`] (a `Stream` of
//! incoming [`Message`]s and a `Sink` for outgoing ones).
//!
//! ```ignore
//! use actus::prelude::*;
//! use futures_util::{SinkExt, StreamExt};
//!
//! pub async fn echo(&self, _params: &Params) -> Reply {
//!     Ok(actus::ws::upgrade(|mut socket| async move {
//!         while let Some(Ok(msg)) = socket.next().await {
//!             if msg.is_text() || msg.is_binary() {
//!                 let _ = socket.send(msg).await;
//!             }
//!         }
//!     }))
//! }
//! ```
//!
//! Mount it like any other route (`GET "echo" => echo()`). If the request
//! reaching such a handler isn't actually a WebSocket handshake, the server
//! responds `426 Upgrade Required` instead of attempting the upgrade.
//!
//! ## Timing of the upgrade capture
//!
//! The server captures `OnUpgrade` (and derives `Sec-WebSocket-Accept`) for
//! any request whose method/headers look like an RFC 6455 handshake —
//! *before* middleware or routing runs. That means a handshake-shaped request
//! that ends up short-circuited by middleware, rejected on auth, or routed to
//! a non-WS handler still pays the small cost of capturing the upgrade
//! future; the captured `OnUpgrade` is then dropped harmlessly (hyper handles
//! a dropped upgrade future). This shape is deliberate: it keeps the upgrade
//! capture out of the handler's hot path and avoids threading a "give me the
//! upgrade now" callback through the reply machinery.

use actus_reply::ReplyData;
use futures_util::future::BoxFuture;
use http::{HeaderMap, HeaderValue, Method, header};
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use std::future::Future;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

/// Re-export of `tungstenite`'s message type — text / binary / ping / pong /
/// close frames.
pub use tokio_tungstenite::tungstenite::Message;

/// A server-side WebSocket connection: a [`Stream`](futures_util::Stream) of
/// incoming [`Message`]s and a [`Sink`](futures_util::Sink) for outgoing ones.
/// Obtained inside the closure passed to [`upgrade`].
pub type WebSocket = WebSocketStream<TokioIo<Upgraded>>;

/// The boxed one-shot task carried by `ReplyData::Upgrade` — run once the
/// connection has been upgraded.
pub(crate) type UpgradeTask = Box<dyn FnOnce(WebSocket) -> BoxFuture<'static, ()> + Send>;

/// Return this from a route handler to turn the response into a WebSocket
/// upgrade. `handler` runs on the upgraded connection after the handshake.
/// See the [module docs](self).
pub fn upgrade<F, Fut>(handler: F) -> ReplyData
where
    F: FnOnce(WebSocket) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let task: UpgradeTask = Box::new(move |ws| -> BoxFuture<'static, ()> { Box::pin(handler(ws)) });
    ReplyData::Upgrade(Box::new(task))
}

/// True iff `(method, headers)` carry an RFC 6455 WebSocket handshake.
pub(crate) fn is_upgrade_request(method: &Method, headers: &HeaderMap) -> bool {
    fn list_contains(headers: &HeaderMap, name: header::HeaderName, needle: &str) -> bool {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(needle)))
    }
    *method == Method::GET
        && list_contains(headers, header::CONNECTION, "upgrade")
        && list_contains(headers, header::UPGRADE, "websocket")
        && headers.contains_key(header::SEC_WEBSOCKET_KEY)
        && headers
            .get(header::SEC_WEBSOCKET_VERSION)
            .and_then(|v| v.to_str().ok())
            == Some("13")
}

/// The `Sec-WebSocket-Accept` value derived from the request's
/// `Sec-WebSocket-Key`, ready to put on the `101` response.
pub(crate) fn accept_key(request_headers: &HeaderMap) -> Option<HeaderValue> {
    let key = request_headers.get(header::SEC_WEBSOCKET_KEY)?;
    HeaderValue::from_str(&derive_accept_key(key.as_bytes())).ok()
}

/// Await the connection upgrade and run `task` on the WebSocket. Spawned by
/// the server after it has sent the `101` response.
pub(crate) async fn run_upgrade(on_upgrade: OnUpgrade, task: UpgradeTask) {
    match on_upgrade.await {
        Ok(upgraded) => {
            let socket =
                WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
            task(socket).await;
        }
        Err(e) => tracing::warn!("websocket upgrade failed: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (n, v) in pairs {
            h.insert(n.clone(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn recognizes_a_valid_handshake() {
        let h = headers(&[
            (header::CONNECTION, "keep-alive, Upgrade"),
            (header::UPGRADE, "websocket"),
            (header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ=="),
            (header::SEC_WEBSOCKET_VERSION, "13"),
        ]);
        assert!(is_upgrade_request(&Method::GET, &h));
        // wrong method / missing bits → not a handshake
        assert!(!is_upgrade_request(&Method::POST, &h));
        assert!(!is_upgrade_request(
            &Method::GET,
            &headers(&[(header::UPGRADE, "websocket")])
        ));
    }

    #[test]
    fn derives_the_rfc6455_accept_key() {
        // RFC 6455 §1.3: key "dGhlIHNhbXBsZSBub25jZQ==" → accept
        // "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".
        let h = headers(&[(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")]);
        assert_eq!(accept_key(&h).unwrap(), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert!(accept_key(&HeaderMap::new()).is_none());
    }
}
