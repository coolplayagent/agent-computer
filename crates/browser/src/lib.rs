//! Trusted controller side of the private Browser worker protocol. Identities
//! correlate an already admitted operation; they never authenticate a caller.
#![forbid(unsafe_code)]
mod binding;
pub use binding::{Actor, Binding, Scope, Viewport};
mod contracts;
pub use contracts::{
    Action, ActionIntent, ActionReceipt, AuthorizationRequest, Callback, Decision, Grant,
    Operation, Outcome, Permission,
};
mod observation;
mod session;
pub use session::{Policy, ResponseObservation, Session, Transport};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidBinding,
    Protocol,
    Transport,
    Deadline,
    Limit,
    Closed,
    Denied,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "browser controller: {self:?}")
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

pub const INPUT_FRAME_BYTES: usize = 65_536;
pub const OUTPUT_FRAME_BYTES: usize = 3 * 1024 * 1024;

pub fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

#[cfg(test)]
mod tests;
