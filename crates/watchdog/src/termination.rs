//! A live pinned kernel domain. Empty observations require an independent IO
//! barrier before they can participate in a storage writer completion.
use crate::{Error, Request, Result, boottime_ms, cgroup};
use serde::Serialize;

#[derive(Debug)]
pub struct PinnedDomain {
    group: cgroup::Cgroup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmptyDomain {
    Empty,
    Removed,
}

impl PinnedDomain {
    /// Pin during live admission, while the original boot/deadline is valid.
    /// Dropping the pin does not disarm or signal the independent watchdogs.
    pub fn pin(request: &Request, device: u64) -> Result<Self> {
        if !rustix::process::geteuid().is_root() {
            return Err(Error::RootRequired);
        }
        Request::parse(&serde_json::to_vec(request).map_err(|_| Error::InvalidRequest)?)?;
        let boot = cgroup::read_small("/proc/sys/kernel/random/boot_id")?;
        let own = cgroup::read_small("/proc/self/cgroup")?;
        let own = own
            .strip_prefix("0::")
            .ok_or(Error::CgroupRequired)?
            .trim_end();
        request.check_node(boot.trim_end(), boottime_ms(), own)?;
        if request.deadline_boottime_ms <= boottime_ms() {
            return Err(Error::IdentityMismatch);
        }
        Ok(Self {
            group: cgroup::Cgroup::pin_matching(request, device)?,
        })
    }

    /// A kernel-removed cgroup cannot be repopulated. Path absence is never used.
    /// Kernel rmdir requires no child groups or live processes; zombie processes
    /// still require the caller's independent pidfd exit observations.
    pub fn observe_empty(&self) -> Result<Option<EmptyDomain>> {
        if self.group.removed()? {
            return Ok(Some(EmptyDomain::Removed));
        }
        match self.group.populated() {
            Ok(false) => Ok(Some(EmptyDomain::Empty)),
            Ok(true) => Ok(None),
            Err(_) if self.group.removed()? => Ok(Some(EmptyDomain::Removed)),
            Err(error) => Err(error),
        }
    }

    /// Revoke all processes in the original domain and its descendants. This is
    /// bounded observation, not an IO drain or permission to restart anything.
    pub fn terminate(&self) -> Result<EmptyDomain> {
        if self.group.removed()? {
            return Ok(EmptyDomain::Removed);
        }
        if let Err(error) = self.group.kill()
            && !self.group.removed()?
        {
            return Err(error);
        }
        let deadline = boottime_ms().saturating_add(5000);
        loop {
            if let Some(observation) = self.observe_empty()? {
                return Ok(observation);
            }
            if boottime_ms() >= deadline {
                return Err(Error::DrainTimeout);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::MetadataExt, path::PathBuf, process::Command};
    struct Fixture {
        path: PathBuf,
        child: Option<std::process::Child>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::write(self.path.join("cgroup.kill"), b"1");
            if let Some(child) = &mut self.child {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = fs::remove_dir(self.path.join("nested"));
            let _ = fs::remove_dir(&self.path);
        }
    }
    #[test]
    #[ignore = "requires explicitly disposable root VM and writable cgroup v2"]
    fn pinned_domain_handles_stopped_descendants_removal_and_path_reuse() {
        assert_eq!(
            std::env::var("AGENT_COMPUTER_DISPOSABLE_CGROUP_TEST").as_deref(),
            Ok("1")
        );
        let name = format!("ac-pinned-domain-{}-{}", std::process::id(), boottime_ms());
        let path = PathBuf::from("/sys/fs/cgroup").join(&name);
        fs::create_dir(&path).unwrap();
        let mut fixture = Fixture { path, child: None };
        let stat = fs::metadata(&fixture.path).unwrap();
        let request = Request {
            version: 1,
            execution_id: name.clone(),
            boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .unwrap()
                .trim()
                .into(),
            cgroup_path: name,
            cgroup_inode: stat.ino(),
            deadline_boottime_ms: boottime_ms() + 5000,
        };
        assert!(PinnedDomain::pin(&request, stat.dev() + 1).is_err());
        let pin = PinnedDomain::pin(&request, stat.dev()).unwrap();
        assert_eq!(pin.observe_empty().unwrap(), Some(EmptyDomain::Empty));
        fs::create_dir(fixture.path.join("nested")).unwrap();
        let child = Command::new("/usr/bin/sleep").arg("30").spawn().unwrap();
        fs::write(
            fixture.path.join("nested/cgroup.procs"),
            child.id().to_string(),
        )
        .unwrap();
        rustix::process::kill_process(
            rustix::process::Pid::from_child(&child),
            rustix::process::Signal::STOP,
        )
        .unwrap();
        fixture.child = Some(child);
        assert_eq!(pin.observe_empty().unwrap(), None);
        assert_eq!(pin.terminate().unwrap(), EmptyDomain::Empty);
        assert!(!fixture.child.take().unwrap().wait().unwrap().success());
        fs::remove_dir(fixture.path.join("nested")).unwrap();
        fs::remove_dir(&fixture.path).unwrap();
        assert_eq!(pin.observe_empty().unwrap(), Some(EmptyDomain::Removed));
        fs::create_dir(&fixture.path).unwrap();
        let child = Command::new("/usr/bin/sleep").arg("30").spawn().unwrap();
        fs::write(fixture.path.join("cgroup.procs"), child.id().to_string()).unwrap();
        fixture.child = Some(child);
        assert!(PinnedDomain::pin(&request, stat.dev()).is_err());
        assert_eq!(pin.terminate().unwrap(), EmptyDomain::Removed);
        // Reusing the pathname cannot redirect a kill to the replacement group.
        assert!(
            fixture
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none()
        );
    }
}
