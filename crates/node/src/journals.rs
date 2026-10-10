//! Read-only recovery. A local observation never reconstructs a live guard.
use crate::{Error, Result, guard::ArmedReceipt};
use agent_computer_watchdog::{
    Report,
    journal::{Journal, Reference},
};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JournalStatus {
    LegacyUnjournaled,
    Unconfirmed {
        journal: Reference,
    },
    Recorded {
        journal: Reference,
        report: Box<Report>,
        #[serde(skip_serializing_if = "Option::is_none")]
        recovery: Option<Box<Report>>,
    },
    Recovered {
        journal: Reference,
        report: Box<Report>,
    },
    Unavailable {
        error: Error,
    },
}

#[derive(Debug, Serialize)]
pub struct JournalObservations {
    pub guards: [JournalStatus; 2],
}

/// The input comes from the original immutable database arm, never a tenant
/// request. Recheck every file/reference and compare the actual armed identity.
/// No runtime commands, process signals, grants or writer transitions occur here.
pub fn observe_journals(spool: &Path, evidence: &Value) -> Result<JournalObservations> {
    if !rustix::process::geteuid().is_root() {
        return Err(Error::RootRequired);
    }
    let mut guards = Vec::new();
    for (index, name) in ["armed", "backup_armed"].iter().enumerate() {
        let result = (|| {
            // Version 1 had only one process and cannot fabricate a backup.
            if evidence.get(*name).is_none() {
                return if index == 1
                    && evidence.get("version").is_none()
                    && evidence.get("armed").is_some()
                {
                    Ok(JournalStatus::LegacyUnjournaled)
                } else {
                    Err(Error::InvalidObservation)
                };
            }
            let armed: ArmedReceipt = serde_json::from_value(evidence[*name].clone())
                .map_err(|_| Error::InvalidObservation)?;
            let Some(reference) = armed.journal else {
                return Ok(JournalStatus::LegacyUnjournaled);
            };
            let snapshot =
                Journal::read(spool, &reference).map_err(|_| Error::InvalidObservation)?;
            if armed.version != 1
                || armed.event != "armed"
                || snapshot.intent.request != armed.request
                || evidence["watchdog_pids"][index].as_u64()
                    != Some(u64::from(snapshot.intent.watchdog_pid))
                || snapshot
                    .enrollment
                    .as_ref()
                    .is_some_and(|v| v.cgroup_device != armed.cgroup_device)
            {
                return Err(Error::IdentityMismatch);
            }
            if let Some(report) = snapshot.report {
                if report.armed_boottime_ms != armed.armed_boottime_ms
                    || report.cgroup_device != armed.cgroup_device
                {
                    return Err(Error::IdentityMismatch);
                }
                Ok(JournalStatus::Recorded {
                    journal: reference,
                    report: Box::new(report),
                    recovery: snapshot.recovery.map(Box::new),
                })
            } else if let Some(report) = snapshot.recovery {
                Ok(JournalStatus::Recovered {
                    journal: reference,
                    report: Box::new(report),
                })
            } else {
                Ok(JournalStatus::Unconfirmed { journal: reference })
            }
        })();
        guards.push(result.unwrap_or_else(|error| JournalStatus::Unavailable { error }));
    }
    Ok(JournalObservations {
        guards: guards.try_into().map_err(|_| Error::InvalidObservation)?,
    })
}
