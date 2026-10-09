//! Validated opaque identifiers and independent monotonic counters.

use crate::{Error, Result};
use std::fmt;

macro_rules! identifier {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                if value.is_empty() || value.len() > 128
                    || !value.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
                {
                    return Err(Error::InvalidIdentifier);
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    )+};
}

identifier!(
    ComputerId,
    WorkspaceId,
    OrganizationId,
    PrincipalId,
    ConnectionId,
    AppId,
    CandidateId,
    ExecutionId,
    LeaseId,
    CheckpointId,
    EvidenceId,
    IdempotencyKey,
    OperationKind,
);

macro_rules! counter {
    ($name:ident, $initial:expr) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            pub const INITIAL: Self = Self($initial);

            /// Decode a value read from authoritative storage, never from a client clock.
            pub const fn from_u64(value: u64) -> Self {
                Self(value)
            }
            pub const fn value(self) -> u64 {
                self.0
            }
            pub fn next(self) -> Result<Self> {
                self.0
                    .checked_add(1)
                    .map(Self)
                    .ok_or(Error::CounterExhausted)
            }
        }
    };
}

counter!(Revision, 1);
counter!(Generation, 0);
counter!(LeaseEpoch, 0);

/// A parsed SHA-256 digest, not a hashing implementation or an integrity check.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InputDigest([u8; 32]);

impl InputDigest {
    pub fn parse(value: &str) -> Result<Self> {
        let hex = value.strip_prefix("sha256:").ok_or(Error::InvalidDigest)?;
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::InvalidDigest);
        }
        let mut bytes = [0; 32];
        for (i, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
            fn digit(b: u8) -> u8 {
                if b.is_ascii_digit() {
                    b - b'0'
                } else {
                    b.to_ascii_lowercase() - b'a' + 10
                }
            }
            bytes[i] = digit(pair[0]) * 16 + digit(pair[1]);
        }
        Ok(Self(bytes))
    }
}

pub(crate) fn check_revision(actual: Revision, expected: Revision) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::RevisionConflict)
    }
}

pub(crate) fn check_generation(actual: Generation, supplied: Generation) -> Result<()> {
    if actual == supplied {
        Ok(())
    } else {
        Err(Error::StaleGeneration)
    }
}
