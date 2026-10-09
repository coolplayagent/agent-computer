//! HTTP control plane with an optional bounded Candidate file gateway.
#![forbid(unsafe_code)]

mod computers;
mod connections;
mod error;
mod executions;
mod files;
mod http;
mod plans;
mod requests;
mod runtime;
mod writers;
pub use http::{router, router_with_files};
