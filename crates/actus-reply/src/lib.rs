//! Response types and the `reply!` macro for the Actus framework.
#![warn(missing_docs)]

pub mod finalizer;
pub mod reply;

pub use finalizer::Finalizer;
pub use reply::{
    BodyStream, ProblemDetails, Reply, ReplyData, ReplySpec, SseEvent, WebError, build_reply,
    bytes, empty, json, sse, stream, try_json,
};

/// Convenience prelude for downstream crates and applications.
pub mod prelude {
    pub use crate::reply;
    pub use crate::reply::{
        BodyStream, ProblemDetails, Reply, ReplyData, ReplySpec, SseEvent, WebError, build_reply,
        bytes, empty, json, sse, stream, try_json,
    };
    // HTTP status codes show up in handlers (`reply!(status = …, body)`,
    // `ProblemDetails::new(StatusCode::FORBIDDEN, …)`, etc.), so include
    // them in the prelude rather than make every consumer import `http`.
    pub use http::StatusCode;
}
