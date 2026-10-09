use crate::{Error, Result};
use serde::{Deserialize, Serialize};

pub const MAX_REQUEST_BYTES: usize = 4096;
pub const MAX_BUDGET_MS: u64 = 30_000;

/// Trusted node operator input; never accept this from a workload or public API.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u8,
    pub execution_id: String,
    pub boot_id: String,
    /// Normalized path relative to the host's /sys/fs/cgroup mount.
    pub cgroup_path: String,
    pub cgroup_inode: u64,
    /// Absolute CLOCK_BOOTTIME milliseconds, fixed before invocation.
    pub deadline_boottime_ms: u64,
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

    fn validate(&self) -> Result<()> {
        let id = &self.execution_id;
        if self.version != 1
            || id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || !boot_id_valid(&self.boot_id)
            || !path_valid(&self.cgroup_path)
            || self.cgroup_inode == 0
            || self.deadline_boottime_ms == 0
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }

    pub(crate) fn check_node(&self, boot_id: &str, now: u64, own_path: &str) -> Result<()> {
        self.validate()?;
        if self.boot_id != boot_id
            || self.deadline_boottime_ms > now.saturating_add(MAX_BUDGET_MS)
            || !own_path.starts_with('/')
            || (own_path != "/" && !path_valid(&own_path[1..]))
            || own_path[1..] == self.cgroup_path
            || own_path[1..].starts_with(&format!("{}/", self.cgroup_path))
        {
            return Err(Error::IdentityMismatch);
        }
        // Expired requests are safe to retry: kill immediately, never extend them.
        Ok(())
    }
}

fn boot_id_valid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

fn path_valid(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.len() <= 255
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            version: 1,
            execution_id: "exec-1".into(),
            boot_id: "12345678-1234-1234-1234-123456789abc".into(),
            cgroup_path: "test.slice/workload.scope".into(),
            cgroup_inode: 123,
            deadline_boottime_ms: 50_000,
        }
    }

    #[test]
    fn absolute_deadline_cannot_be_refreshed_or_cross_boots() {
        let r = request();
        assert!(r.check_node(&r.boot_id, 20_000, "/guard").is_ok());
        assert!(r.check_node(&r.boot_id, 50_001, "/guard").is_ok());
        assert!(r.check_node(&r.boot_id, 19_999, "/guard").is_err());
        assert!(
            r.check_node("22345678-1234-1234-1234-123456789abc", 20_000, "/guard")
                .is_err()
        );
    }

    #[test]
    fn rejects_root_traversal_and_noncanonical_paths() {
        for path in [
            "", "/", "/test", ".", "..", "a/../b", "a//b", "a/", "a/./b", "a\nb",
        ] {
            let mut r = request();
            r.cgroup_path = path.into();
            assert!(r.validate().is_err(), "{path}");
        }
    }

    #[test]
    fn cannot_kill_watchdog_or_its_ancestor() {
        let r = request();
        for own in [
            "/test.slice/workload.scope",
            "/test.slice/workload.scope/child",
            "/../outside",
        ] {
            assert!(r.check_node(&r.boot_id, 20_000, own).is_err());
        }
        assert!(
            r.check_node(&r.boot_id, 20_000, "/test.slice/workload.scope-other")
                .is_ok()
        );
        assert!(r.check_node(&r.boot_id, 20_000, "/").is_ok());
    }

    #[test]
    fn rejects_ambiguous_or_unbounded_protocol() {
        let bytes = serde_json::to_vec(&request()).unwrap();
        assert!(Request::parse(&bytes).is_ok());
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json["renew_ms"] = 100.into();
        assert!(Request::parse(&serde_json::to_vec(&json).unwrap()).is_err());
        assert!(Request::parse(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
        let mut r = request();
        r.deadline_boottime_ms = 0;
        assert!(r.validate().is_err());
        r = request();
        r.boot_id.make_ascii_uppercase();
        assert!(r.validate().is_err());
    }
}
