use crate::request::Request;
use actus_reply::{ReplyData, WebError};
use async_trait::async_trait;
use std::sync::Arc;

/// What a middleware's [`before`](Middleware::before) hook decided.
#[derive(Debug)]
pub enum Outcome {
    /// Carry on — run the next middleware (and, eventually, the handler).
    Continue,
    /// Short-circuit: build this reply and return it. The handler and any
    /// remaining `before` hooks are skipped (`after` hooks still run on the
    /// reply). Use `Outcome::Respond(reply::empty())`, a `reply::build_reply()`
    /// for an explicit status/headers, etc. — anything a handler could return.
    Respond(ReplyData),
}

/// A trait for implementing middleware.
///
/// `before` runs in registration order; `after` runs in reverse — so a
/// middleware wraps the ones added after it (`[A, B]`: `A.before`, `B.before`,
/// handler, `B.after`, `A.after`).
#[async_trait]
pub trait Middleware: Send + Sync {
    /// Called before the request is routed. Return [`Outcome::Continue`] to
    /// proceed, [`Outcome::Respond`] to short-circuit with a normal response,
    /// or `Err(WebError)` to short-circuit with an error response. The default
    /// implementation continues.
    async fn before(&self, _request: &mut Request) -> Result<Outcome, WebError> {
        Ok(Outcome::Continue)
    }

    /// Called after the handler returns (or after a `before`-hook
    /// `Outcome::Respond` short-circuit), on the way back out. `request` is
    /// the request the reply is for — so the hook can decide what to do based
    /// on the request's headers / method / path. Mutate `response` in place
    /// (including replacing it wholesale, or stamping headers via
    /// [`ReplyData::add_header`](actus_reply::ReplyData::add_header));
    /// `Err(WebError)` swaps the reply for an error response. The default
    /// implementation does nothing.
    ///
    /// `after` is **not** called on [`ReplyData::Upgrade`] replies — a
    /// WebSocket handshake's `101 Switching Protocols` has no body to decorate,
    /// and the server short-circuits that variant to the upgrade machinery
    /// before the `after` chain runs.
    async fn after(&self, _request: &Request, _response: &mut ReplyData) -> Result<(), WebError> {
        Ok(())
    }
}

/// A chain of middleware to be executed.
#[derive(Clone, Default)]
pub struct MiddlewareChain {
    middlewares: Vec<Arc<dyn Middleware>>,
}

impl MiddlewareChain {
    /// Create an empty chain.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a middleware to the chain. `before` hooks run in insertion
    /// order; `after` hooks run in reverse (outermost-last).
    pub fn add<M: Middleware + 'static>(&mut self, middleware: M) {
        self.middlewares.push(Arc::new(middleware));
    }

    /// Run every `before` hook in order. Returns [`Outcome::Continue`] if all
    /// of them did; [`Outcome::Respond`] from the first that short-circuited
    /// (the rest aren't run); or the first `Err`.
    pub async fn process_request(&self, request: &mut Request) -> Result<Outcome, WebError> {
        for middleware in &self.middlewares {
            match middleware.before(request).await? {
                Outcome::Continue => {}
                respond @ Outcome::Respond(_) => return Ok(respond),
            }
        }
        Ok(Outcome::Continue)
    }

    /// Run every `after` hook in reverse order (so the middleware added first
    /// wraps outermost), letting each decorate the outgoing reply.
    pub async fn process_response(
        &self,
        request: &Request,
        response: &mut ReplyData,
    ) -> Result<(), WebError> {
        for middleware in self.middlewares.iter().rev() {
            middleware.after(request, response).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::{HeaderMap, Method};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn req() -> Request {
        Request {
            method: Method::GET,
            path_parts: Vec::new(),
            query_params: HashMap::new(),
            body: Bytes::new(),
            headers: HeaderMap::new(),
            rate_limit_class: None,
        }
    }

    /// Uses the default `before` (continues).
    struct Continues;
    #[async_trait]
    impl Middleware for Continues {}

    struct ShortCircuits;
    #[async_trait]
    impl Middleware for ShortCircuits {
        async fn before(&self, _request: &mut Request) -> Result<Outcome, WebError> {
            Ok(Outcome::Respond(ReplyData::Empty))
        }
    }

    /// Flips a flag when its `before` runs — so a test can prove it *didn't*.
    struct Records(Arc<AtomicBool>);
    #[async_trait]
    impl Middleware for Records {
        async fn before(&self, _request: &mut Request) -> Result<Outcome, WebError> {
            self.0.store(true, Ordering::SeqCst);
            Ok(Outcome::Continue)
        }
    }

    #[tokio::test]
    async fn before_short_circuit_skips_the_rest() {
        let later_ran = Arc::new(AtomicBool::new(false));
        let mut chain = MiddlewareChain::new();
        chain.add(Continues);
        chain.add(ShortCircuits);
        chain.add(Records(later_ran.clone()));
        match chain.process_request(&mut req()).await {
            Ok(Outcome::Respond(ReplyData::Empty)) => {}
            other => panic!("expected Respond(Empty), got {other:?}"),
        }
        assert!(
            !later_ran.load(Ordering::SeqCst),
            "a middleware after the short-circuit still ran"
        );
    }

    #[tokio::test]
    async fn all_continue_yields_continue() {
        let mut chain = MiddlewareChain::new();
        chain.add(Continues);
        chain.add(Continues);
        assert!(matches!(
            chain.process_request(&mut req()).await,
            Ok(Outcome::Continue)
        ));
    }

    /// Appends an identifying letter (and the request's path) to a shared
    /// buffer when its `after` runs — so a test can prove ordering and that
    /// the hook saw the request.
    struct Trace {
        tag: &'static str,
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl Middleware for Trace {
        async fn after(
            &self,
            request: &Request,
            _response: &mut ReplyData,
        ) -> Result<(), WebError> {
            let path = request.path_parts.join("/");
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:{}", self.tag, path));
            Ok(())
        }
    }

    #[tokio::test]
    async fn after_runs_in_reverse_order_and_sees_the_request() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add(Trace {
            tag: "A",
            log: log.clone(),
        });
        chain.add(Trace {
            tag: "B",
            log: log.clone(),
        });
        chain.add(Trace {
            tag: "C",
            log: log.clone(),
        });
        let mut request = req();
        request.path_parts = vec!["api".into(), "users".into()];
        let mut data = ReplyData::Empty;
        chain.process_response(&request, &mut data).await.unwrap();
        // Registration order [A, B, C] → `after` order [C, B, A].
        assert_eq!(
            *log.lock().unwrap(),
            vec!["C:api/users", "B:api/users", "A:api/users"]
        );
    }
}
