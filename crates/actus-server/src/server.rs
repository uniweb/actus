//! [`Server`] — the hyper-based HTTP server that owns the request lifecycle:
//! routing, body limiting, the middleware chain, CORS, compression, and the
//! HTTP-correctness stamping (`Allow`, `Vary`), with graceful shutdown.

#[cfg(feature = "compression")]
use crate::compression::CompressionLayer;
use crate::cors::CorsLayer;
use crate::error::ServerError;
use crate::middleware::{Middleware, MiddlewareChain, Outcome};
use crate::request::Request;
use crate::router::Router;
#[cfg(feature = "websocket")]
use crate::websocket;
#[cfg(feature = "websocket")]
use actus_reply::ProblemDetails;
use actus_reply::{Finalizer, ReplyData, WebError};
use bytes::Bytes;
#[cfg(feature = "websocket")]
use http::{HeaderValue, StatusCode, header};
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response as HyperResponse};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{Instrument, Level, error, info, span, warn};

type ResponseBody = BoxBody<Bytes, WebError>;

/// One kibibyte (1024 bytes). For readable byte-size limits, e.g.
/// `#[controller(max_body_bytes = 4 * KIB)]`.
pub const KIB: usize = 1024;
/// One mebibyte (1024 × 1024 bytes), e.g. `Server::with_max_body_bytes(2 * MIB)`.
pub const MIB: usize = 1024 * KIB;
/// One gibibyte (1024 × 1024 × 1024 bytes).
pub const GIB: usize = 1024 * MIB;

/// Default cap on the request body Actus will buffer: **2 MiB** — a safe
/// ceiling for the common case (JSON APIs, forms). Endpoints that accept larger
/// bodies (uploads) opt in via [`Server::with_max_body_bytes`] or a
/// per-controller `#[controller(max_body_bytes = …)]`. Matches axum's default.
pub const DEFAULT_MAX_BODY_BYTES: usize = 2 * MIB;

/// Default grace period for in-flight connections to finish after a
/// shutdown signal: 30 seconds. Override with [`Server::with_drain_deadline`].
pub const DEFAULT_DRAIN_DEADLINE: Duration = Duration::from_secs(30);

/// The main Actus server.
pub struct Server {
    router: Arc<Router>,
    middleware_chain: Arc<MiddlewareChain>,
    finalizer: Arc<Finalizer>,
    max_body_bytes: usize,
    cors: Option<Arc<CorsLayer>>,
    #[cfg(feature = "compression")]
    compression: Option<Arc<CompressionLayer>>,
    /// `Some(d)` caps each request's total time (parse → middleware →
    /// handler → after-chain → finalize) at `d`; an over-budget request is
    /// aborted and replied with `504 Gateway Timeout`. `None` disables the
    /// per-request timer (the default).
    request_timeout: Option<Duration>,
    /// Grace period for in-flight connections to drain after shutdown.
    drain_deadline: Duration,
    /// Cap on concurrent connection tasks. `Some(n)` installs an
    /// `Arc<Semaphore>` of `n` permits in the accept loop; while at
    /// capacity, the loop pauses on permit acquisition and new SYNs queue
    /// in the kernel's accept backlog (`SOMAXCONN`), at which point the
    /// kernel drops them. `None` is unbounded.
    max_connections: Option<usize>,
    /// Cap on the total bytes being buffered across all in-flight body
    /// reads. `Some(n)` installs a byte-permit semaphore; each
    /// `collect_body_capped` reserves its per-request cap upfront and
    /// releases the permits when the body is buffered or rejected. Refuses
    /// excess requests with `503 Service Unavailable` (via `WebError::Busy`).
    /// `None` is unbounded.
    max_inflight_body_bytes: Option<Arc<Semaphore>>,
    /// `Some(d)` is forwarded to hyper's `http1::Builder::header_read_timeout`
    /// — bounds how long after starting to read request headers we'll wait
    /// before dropping the connection. Catches slowloris and clients that
    /// TCP-connect-and-send-nothing. `None` leaves hyper's default (none).
    header_read_timeout: Option<Duration>,
}

impl Server {
    /// Create a server for `router` with default settings: no middleware, no
    /// CORS, the default body cap, and no DoS limits. Configure it with the
    /// `with_*` builder methods, then call [`run`](Self::run).
    pub fn new(router: Router) -> Self {
        Self {
            router: Arc::new(router),
            middleware_chain: Arc::new(MiddlewareChain::new()),
            finalizer: Arc::new(Finalizer::new()),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            cors: None,
            #[cfg(feature = "compression")]
            compression: None,
            request_timeout: None,
            drain_deadline: DEFAULT_DRAIN_DEADLINE,
            max_connections: None,
            max_inflight_body_bytes: None,
            header_read_timeout: None,
        }
    }

    /// Adds a middleware to the server's request processing chain.
    pub fn with_middleware(mut self, middleware: impl Middleware + 'static) -> Self {
        let mut chain = Arc::try_unwrap(self.middleware_chain).unwrap_or_else(|arc| (*arc).clone());
        chain.add(middleware);
        self.middleware_chain = Arc::new(chain);
        self
    }

    /// Enables CORS with the given policy. The server then answers preflight
    /// (`OPTIONS`) requests itself and adds the `Access-Control-*` headers to
    /// every cross-origin response (including error responses). See
    /// [`CorsLayer`].
    pub fn with_cors(mut self, cors: CorsLayer) -> Self {
        self.cors = Some(Arc::new(cors));
        self
    }

    /// Enables response compression (gzip / brotli). For each response Actus
    /// picks an encoding from the request's `Accept-Encoding` and compresses
    /// buffered, compressible bodies above the layer's size threshold. See
    /// [`CompressionLayer`]. *(Requires the `compression` feature.)*
    #[cfg(feature = "compression")]
    pub fn with_compression(mut self, layer: CompressionLayer) -> Self {
        self.compression = Some(Arc::new(layer));
        self
    }

    /// Caps the request body Actus will buffer (default
    /// [`DEFAULT_MAX_BODY_BYTES`] = 2 MiB). A larger body is rejected with
    /// `413 Payload Too Large` before it can exhaust memory — the limit
    /// bounds buffered bytes, so it also covers chunked bodies that lie about
    /// (or omit) `Content-Length`.
    ///
    /// `0` is accepted and means "reject every non-empty body" — typically
    /// only useful on a strictly-GET surface that should never see a body.
    pub fn with_max_body_bytes(mut self, max: usize) -> Self {
        self.max_body_bytes = max;
        self
    }

    /// Cap the total time any single request may take — body parse,
    /// middleware `before`, handler, middleware `after`, and finalization
    /// combined. An over-budget request is aborted (the handler's future
    /// is dropped) and the client gets `504 Gateway Timeout`. No timeout
    /// is set by default.
    ///
    /// **Scope.** The timer covers the request/response exchange. A
    /// WebSocket upgrade succeeds inside the timer (the `101` is the
    /// response); the post-upgrade conversation runs in its own task and
    /// is not bound by this timeout.
    ///
    /// **Effect of an over-budget request.** When the timer elapses the
    /// in-flight future is dropped, which cancels whatever the handler
    /// was awaiting (DB query, channel recv, etc.). The 504 reply is
    /// one-shot — the after-chain doesn't run on it (by definition,
    /// some component upstream was unresponsive; running more risks
    /// hanging again).
    pub fn with_request_timeout(mut self, d: Duration) -> Self {
        self.request_timeout = Some(d);
        self
    }

    /// Override the grace period for in-flight connections after a
    /// shutdown signal (default [`DEFAULT_DRAIN_DEADLINE`] = 30 s).
    /// Anything still running at the deadline is hard-aborted via
    /// `JoinSet::shutdown`. Use a longer value for surfaces that hold
    /// long-lived connections (large file downloads, WebSockets);
    /// a shorter value for fast-iteration dev workflows. `Duration::ZERO`
    /// aborts every in-flight task immediately.
    pub fn with_drain_deadline(mut self, d: Duration) -> Self {
        self.drain_deadline = d;
        self
    }

    /// Cap concurrent connection tasks at `n`. While the cap is held, the
    /// accept loop pauses on permit acquisition; new SYNs queue in the
    /// kernel's accept backlog and (once that fills, governed by
    /// `SOMAXCONN`) get dropped at the OS level. No userland reject /
    /// no-503-per-conn cost — the kernel handles the spillover.
    ///
    /// Each connection task holds its permit until it ends, including the
    /// post-handshake WebSocket conversation. Size accordingly: a
    /// `with_max_connections(N)` server can hold `N` long-lived WebSockets
    /// *before* it stops accepting new connections of any kind.
    ///
    /// Unbounded by default (no semaphore installed).
    pub fn with_max_connections(mut self, n: usize) -> Self {
        self.max_connections = Some(n);
        self
    }

    /// Cap the total bytes being buffered across all in-flight body reads
    /// at `n`. Each request reserves its per-request cap (see
    /// `with_max_body_bytes`) from this global budget upfront; if the
    /// budget is exhausted, the request is refused with `503 Service
    /// Unavailable` (via [`WebError::Busy`]) and a short `Retry-After`.
    ///
    /// Together with [`Self::with_max_connections`] this puts a hard
    /// ceiling on the framework's memory under adversarial load:
    /// `with_max_connections(C) * with_max_body_bytes(B)` is the *worst*
    /// case absent this knob; with it, the ceiling is `min(C * B, this
    /// value)`.
    ///
    /// Pre-reserving the per-request cap over-counts (a 1 KB request
    /// reserves up to its full cap); the alternative — incremental
    /// per-chunk byte accounting — is more code for the same effective
    /// ceiling, and a request that has already started buffering can't
    /// be sensibly aborted partway through anyway.
    ///
    /// `n` is clamped to `u32::MAX` internally (Tokio's `Semaphore`
    /// permit count uses `u32`); for practical deployments this is no
    /// limit (4 GiB).
    ///
    /// Unbounded by default.
    pub fn with_max_inflight_body_bytes(mut self, n: usize) -> Self {
        // u32 cap is a tokio Semaphore constraint, not a design choice.
        let n_capped = n.min(u32::MAX as usize);
        self.max_inflight_body_bytes = Some(Arc::new(Semaphore::new(n_capped)));
        self
    }

    /// Bound how long after starting to read request headers we'll wait
    /// before dropping the connection. Forwards to hyper's
    /// `http1::Builder::header_read_timeout`. Catches slowloris (sending
    /// headers one byte at a time) and clients that TCP-connect-and-send-
    /// nothing — the most common file-descriptor-exhaustion attack on a
    /// keep-alive HTTP server.
    ///
    /// Note: hyper 1.x doesn't have a separate "idle between requests"
    /// timeout (after a complete request, an idle keep-alive connection
    /// stays open until either side closes or the OS-level TCP keep-alive
    /// fires). If that matters for your deployment, either disable
    /// keep-alive entirely upstream of Actus or rely on the OS knobs.
    ///
    /// No timeout by default (hyper's default).
    pub fn with_header_read_timeout(mut self, d: Duration) -> Self {
        self.header_read_timeout = Some(d);
        self
    }

    /// Runs the server on `127.0.0.1:port` (loopback only). For a different
    /// bind address — e.g. `0.0.0.0:port` to accept connections from other
    /// hosts in a container — use [`Server::run_on`].
    ///
    /// Listens for SIGTERM/SIGINT (Unix) or Ctrl-C (cross-platform) and
    /// shuts down gracefully: stops accepting new connections, signals
    /// in-flight connections to finish, and waits up to 30 seconds for
    /// them to drain before returning.
    pub async fn run(self, port: u16) -> Result<(), ServerError> {
        self.run_on(SocketAddr::from(([127, 0, 0, 1], port))).await
    }

    /// Like [`Server::run`] but binds an arbitrary address. Pass
    /// `0.0.0.0:port` (or `[::]:port`) to accept connections from other hosts.
    pub async fn run_on(self, addr: SocketAddr) -> Result<(), ServerError> {
        self.run_with_shutdown_on(addr, default_shutdown_signal())
            .await
    }

    /// Like [`Server::run`] but with a custom shutdown trigger (a future that,
    /// when it resolves, starts the graceful drain). Binds `127.0.0.1:port`;
    /// see [`Server::run_with_shutdown_on`] for a custom bind address. Useful
    /// for tests or for embedding the server in a larger supervision tree.
    pub async fn run_with_shutdown(
        self,
        port: u16,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), ServerError> {
        self.run_with_shutdown_on(SocketAddr::from(([127, 0, 0, 1], port)), shutdown)
            .await
    }

    /// Bind `addr`, then serve exactly as
    /// [`Server::run_with_shutdown_listener`] — the general form — does.
    /// [`Server::run`], [`Server::run_on`], and [`Server::run_with_shutdown`]
    /// are thin wrappers over this; this is a thin bind over the listener
    /// form.
    ///
    /// **Drain bound.** Once `shutdown` resolves the server stops accepting
    /// and signals every in-flight connection to wind down. The drain
    /// deadline defaults to [`DEFAULT_DRAIN_DEADLINE`] (30 s); override
    /// with [`Server::with_drain_deadline`]. Anything still running at
    /// the deadline is hard-aborted via `JoinSet::shutdown`. In particular,
    /// long-lived connections (WebSockets, slow downloads, kept-alive idle
    /// clients) and any connection task that raced the shutdown notification
    /// and missed it both get aborted at the deadline rather than draining
    /// gracefully.
    pub async fn run_with_shutdown_on(
        self,
        addr: SocketAddr,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), ServerError> {
        let listener = TcpListener::bind(addr).await?;
        self.run_with_shutdown_listener(listener, shutdown).await
    }

    /// Like [`Server::run_on`] but serves on a listener the caller already
    /// bound (or inherited), with the default SIGTERM / SIGINT shutdown
    /// trigger. See [`Server::run_with_shutdown_listener`] for what a
    /// pre-bound listener buys and how to adopt an inherited one.
    pub async fn run_listener(self, listener: TcpListener) -> Result<(), ServerError> {
        self.run_with_shutdown_listener(listener, default_shutdown_signal())
            .await
    }

    /// The most general form: serve on a listener the caller already bound
    /// (or inherited) until `shutdown` resolves, then drain. Every other
    /// `run*` method is a wrapper over this one.
    ///
    /// **Why hand the server a listener.** Two cases the bind-it-yourself
    /// forms can't express:
    ///
    /// - **Socket activation** — a supervisor (systemd's `LISTEN_FDS`
    ///   protocol, launchd, …) owns the listening socket and passes it to
    ///   the process it spawns. Because the socket outlives the process,
    ///   connections arriving during a restart queue in the kernel's accept
    ///   backlog instead of being refused, and the next process serves them.
    /// - **Race-free embedding and tests** — bind `127.0.0.1:0`, keep the
    ///   listener, and pass it in; no bind-drop-rebind window in which the
    ///   port can be lost, and requests may connect before the accept loop
    ///   even starts (the kernel queues them).
    ///
    /// An inherited `std::net::TcpListener` must be set non-blocking before
    /// conversion — tokio requires it, and a supervisor passes the fd
    /// blocking:
    ///
    /// ```no_run
    /// # async fn doc(server: actus_server::Server, inherited: std::net::TcpListener)
    /// # -> Result<(), actus_server::ServerError> {
    /// inherited.set_nonblocking(true)?;
    /// let listener = tokio::net::TcpListener::from_std(inherited)?;
    /// server.run_with_shutdown_listener(listener, std::future::pending()).await
    /// # }
    /// ```
    ///
    /// **Drain bound.** As [`Server::run_with_shutdown_on`]: once `shutdown`
    /// resolves the server stops accepting (the listener is dropped — under
    /// socket activation the *socket* stays open in the supervisor, which is
    /// the point) and in-flight connections get [`Server::with_drain_deadline`]
    /// (default [`DEFAULT_DRAIN_DEADLINE`] = 30 s) to finish before being
    /// aborted. A streaming response that never completes (SSE) always rides
    /// to the deadline.
    pub async fn run_with_shutdown_listener(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), ServerError> {
        match listener.local_addr() {
            Ok(addr) => info!("Server listening on http://{}", addr),
            // An inherited fd can decline local_addr (exotic socket family);
            // serving still works, so log what we know and carry on.
            Err(_) => info!("Server listening on an inherited socket"),
        }

        let app = Arc::new(self);
        // Per-connection cancellation: once `Notify::notify_waiters` fires,
        // every in-flight task wakes up and asks hyper to gracefully close
        // its connection (finishing the current response, then exiting).
        let notify = Arc::new(tokio::sync::Notify::new());
        let mut tasks: JoinSet<()> = JoinSet::new();

        // Optional cap on concurrent connections. When at-capacity the
        // accept loop pauses on permit acquisition; new SYNs queue in the
        // kernel accept backlog (SOMAXCONN) and get dropped at the OS
        // level once that fills. Each spawned connection task moves its
        // permit in; the permit releases when the task exits.
        let conn_permits = app.max_connections.map(|n| Arc::new(Semaphore::new(n)));

        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                // Accept-branch: acquire a connection permit first (when
                // a cap is configured), then accept. The outer select
                // races this against shutdown so a paused-at-capacity
                // accept loop still notices the shutdown signal.
                accept_with_permit = async {
                    let permit = match &conn_permits {
                        Some(s) => Some(s.clone().acquire_owned().await.expect("semaphore never closed")),
                        None => None,
                    };
                    let result = listener.accept().await;
                    (result, permit)
                } => {
                    let (accept_result, permit) = accept_with_permit;
                    let (stream, _peer) = match accept_result {
                        Ok(s) => s,
                        Err(e) => {
                            error!("accept error: {}", e);
                            // permit released when this branch ends — fine
                            continue;
                        }
                    };
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let app = app.clone();
                    let notify = notify.clone();
                    let header_timeout = app.header_read_timeout;
                    tasks.spawn(async move {
                        // The permit (if any) lives for the connection's
                        // lifetime; releasing happens at task drop.
                        let _permit = permit;

                        let mut builder = hyper::server::conn::http1::Builder::new();
                        if let Some(d) = header_timeout {
                            builder.header_read_timeout(d);
                        }
                        let conn = builder.serve_connection(
                            io,
                            service_fn(move |req| app.clone().handle_request(req)),
                        );
                        // With the `websocket` feature, allow `101 Switching
                        // Protocols` responses to hand off the connection.
                        #[cfg(feature = "websocket")]
                        let conn = conn.with_upgrades();
                        tokio::pin!(conn);
                        tokio::select! {
                            res = conn.as_mut() => {
                                if let Err(err) = res {
                                    error!("Error serving connection: {}", err);
                                }
                            }
                            _ = notify.notified() => {
                                conn.as_mut().graceful_shutdown();
                                if let Err(err) = conn.await {
                                    error!("Error during graceful shutdown: {}", err);
                                }
                            }
                        }
                    });
                }
                // Reap finished connection tasks so the `JoinSet` doesn't grow
                // without bound over the server's lifetime — and so a panicked
                // connection task is logged promptly, not only at shutdown.
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    match joined {
                        Some(Err(e)) if e.is_panic() => error!("Connection task panicked: {}", e),
                        Some(Err(e)) => error!("Connection task failed: {}", e),
                        Some(Ok(())) | None => {}
                    }
                }
                _ = &mut shutdown => {
                    info!("Shutdown signal received; draining in-flight requests");
                    break;
                }
            }
        }

        // Stop accepting; signal connections to wind down.
        drop(listener);
        notify.notify_waiters();

        // Drain. The grace period is configurable via
        // `Server::with_drain_deadline` (default 30 s).
        let drain_deadline = tokio::time::sleep(app.drain_deadline);
        tokio::pin!(drain_deadline);
        loop {
            tokio::select! {
                next = tasks.join_next() => {
                    match next {
                        Some(Ok(())) => {}
                        Some(Err(e)) if e.is_panic() => {
                            error!("Connection task panicked: {}", e);
                        }
                        Some(Err(e)) => {
                            error!("Connection task failed: {}", e);
                        }
                        None => break,
                    }
                }
                _ = &mut drain_deadline => {
                    warn!("Drain deadline exceeded; aborting {} connection(s)", tasks.len());
                    tasks.shutdown().await;
                    break;
                }
            }
        }

        info!("Server shutdown complete");
        Ok(())
    }

    /// Stamp the configured CORS response headers onto `response` (no-op when
    /// CORS isn't enabled, or the request had no allowed `Origin`). Applied to
    /// *every* outgoing response — success and error alike — so the browser
    /// can read 4xx/5xx bodies.
    fn with_cors_headers(
        &self,
        request: &Request,
        mut response: HyperResponse<ResponseBody>,
    ) -> HyperResponse<ResponseBody> {
        if let Some(cors) = &self.cors {
            cors.apply(&request.headers, response.headers_mut(), false);
        }
        response
    }

    /// Fulfil a `ReplyData::Upgrade` from a handler when the request was
    /// genuinely a WebSocket handshake: send `101 Switching Protocols` and
    /// spawn the handler on the upgraded connection. (The "handler returned
    /// Upgrade but the request wasn't a handshake" case is rewritten to a
    /// 426 reply in [`Self::finalize_reply`] before reaching this method, so
    /// it can flow through the after-chain like any other error.)
    ///
    /// No CORS headers on the `101`: WebSocket handshakes are scoped by
    /// browser origin checks (the handler inspects `Origin` itself before
    /// calling `ws::upgrade`), not by the CORS protocol — `Access-Control-*`
    /// on a `101` is meaningless to the browser.
    #[cfg(feature = "websocket")]
    async fn complete_ws_upgrade(
        &self,
        handler: Box<dyn std::any::Any + Send>,
        ws_upgrade: (hyper::upgrade::OnUpgrade, HeaderValue),
    ) -> HyperResponse<ResponseBody> {
        // `ReplyData::Upgrade` is only constructible via `ws::upgrade(...)`,
        // which always boxes an `UpgradeTask`. A failing downcast would mean
        // a crate-internal invariant is broken; surface as a panic rather
        // than silently producing a 500.
        let task = handler
            .downcast::<websocket::UpgradeTask>()
            .expect("ReplyData::Upgrade always carries an UpgradeTask");
        let (on_upgrade, accept) = ws_upgrade;
        tokio::spawn(websocket::run_upgrade(on_upgrade, *task));
        let mut resp = self.finalizer.build_response(ReplyData::Empty).await;
        *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        let h = resp.headers_mut();
        h.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        h.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
        resp
    }

    /// Build the error reply for `error` and route it through
    /// [`finalize_reply`](Self::finalize_reply), so the after-chain,
    /// compression, and CORS apply to errors exactly as they do to handler
    /// successes. This is the canonical way to produce a `WebError`
    /// response anywhere a `Request` exists.
    async fn finalize_error(
        &self,
        error: WebError,
        request: &Request,
        #[cfg(feature = "websocket")] ws_upgrade: Option<(hyper::upgrade::OnUpgrade, HeaderValue)>,
    ) -> HyperResponse<ResponseBody> {
        let data = self.finalizer.error_to_reply(error);
        self.finalize_reply(
            data,
            request,
            #[cfg(feature = "websocket")]
            ws_upgrade,
        )
        .await
    }

    /// Run the after-middleware chain, then turn the reply into a `Response`
    /// via [`dispatch_reply`](Self::dispatch_reply).
    ///
    /// **After-chain runs on every reply with a body and a `Request`.** That
    /// includes handler successes, `Outcome::Respond` short-circuits, *and*
    /// every error the application produced (404 / 405 / 401 / 400 / a
    /// handler-returned `Err(WebError)`, etc.). The README's promise that a
    /// request-id stamper "still fires on a short-circuit" generalizes to
    /// every reply — that's the contract this method enforces.
    ///
    /// **Exceptions** (the after-chain *doesn't* run):
    /// - **101 Switching Protocols** — a WebSocket-handshake success has no
    ///   HTTP body to decorate, and the upgrade machinery consumes the
    ///   connection.
    /// - **Pre-parse failures** — a request that fails before
    ///   [`Request::from_hyper`] returns a skeleton (e.g. malformed HTTP
    ///   from hyper itself) has no `Request` to give the hook. The body-cap
    ///   413 and truncated-body 400 are *not* exceptions here: `from_hyper`
    ///   now returns a skeleton `Request` on those, so they do run through
    ///   the after-chain.
    /// - **CORS preflight 204** — synthesized before middleware or routing;
    ///   not an application request (see [`Self::handle_request`]).
    async fn finalize_reply(
        &self,
        #[allow(unused_mut)] mut data: ReplyData,
        request: &Request,
        #[cfg(feature = "websocket")] ws_upgrade: Option<(hyper::upgrade::OnUpgrade, HeaderValue)>,
    ) -> HyperResponse<ResponseBody> {
        // If the handler returned `ws::upgrade(...)` but the request isn't a
        // real WebSocket handshake, rewrite to a 426 error reply *here* so
        // it flows through the same after-chain / compression / CORS path as
        // any other error. Only the success-handshake path (Upgrade reply +
        // ws_upgrade present) keeps the after-chain bypass — a 101 has no
        // HTTP body to decorate.
        #[cfg(feature = "websocket")]
        if matches!(data, ReplyData::Upgrade(_)) && ws_upgrade.is_none() {
            data = self.finalizer.error_to_reply(WebError::Problem(
                ProblemDetails::new(StatusCode::UPGRADE_REQUIRED, "WebSocket Upgrade Required")
                    .detail("this endpoint expects a WebSocket handshake"),
            ));
        }

        let needs_after_chain = !matches!(data, ReplyData::Upgrade(_));
        if needs_after_chain
            && let Err(e) = self
                .middleware_chain
                .process_response(request, &mut data)
                .await
        {
            // After-chain itself errored. Build a plain error response
            // (no further after-chain — recursion prevention) so a buggy
            // hook can't infinite-loop the request.
            return self.with_cors_headers(request, self.finalizer.build_error(e).await);
        }
        self.dispatch_reply(
            data,
            request,
            #[cfg(feature = "websocket")]
            ws_upgrade,
        )
        .await
    }

    /// Turn a handler's (or a short-circuiting middleware's) `ReplyData` into a
    /// fully processed response: WebSocket upgrade if it's an `Upgrade` reply;
    /// otherwise compress (if enabled), finalize, and stamp CORS / `Vary`.
    async fn dispatch_reply(
        &self,
        #[allow(unused_mut)] mut data: ReplyData,
        request: &Request,
        #[cfg(feature = "websocket")] ws_upgrade: Option<(hyper::upgrade::OnUpgrade, HeaderValue)>,
    ) -> HyperResponse<ResponseBody> {
        // A handler that returned `ws::upgrade(...)`: complete the handshake
        // instead of finalizing a body. `finalize_reply` only lets us reach
        // here for an `Upgrade` reply when the request *was* a real
        // handshake (otherwise it rewrote the reply to a 426 error), so
        // `ws_upgrade` is guaranteed `Some` on this branch.
        #[cfg(feature = "websocket")]
        if matches!(data, ReplyData::Upgrade(_)) {
            let ReplyData::Upgrade(handler) = data else {
                unreachable!()
            };
            let ws_upgrade =
                ws_upgrade.expect("finalize_reply rewrites Upgrade-without-handshake to 426");
            return self.complete_ws_upgrade(handler, ws_upgrade).await;
        }
        // Compression is the last transform — after response middleware,
        // before the bytes leave. (Only buffered, compressible bodies above
        // the threshold are touched.)
        #[cfg(feature = "compression")]
        if let Some(c) = &self.compression {
            data = c.compress_reply(
                data,
                request
                    .headers
                    .get("accept-encoding")
                    .and_then(|v| v.to_str().ok()),
            );
        }
        let response = self.finalizer.build_response(data).await;
        let response = self.with_cors_headers(request, response);
        #[cfg(feature = "compression")]
        let response = crate::compression::tag_vary_if_encoded(response);
        response
    }

    /// Handles an individual incoming `hyper::Request`.
    ///
    /// Wraps [`handle_request_inner`](Self::handle_request_inner) in a
    /// per-request timeout when one is configured (see
    /// [`Server::with_request_timeout`]); a timed-out request gets a
    /// one-shot `504 Gateway Timeout` (no after-chain, since by definition
    /// something upstream was unresponsive).
    ///
    /// Every reply with a `Request` flows through
    /// [`finalize_reply`](Self::finalize_reply) — handler successes,
    /// `Outcome::Respond` short-circuits, and *every* error (middleware
    /// `Err`, body parse failure, 404 / 405 from the router, handler-returned
    /// `Err`, even the 413 / 400 from the body-cap path). The after-chain,
    /// compression, and CORS apply uniformly. CORS preflight is the one
    /// short-circuit that bypasses the pipeline — it's HTTP-protocol traffic
    /// rather than an application request.
    async fn handle_request(
        self: Arc<Self>,
        req: HyperRequest<Incoming>,
    ) -> Result<HyperResponse<ResponseBody>, hyper::Error> {
        let timeout = self.request_timeout;
        let app = self.clone();
        let inner = app.handle_request_inner(req);
        match timeout {
            None => inner.await,
            Some(d) => match tokio::time::timeout(d, inner).await {
                Ok(r) => r,
                Err(_) => {
                    warn!(timeout = ?d, "request exceeded configured timeout");
                    Ok(self.finalizer.build_error(WebError::Timeout).await)
                }
            },
        }
    }

    /// The actual request pipeline. Split from
    /// [`handle_request`](Self::handle_request) so the latter can wrap it
    /// in a timeout when one is configured.
    ///
    /// **Lifecycle order:**
    ///
    /// 1. capture WS upgrade (if request looks like a handshake)
    /// 2. build the `Request` skeleton (no body yet)
    /// 3. CORS preflight short-circuit (uses headers only)
    /// 4. match controller — 404 short-circuits *without* buffering the
    ///    body (efficiency win on adversarial bad-path requests); then stamp
    ///    the matched controller's rate-limit class onto the request, so a
    ///    `before` middleware (which only gets `&Request`) can read it
    /// 5. buffer the body, capped per the resolved policy (today: server-
    ///    wide; soon, per-controller / per-route)
    /// 6. middleware `before`
    /// 7. `to_params` (Content-Type-driven body parse)
    /// 8. dispatch via the already-matched controller
    /// 9. middleware `after` + finalize
    ///
    /// The "route before buffer" order is what lets the body cap depend on
    /// the matched route — without it, the framework would have to commit
    /// to a single cap before knowing where the request is headed.
    async fn handle_request_inner(
        self: Arc<Self>,
        #[allow(unused_mut)] mut req: HyperRequest<Incoming>,
    ) -> Result<HyperResponse<ResponseBody>, hyper::Error> {
        let request_span = span!(Level::INFO, "request");
        async move {
            // 1. Capture the WS upgrade handshake (if any) before
            //    `from_hyper_parts` consumes the request: the `OnUpgrade`
            //    future and the derived `Sec-WebSocket-Accept`. (See
            //    `websocket` module docs for why this happens up front.)
            #[cfg(feature = "websocket")]
            let ws_upgrade: Option<(hyper::upgrade::OnUpgrade, HeaderValue)> =
                if websocket::is_upgrade_request(req.method(), req.headers()) {
                    websocket::accept_key(req.headers())
                        .map(|accept| (hyper::upgrade::on(&mut req), accept))
                } else {
                    None
                };

            // 2. Build the skeleton (method / path / query / headers); the
            //    body stream is held aside for step 5.
            let (mut request, body_stream) = Request::from_hyper_parts(req);

            // 3. CORS preflight: synthesize the 204 ourselves before any
            //    application-layer work. Preflights are HTTP-protocol
            //    traffic; neither `before` nor `after` middleware runs on
            //    them (see `CLAUDE.md` principle 1).
            if let Some(cors) = &self.cors
                && CorsLayer::is_preflight(&request.method, &request.headers)
            {
                let mut resp = self.finalizer.build_response(ReplyData::Empty).await;
                cors.apply(&request.headers, resp.headers_mut(), true);
                return Ok(resp);
            }

            // 4. Match controller. A path that hits nothing is 404 *before*
            //    body buffering — a 10 MiB POST to a non-existent URL no
            //    longer wastes 10 MiB of memory.
            let route_match = match self.router.match_controller(&request.path_parts) {
                Some(rm) => rm,
                None => {
                    return Ok(self
                        .finalize_error(
                            WebError::NotFound,
                            &request,
                            #[cfg(feature = "websocket")]
                            ws_upgrade,
                        )
                        .await);
                }
            };

            // 4b. Stamp the matched controller's rate-limit class onto the
            //     request (the skeleton predates routing, so it was `None`).
            //     This is the one piece of routing context a `before`
            //     middleware can't otherwise see — it gets `&mut Request`,
            //     not the matched controller. An application rate-limit
            //     middleware reads `request.rate_limit_class` and applies its
            //     own per-class policy; the framework owns the label and the
            //     `429` response, not the limiter. (Set before the body
            //     buffer so it survives `collect_body` and is present for the
            //     whole pipeline, including the after-chain on error replies.)
            request.rate_limit_class = route_match.controller.actus_rate_limit();

            // 5. Resolve the effective body cap: the matched controller's
            //    `#[controller(max_body_bytes = …)]` if it set one, otherwise
            //    the server-wide `with_max_body_bytes` cap (default 2 MiB). A
            //    future Phase 2 adds per-route overrides at the top of this
            //    fall-through.
            //
            //    The error path returns the same skeleton so the after-chain
            //    still has a `Request`.
            let effective_cap = route_match
                .controller
                .actus_max_body_bytes()
                .unwrap_or(self.max_body_bytes);
            request = match request
                .collect_body(
                    body_stream,
                    effective_cap,
                    self.max_inflight_body_bytes.as_ref(),
                )
                .await
            {
                Ok(r) => r,
                Err((request, e)) => {
                    warn!("rejecting request before parse: {}", e);
                    return Ok(self
                        .finalize_error(
                            e,
                            &request,
                            #[cfg(feature = "websocket")]
                            ws_upgrade,
                        )
                        .await);
                }
            };

            // 6. Middleware `before` chain.
            let pre_data: Option<ReplyData> =
                match self.middleware_chain.process_request(&mut request).await {
                    Ok(Outcome::Continue) => None,
                    Ok(Outcome::Respond(data)) => Some(data),
                    Err(e) => {
                        return Ok(self
                            .finalize_error(
                                e,
                                &request,
                                #[cfg(feature = "websocket")]
                                ws_upgrade,
                            )
                            .await);
                    }
                };

            // A `before` hook short-circuited with a reply — skip routing
            // and the handler, but still run the after-chain.
            if let Some(data) = pre_data {
                return Ok(self
                    .finalize_reply(
                        data,
                        &request,
                        #[cfg(feature = "websocket")]
                        ws_upgrade,
                    )
                    .await);
            }

            // 7. Body parse (JSON / form / opaque, per Content-Type).
            //    Malformed body → 400 through the after-chain.
            let params = match request.to_params() {
                Ok(p) => p,
                Err(e) => {
                    return Ok(self
                        .finalize_error(
                            e,
                            &request,
                            #[cfg(feature = "websocket")]
                            ws_upgrade,
                        )
                        .await);
                }
            };

            // 8. Dispatch via the matched controller. 405 (verb mismatch
            //    inside the controller) and handler-returned errors both
            //    come back through here.
            match route_match
                .controller
                .actus_dispatch(&route_match.action, params)
                .await
            {
                Ok(data) => Ok(self
                    .finalize_reply(
                        data,
                        &request,
                        #[cfg(feature = "websocket")]
                        ws_upgrade,
                    )
                    .await),
                Err(e) => Ok(self
                    .finalize_error(
                        e,
                        &request,
                        #[cfg(feature = "websocket")]
                        ws_upgrade,
                    )
                    .await),
            }
        }
        .instrument(request_span)
        .await
    }
}

/// Default shutdown trigger: resolves on SIGTERM, SIGINT (Unix), or Ctrl-C
/// (Windows). This is what [`Server::run`] uses; for tests or embedding,
/// see [`Server::run_with_shutdown`].
async fn default_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => info!("Received SIGTERM"),
            _ = sigint.recv() => info!("Received SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
        info!("Received Ctrl-C");
    }
}
