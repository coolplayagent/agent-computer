//! HTTP control-plane entry point. Runtime capabilities remain explicitly unsupported.
#![forbid(unsafe_code)]

mod error;
mod http;
pub use http::router;
