//! Restartable expiry enforcement, with no startup or writer authority.
use crate::{Error, Observation, Report, Result, Trigger, boottime_ms, cgroup, journal};
use rustix::fs::{Dir, FlockOperation, flock};
use serde::Serialize;
use std::{
    fs::File,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Outcome {
    Unenrolled,
    Waiting,
    HistoricalBoot,
    Recorded,
    Recovered,
    Unavailable { error: Error },
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub journal_id: String,
    pub outcome: Outcome,
}

#[derive(Debug, Serialize)]
pub struct Batch {
    pub pass_complete: bool,
    pub scanned: usize,
    pub entries: Vec<Entry>,
}

/// The directory lock excludes duplicate reapers. Guards do not take this lock.
/// Batches bound memory and allow service watchdog notifications between them;
/// the cursor advances through the whole spool instead of starving later entries.
pub struct Reaper {
    spool: PathBuf,
    _lock: File,
    entries: Dir,
}

impl Reaper {
    pub fn open(spool: &Path) -> Result<Self> {
        if !rustix::process::geteuid().is_root() {
            return Err(Error::RootRequired);
        }
        let dir = journal::directory(spool, true)?;
        flock(&dir, FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| Error::ReaperAlreadyRunning)?;
        let entries = Dir::read_from(&dir).map_err(|_| Error::JournalUnavailable)?;
        Ok(Self {
            spool: spool.into(),
            _lock: dir,
            entries,
        })
    }

    pub fn step(&mut self) -> Result<Batch> {
        let mut batch = Batch {
            pass_complete: false,
            scanned: 0,
            entries: Vec::new(),
        };
        for _ in 0..128 {
            let Some(entry) = self.entries.next() else {
                self.entries.rewind();
                batch.pass_complete = true;
                break;
            };
            let entry = entry.map_err(|_| Error::JournalUnavailable)?;
            batch.scanned += 1;
            let Ok(id) = entry.file_name().to_str() else {
                continue;
            };
            if !id.starts_with("journal-") {
                continue;
            }
            let outcome =
                expire(&self.spool, id).unwrap_or_else(|error| Outcome::Unavailable { error });
            batch.entries.push(Entry {
                journal_id: id.into(),
                outcome,
            });
        }
        Ok(batch)
    }
}

fn eligible(deadline: u64, original_boot: &str, current_boot: &str, now: u64) -> Option<Outcome> {
    if original_boot != current_boot {
        Some(Outcome::HistoricalBoot)
    } else if now < deadline {
        Some(Outcome::Waiting)
    } else {
        None
    }
}

fn expire(spool: &Path, id: &str) -> Result<Outcome> {
    let Some((journal, enrollment)) = journal::Journal::open_enrolled(spool, id)? else {
        return Ok(Outcome::Unenrolled);
    };
    let request = journal.request();
    let boot = cgroup::read_small("/proc/sys/kernel/random/boot_id")?;
    if let Some(outcome) = eligible(
        request.deadline_boottime_ms,
        &request.boot_id,
        boot.trim_end(),
        boottime_ms(),
    ) {
        return Ok(outcome);
    }
    if journal.recovery()?.is_some()
        || journal
            .report()?
            .is_some_and(|r| r.observation == Observation::EmptyObserved)
    {
        return Ok(Outcome::Recorded);
    }
    let own = cgroup::read_small("/proc/self/cgroup")?;
    let own = own
        .strip_prefix("0::")
        .ok_or(Error::CgroupRequired)?
        .trim_end_matches('\n');
    request.check_node(boot.trim_end(), boottime_ms(), own)?;
    // Device/inode validation happens before constructing a kill-on-drop handle.
    let group = cgroup::Cgroup::open_matching(request, Some(enrollment.cgroup_device))?;
    let started = boottime_ms();
    group.kill()?;
    // Do not wait five seconds per uninterruptible workload and starve siblings.
    // No successful record until the kernel is empty; later passes retry.
    if group.populated()? {
        return Err(Error::DrainTimeout);
    }
    let report = Report {
        version: 1,
        request: request.clone(),
        cgroup_device: group.device,
        armed_boottime_ms: started,
        kill_boottime_ms: started,
        observed_boottime_ms: boottime_ms(),
        trigger: Trigger::Recovery,
        observation: Observation::EmptyObserved,
        error: None,
        journal: Some(journal.reference().clone()),
    };
    journal.complete_recovery(&report)?;
    Ok(Outcome::Recovered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_never_renews_the_original_deadline_or_crosses_boots() {
        assert!(matches!(
            eligible(100, "a", "a", 99),
            Some(Outcome::Waiting)
        ));
        assert!(eligible(100, "a", "a", 100).is_none());
        assert!(eligible(100, "a", "a", u64::MAX).is_none());
        assert!(matches!(
            eligible(100, "a", "b", 500),
            Some(Outcome::HistoricalBoot)
        ));
    }
}
