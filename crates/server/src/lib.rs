//! HTTP control-plane entry point. Runtime capabilities remain explicitly unsupported.
#![forbid(unsafe_code)]

mod computers;
mod error;
mod http;
mod plans;
mod requests;
mod runtime;
pub use http::router;
