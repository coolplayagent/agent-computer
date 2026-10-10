//! Root-private control protocol. A durable record can delay expiry enforcement
//! within its immutable ceiling; it cannot reconstruct a live controller handle.
use crate::{Error, MAX_BUDGET_MS, Request, Result, journal::Reference};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_EXECUTION_MS: u64 = 3_630_000;
pub const MAX_RENEWALS: u32 = 4096;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub hard_deadline_boottime_ms: u64,
    pub authority_digest: String,
}
impl Policy {
    pub(crate) fn validate(&self, initial_deadline: u64) -> Result<()> {
        if !digest_valid(&self.authority_digest)
            || self.hard_deadline_boottime_ms < initial_deadline
            || self.hard_deadline_boottime_ms - initial_deadline > MAX_EXECUTION_MS
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub version: u8,
    pub request_digest: String,
    pub sequence: u32,
    pub grant_digest: String,
    pub deadline_boottime_ms: u64,
}
impl Command {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > crate::MAX_REQUEST_BYTES {
            return Err(Error::InvalidRequest);
        }
        let value: Self = serde_json::from_slice(bytes).map_err(|_| Error::InvalidRequest)?;
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<()> {
        if self.version != 1
            || !(1..=MAX_RENEWALS).contains(&self.sequence)
            || !digest_valid(&self.request_digest)
            || !digest_valid(&self.grant_digest)
            || self.deadline_boottime_ms == 0
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u8,
    pub event: String,
    pub journal: Reference,
    pub command: Command,
    pub previous_deadline_boottime_ms: u64,
    pub accepted_boottime_ms: u64,
}
impl Receipt {
    pub fn validate(&self, request: &Request, journal: &Reference) -> Result<()> {
        self.command.validate()?;
        let policy = request.renewal.as_ref().ok_or(Error::InvalidRequest)?;
        if self.version != 1
            || self.event != "renewed"
            || &self.journal != journal
            || self.command.request_digest != request_digest(request)?
            || self.previous_deadline_boottime_ms < request.deadline_boottime_ms
            || (self.command.sequence == 1
                && self.previous_deadline_boottime_ms != request.deadline_boottime_ms)
            || self.accepted_boottime_ms >= self.previous_deadline_boottime_ms
            || self.command.deadline_boottime_ms <= self.previous_deadline_boottime_ms
            || self.command.deadline_boottime_ms
                > self.accepted_boottime_ms.saturating_add(MAX_BUDGET_MS)
            || self.command.deadline_boottime_ms > policy.hard_deadline_boottime_ms
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
}

pub fn request_digest(request: &Request) -> Result<String> {
    let mut h = Sha256::new();
    h.update(b"agent-computer/node-renewal-request-v1\0");
    h.update(serde_json::to_vec(request).map_err(|_| Error::InvalidRequest)?);
    Ok(format!("sha256:{:x}", h.finalize()))
}
fn digest_valid(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The timer state only exists in the original process. Advancing the kernel
/// timer and publishing a journal are separate operations; an acknowledgment is
/// sent only after both. Persistence never runs in the timer thread.
pub(crate) struct State {
    request: Request,
    journal: Reference,
    pub(crate) latest: Option<Receipt>,
}

pub(crate) struct Control {
    pub(crate) state: State,
    input: std::fs::File,
    persist: std::sync::mpsc::SyncSender<Receipt>,
    completed: std::sync::mpsc::Receiver<Result<Receipt>>,
    pending: bool,
    buffer: Vec<u8>,
}
impl Control {
    pub(crate) fn new(
        request: Request,
        journal: crate::journal::Journal,
        timer: &impl rustix::fd::AsFd,
    ) -> Result<Self> {
        use rustix::fs::{self, FileType, OFlags};
        let input = std::fs::File::from(rustix::io::dup(std::io::stdin())?);
        if FileType::from_raw_mode(fs::fstat(&input)?.st_mode) != FileType::Fifo {
            return Err(Error::PipeRequired);
        }
        fs::fcntl_setfl(&input, OFlags::NONBLOCK)?;
        fs::fcntl_setfl(timer, OFlags::NONBLOCK)?;
        let state = State::new(request, journal.reference().clone())?;
        let (persist, incoming) = std::sync::mpsc::sync_channel::<Receipt>(1);
        let (finished, completed) = std::sync::mpsc::sync_channel(1);
        // Detached on expiry, including a stalled fsync. Neither joining this
        // thread nor writing the final journal may delay the cgroup kill.
        std::thread::Builder::new()
            .name("watchdog-journal".into())
            .spawn(move || {
                while let Ok(receipt) = incoming.recv() {
                    let result = journal.renew(&receipt).map(|()| receipt);
                    let failed = result.is_err();
                    if finished.try_send(result).is_err() || failed {
                        break;
                    }
                }
            })
            .map_err(|_| Error::Setup)?;
        Ok(Self {
            state,
            input,
            persist,
            completed,
            pending: false,
            buffer: Vec::new(),
        })
    }
    pub(crate) fn wait(
        &mut self,
        timer: &impl rustix::fd::AsFd,
        output: &impl rustix::fd::AsFd,
    ) -> crate::Trigger {
        loop {
            match self.tick(timer, output) {
                Ok(()) => {}
                Err(Error::LeaseExpired) => return crate::Trigger::Deadline,
                Err(_) => return crate::Trigger::ControlUnavailable,
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    fn tick(
        &mut self,
        timer: &impl rustix::fd::AsFd,
        output: &impl rustix::fd::AsFd,
    ) -> Result<()> {
        use std::sync::mpsc::TryRecvError;
        if crate::boottime_ms() >= self.state.deadline() {
            return Err(Error::LeaseExpired);
        }
        let mut ticks = [0; 8];
        match rustix::io::read(timer, &mut ticks) {
            Ok(_) => return Err(Error::LeaseExpired),
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
            Err(_) => return Err(Error::Setup),
        }
        if self.pending {
            match self.completed.try_recv() {
                Ok(result) => {
                    let receipt = result?;
                    if self.state.latest.as_ref() != Some(&receipt) {
                        return Err(Error::InvalidJournal);
                    }
                    if crate::boottime_ms() >= self.state.deadline() {
                        return Err(Error::LeaseExpired);
                    }
                    crate::write_frame(output, &receipt)?;
                    self.pending = false;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return Err(Error::JournalUnavailable),
            }
        }
        let mut chunk = [0; crate::MAX_REQUEST_BYTES];
        let count = match rustix::io::read(&self.input, &mut chunk) {
            Ok(0) => return Err(Error::OutputUnavailable),
            Ok(count) => count,
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => return Ok(()),
            Err(_) => return Err(Error::Setup),
        };
        if self.pending || count > crate::MAX_REQUEST_BYTES.saturating_sub(self.buffer.len()) {
            return Err(Error::InvalidRequest);
        }
        self.buffer.extend_from_slice(&chunk[..count]);
        if let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            if end + 1 != self.buffer.len() {
                return Err(Error::InvalidRequest);
            }
            let command = Command::parse(&self.buffer[..end])?;
            let receipt = self.state.propose(command, crate::boottime_ms())?;
            renew_timer(timer, receipt.command.deadline_boottime_ms)?;
            self.state.latest = Some(receipt.clone());
            self.persist
                .try_send(receipt)
                .map_err(|_| Error::JournalUnavailable)?;
            self.pending = true;
            self.buffer.clear();
        }
        Ok(())
    }
}

fn renew_timer(timer: &impl rustix::fd::AsFd, deadline: u64) -> Result<()> {
    // settime returns the previous remaining interval atomically with reset.
    // A userspace time check alone can race suspension across the old expiry.
    let previous = crate::reset_timer(timer, deadline)?;
    if previous.it_value.tv_sec == 0 && previous.it_value.tv_nsec == 0 {
        return Err(Error::LeaseExpired);
    }
    Ok(())
}
impl State {
    pub(crate) fn new(request: Request, journal: Reference) -> Result<Self> {
        request
            .renewal
            .as_ref()
            .ok_or(Error::InvalidRequest)?
            .validate(request.deadline_boottime_ms)?;
        Ok(Self {
            request,
            journal,
            latest: None,
        })
    }
    pub(crate) fn deadline(&self) -> u64 {
        self.latest
            .as_ref()
            .map_or(self.request.deadline_boottime_ms, |r| {
                r.command.deadline_boottime_ms
            })
    }
    pub(crate) fn propose(&self, command: Command, now: u64) -> Result<Receipt> {
        if now >= self.deadline() {
            return Err(Error::LeaseExpired);
        }
        if command.sequence != self.latest.as_ref().map_or(1, |r| r.command.sequence + 1) {
            return Err(Error::InvalidRequest);
        }
        if self.latest.as_ref().is_some_and(|r| {
            r.command.grant_digest == command.grant_digest || now < r.accepted_boottime_ms
        }) {
            return Err(Error::InvalidRequest);
        }
        let receipt = Receipt {
            version: 1,
            event: "renewed".into(),
            journal: self.journal.clone(),
            command,
            previous_deadline_boottime_ms: self.deadline(),
            accepted_boottime_ms: now,
        };
        receipt.validate(&self.request, &self.journal)?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> State {
        State::new(
            Request {
                version: 2,
                execution_id: "execution".into(),
                boot_id: "12345678-1234-1234-1234-123456789abc".into(),
                cgroup_path: "workload".into(),
                cgroup_inode: 5,
                deadline_boottime_ms: 30_000,
                renewal: Some(Policy {
                    hard_deadline_boottime_ms: 60_000,
                    authority_digest: format!("sha256:{}", "a".repeat(64)),
                }),
            },
            Reference {
                id: "journal-1".into(),
                device: 4,
                inode: 5,
                intent_digest: format!("sha256:{}", "b".repeat(64)),
            },
        )
        .unwrap()
    }
    fn command(s: &State) -> Command {
        Command {
            version: 1,
            request_digest: request_digest(&s.request).unwrap(),
            sequence: 1,
            grant_digest: format!("sha256:{}", "c".repeat(64)),
            deadline_boottime_ms: 40_000,
        }
    }
    #[test]
    fn fresh_request_bound_sequence_never_replays_or_revives_expiry() {
        let mut s = state();
        let c = command(&s);
        assert!(matches!(
            s.propose(c.clone(), 30_000),
            Err(Error::LeaseExpired)
        ));
        let receipt = s.propose(c.clone(), 10_000).unwrap();
        assert_eq!(s.deadline(), 30_000);
        s.latest = Some(receipt);
        assert_eq!(s.deadline(), 40_000);
        assert!(s.propose(c.clone(), 11_000).is_err());
        let next = Command {
            sequence: 2,
            deadline_boottime_ms: 50_000,
            grant_digest: format!("sha256:{}", "e".repeat(64)),
            ..c
        };
        s.propose(next.clone(), 20_000).unwrap();
        let foreign = Command {
            request_digest: format!("sha256:{}", "d".repeat(64)),
            ..next
        };
        assert!(s.propose(foreign, 20_000).is_err());
    }
    #[test]
    fn no_shortening_overlong_window_or_hard_ceiling_extension() {
        let s = state();
        for deadline in [0, 30_000, 40_001, 60_001, u64::MAX] {
            let c = Command {
                deadline_boottime_ms: deadline,
                ..command(&s)
            };
            assert!(s.propose(c, 10_000).is_err());
        }
        let mut value = serde_json::to_value(command(&s)).unwrap();
        value["renew_ms"] = 1.into();
        assert!(Command::parse(&serde_json::to_vec(&value).unwrap()).is_err());
        assert!(Command::parse(&vec![b' '; 4097]).is_err());
    }
    #[test]
    fn kernel_reset_detects_expiry_even_without_a_userspace_timer_read() {
        let timer = crate::timer(crate::boottime_ms() + 15).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(25));
        assert_eq!(
            renew_timer(&timer, crate::boottime_ms() + 1000),
            Err(Error::LeaseExpired)
        );
        let timer = crate::timer(crate::boottime_ms() + 1000).unwrap();
        renew_timer(&timer, crate::boottime_ms() + 2000).unwrap();
        let remaining = rustix::time::timerfd_gettime(&timer).unwrap();
        assert!(remaining.it_value.tv_sec >= 1);
    }
}
