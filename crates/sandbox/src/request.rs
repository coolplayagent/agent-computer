use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_REQUEST_BYTES: usize = 65536;

/// Trusted orchestration input. Not a public admission API or a lease token.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub execution_id: String,
    pub generation: u64,
    pub argv: Vec<String>,
    /// Empty means /workspace. Otherwise a normalized relative directory.
    pub cwd: String,
    pub timeout_seconds: u32,
    /// Conservative remaining time supplied by the dispatcher; no renewal yet.
    pub lease_budget_ms: u32,
    pub term_grace_ms: u32,
    /// Retained bytes per stream; excess is drained and counted, never buffered.
    pub output_limit_bytes: usize,
}

impl Request {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(Error::InvalidRequest);
        }
        let request: Self = serde_json::from_slice(bytes).map_err(|_| Error::InvalidRequest)?;
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<()> {
        if self.execution_id.is_empty()
            || self.execution_id.len() > 128
            || !self
                .execution_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            || !(1..=i64::MAX as u64).contains(&self.generation)
            || self.argv.is_empty()
            || self.argv.len() > 128
            || !self.argv[0].starts_with('/')
            || self.argv.iter().any(|s| s.contains('\0'))
            || self.argv.iter().map(String::len).sum::<usize>() > 32768
            || (!self.cwd.is_empty() && !relative(&self.cwd))
            || !(1..=3600).contains(&self.timeout_seconds)
            || !(1..=30000).contains(&self.lease_budget_ms)
            || self.term_grace_ms > 5000
            || self.output_limit_bytes > 1_048_576
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| Error::InvalidRequest)?;
        let mut hash = Sha256::new();
        hash.update(b"agent-computer/sandbox-execution-v1\0");
        hash.update(bytes);
        Ok(format!("sha256:{:x}", hash.finalize()))
    }
}

fn relative(s: &str) -> bool {
    s.len() <= 1024
        && !s.contains(['\\', '\0'])
        && s.split('/').count() <= 32
        && s.split('/')
            .all(|p| !p.is_empty() && p != "." && p != ".." && p.len() <= 255)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> Request {
        Request {
            execution_id: "exec_1".into(),
            generation: 1,
            argv: vec!["/bin/echo".into(), "a;$(id)".into()],
            cwd: String::new(),
            timeout_seconds: 2,
            lease_budget_ms: 3000,
            term_grace_ms: 100,
            output_limit_bytes: 1024,
        }
    }
    #[test]
    fn argv_is_structured_and_every_execution_input_is_bound() {
        let original = request();
        let bytes = serde_json::to_vec(&original).unwrap();
        assert_eq!(Request::parse(&bytes).unwrap().argv[1], "a;$(id)");
        let digest = original.digest().unwrap();
        let mut changed = original.clone();
        changed.argv[1].push('x');
        assert_ne!(digest, changed.digest().unwrap());
        let mut changed = original.clone();
        changed.generation += 1;
        assert_ne!(digest, changed.digest().unwrap());
        let mut changed = original.clone();
        changed.lease_budget_ms += 1;
        assert_ne!(digest, changed.digest().unwrap());
        let mut changed = original;
        changed.cwd = "child".into();
        assert_ne!(digest, changed.digest().unwrap());
    }
    #[test]
    fn rejects_unbounded_or_ambiguous_requests() {
        let original = serde_json::to_value(request()).unwrap();
        for (key, value) in [
            ("generation", serde_json::json!(0)),
            ("argv", serde_json::json!([])),
            ("argv", serde_json::json!(["echo"])),
            ("argv", serde_json::json!(["/bin/echo", "a\0b"])),
            ("cwd", serde_json::json!("../secret")),
            ("cwd", serde_json::json!("/workspace")),
            ("cwd", serde_json::json!("a//b")),
            ("cwd", serde_json::json!("a/./b")),
            ("timeout_seconds", serde_json::json!(0)),
            ("timeout_seconds", serde_json::json!(3601)),
            ("lease_budget_ms", serde_json::json!(0)),
            ("lease_budget_ms", serde_json::json!(30001)),
            ("term_grace_ms", serde_json::json!(5001)),
            ("output_limit_bytes", serde_json::json!(1048577)),
            ("env", serde_json::json!({"TOKEN":"secret"})),
        ] {
            let mut changed = original.clone();
            changed[key] = value;
            assert!(
                Request::parse(&serde_json::to_vec(&changed).unwrap()).is_err(),
                "{key}"
            );
        }
        assert!(Request::parse(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
        assert!(Request::parse(br#"{"execution_id":"a","execution_id":"b"}"#).is_err());
    }
}
