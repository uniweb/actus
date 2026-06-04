//! Middleware for Actus.
//!
//! - The [`Middleware`] trait, the [`Outcome`] of a `before` hook, and the
//!   [`MiddlewareChain`] live here.
//! - Concrete middlewares ship under sub-modules; see [`logger`].

mod chain;
pub mod logger;

pub use chain::{Middleware, MiddlewareChain, Outcome};
pub use logger::RequestLogger;
