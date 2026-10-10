//! Fresh, anchored renewal challenges. These values carry no caller credentials.
//! Only the authenticated controller may deliver a database-authorized response.
use crate::{
    Error, Result,
    startup::{digest, valid_digest},
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const WINDOW_MS: u32 = 30_000;
pub const RENEW_AFTER_MS: u32 = 10_000;
/// One hour of command time plus at most one window for setup and drainage.
pub const MAX_EXECUTION_BUDGET_MS: u32 = 3_630_000;
pub const MAX_RENEWALS: u32 = 4096;
pub const MAX_FRAME_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Challenge {
    pub version: u32,
    pub startup_grant_digest: String,
    pub sequence: u32,
    pub nonce: String,
}
impl Challenge {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let value: Self = parse(bytes)?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || !valid_digest(&self.startup_grant_digest)
            || !(1..=MAX_RENEWALS).contains(&self.sequence)
            || self.nonce.len() != 64
            || !self
                .nonce
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        digest("agent-computer/execution-renewal-challenge-v1", self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub version: u32,
    pub challenge_digest: String,
    /// Computed from fresh DB time after challenge reception. Delivery consumes it.
    pub lease_budget_ms: u32,
}
impl Grant {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let value: Self = parse(bytes)?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || !valid_digest(&self.challenge_digest)
            || !(1..=WINDOW_MS).contains(&self.lease_budget_ms)
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        digest("agent-computer/execution-renewal-grant-v1", self)
    }
}

fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Error::InvalidRequest);
    }
    serde_json::from_slice(bytes).map_err(|_| Error::InvalidRequest)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    /// Zero identifies the original startup grant; positive values identify renewals.
    pub sequence: u32,
    pub grant_digest: String,
}
impl Progress {
    pub fn validate(&self) -> Result<()> {
        if self.sequence > MAX_RENEWALS || !valid_digest(&self.grant_digest) {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
}

/// Bounded stdout frame, disjoint from the final startup report envelope.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChallengeFrame {
    pub renewal: Challenge,
}
impl ChallengeFrame {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let frame: Self = parse(bytes)?;
        frame.renewal.validate()?;
        Ok(frame)
    }
}

struct Pending {
    challenge: Challenge,
    anchor: Instant,
}

/// Process-local monotonic state. Serialized progress cannot reconstruct it.
/// A challenge never extends a lease; expiry takes precedence over a late grant.
pub struct Window {
    startup_grant_digest: String,
    deadline: Instant,
    hard_deadline: Instant,
    renew_at: Instant,
    pending: Option<Pending>,
    progress: Progress,
    expired: bool,
}
impl Window {
    pub fn new(
        startup_grant_digest: String,
        initial_budget_ms: u32,
        hard_budget_ms: u32,
        anchor: Instant,
    ) -> Result<Self> {
        if !valid_digest(&startup_grant_digest)
            || !(1..=WINDOW_MS).contains(&initial_budget_ms)
            || hard_budget_ms < initial_budget_ms
            || hard_budget_ms > MAX_EXECUTION_BUDGET_MS
        {
            return Err(Error::InvalidRequest);
        }
        Ok(Self {
            deadline: anchor + Duration::from_millis(initial_budget_ms.into()),
            hard_deadline: anchor + Duration::from_millis(hard_budget_ms.into()),
            renew_at: renew_at(anchor, initial_budget_ms),
            progress: Progress {
                sequence: 0,
                grant_digest: startup_grant_digest.clone(),
            },
            startup_grant_digest,
            pending: None,
            expired: false,
        })
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn hard_deadline(&self) -> Instant {
        self.hard_deadline
    }
    pub fn progress(&self) -> &Progress {
        &self.progress
    }
    /// Call before writing the challenge. At most one challenge is outstanding.
    pub fn challenge(&mut self, now: Instant) -> Result<Option<Challenge>> {
        self.require_live(now)?;
        if now < self.renew_at || self.pending.is_some() || self.deadline == self.hard_deadline {
            return Ok(None);
        }
        let sequence = self
            .progress
            .sequence
            .checked_add(1)
            .ok_or(Error::InvalidRequest)?;
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(|_| Error::Setup)?;
        let challenge = Challenge {
            version: 1,
            startup_grant_digest: self.startup_grant_digest.clone(),
            sequence,
            nonce: bytes.iter().map(|b| format!("{b:02x}")).collect(),
        };
        challenge.validate()?;
        self.pending = Some(Pending {
            challenge: challenge.clone(),
            anchor: now,
        });
        Ok(Some(challenge))
    }
    pub fn accept(&mut self, grant: &Grant, now: Instant) -> Result<()> {
        self.require_live(now)?;
        grant.validate()?;
        let pending = self.pending.as_ref().ok_or(Error::InvalidRequest)?;
        if now < pending.anchor || grant.challenge_digest != pending.challenge.digest()? {
            return Err(Error::InvalidRequest);
        }
        let deadline = (pending.anchor + Duration::from_millis(grant.lease_budget_ms.into()))
            .min(self.hard_deadline);
        if deadline <= now || deadline <= self.deadline {
            return Err(Error::InvalidRequest);
        }
        let progress = Progress {
            sequence: pending.challenge.sequence,
            grant_digest: grant.digest()?,
        };
        self.deadline = deadline;
        self.renew_at = renew_at(pending.anchor, grant.lease_budget_ms);
        self.progress = progress;
        self.pending = None;
        Ok(())
    }
    fn require_live(&mut self, now: Instant) -> Result<()> {
        if self.expired || now >= self.deadline || now >= self.hard_deadline {
            self.expired = true;
            return Err(Error::LeaseExpired);
        }
        Ok(())
    }
}

/// Only PID 1 owns these descriptors. Polling performs at most one bounded read
/// and one atomic nonblocking write, so control-channel pressure cannot hold up
/// the supervisor's deadline, output drainage or process reaping.
pub(crate) struct Channel {
    pub(crate) window: Window,
    input: std::fs::File,
    output: std::fs::File,
    output_flags: rustix::fs::OFlags,
    buffer: Vec<u8>,
}
impl Channel {
    pub(crate) fn new(window: Window) -> Result<Self> {
        use rustix::{fs, io};
        let input = std::fs::File::from(io::dup(std::io::stdin()).map_err(|_| Error::Setup)?);
        let output = std::fs::File::from(io::dup(std::io::stdout()).map_err(|_| Error::Setup)?);
        let output_flags = fs::fcntl_getfl(&output).map_err(|_| Error::Setup)?;
        fs::fcntl_setfl(&input, fs::OFlags::NONBLOCK).map_err(|_| Error::Setup)?;
        fs::fcntl_setfl(&output, output_flags | fs::OFlags::NONBLOCK).map_err(|_| Error::Setup)?;
        Ok(Self {
            window,
            input,
            output,
            output_flags,
            buffer: Vec::new(),
        })
    }
    pub(crate) fn poll(&mut self) -> Result<()> {
        poll(
            &mut self.window,
            &mut self.buffer,
            &mut self.input,
            &mut self.output,
            &mut Instant::now,
        )
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        // The bounded process-reap path has finished before the final report.
        let _ = rustix::fs::fcntl_setfl(&self.output, self.output_flags);
    }
}
fn poll(
    window: &mut Window,
    buffer: &mut Vec<u8>,
    input: &mut impl std::io::Read,
    output: &mut impl std::io::Write,
    clock: &mut impl FnMut() -> Instant,
) -> Result<()> {
    window.require_live(clock())?;
    let mut chunk = [0u8; MAX_FRAME_BYTES];
    match input.read(&mut chunk) {
        Ok(0) => return Err(Error::Setup),
        Ok(count) => {
            if count > MAX_FRAME_BYTES.saturating_sub(buffer.len()) {
                return Err(Error::InvalidRequest);
            }
            buffer.extend_from_slice(&chunk[..count]);
            if let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                // At most one response to the one outstanding challenge.
                if end + 1 != buffer.len() {
                    return Err(Error::InvalidRequest);
                }
                let grant = Grant::parse(&buffer[..end])?;
                window.accept(&grant, clock())?;
                buffer.clear();
            }
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) => {}
        Err(_) => return Err(Error::Setup),
    }
    if let Some(renewal) = window.challenge(clock())? {
        let mut frame =
            serde_json::to_vec(&ChallengeFrame { renewal }).map_err(|_| Error::Setup)?;
        frame.push(b'\n');
        if frame.len() > MAX_FRAME_BYTES
            || output.write(&frame).map_err(|_| Error::Setup)? != frame.len()
        {
            return Err(Error::Setup);
        }
    }
    Ok(())
}
fn renew_at(anchor: Instant, budget: u32) -> Instant {
    anchor + Duration::from_millis(u64::from((budget / 3).clamp(1, RENEW_AFTER_MS)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn window(anchor: Instant, hard: u32) -> Window {
        Window::new(
            format!("sha256:{}", "a".repeat(64)),
            WINDOW_MS,
            hard,
            anchor,
        )
        .unwrap()
    }
    fn grant(challenge: &Challenge) -> Grant {
        Grant {
            version: 1,
            challenge_digest: challenge.digest().unwrap(),
            lease_budget_ms: WINDOW_MS,
        }
    }
    #[test]
    fn response_delay_is_charged_from_before_the_challenge() {
        let start = Instant::now();
        let mut window = window(start, 120_000);
        assert!(
            window
                .challenge(start + Duration::from_millis(9999))
                .unwrap()
                .is_none()
        );
        let challenge = window
            .challenge(start + Duration::from_secs(10))
            .unwrap()
            .unwrap();
        window
            .accept(&grant(&challenge), start + Duration::from_secs(25))
            .unwrap();
        assert_eq!(window.deadline(), start + Duration::from_secs(40));
        assert_eq!(window.progress().sequence, 1);
    }
    #[test]
    fn pending_challenge_and_late_valid_grant_cannot_revive_expiry() {
        let start = Instant::now();
        let mut window = window(start, 120_000);
        let challenge = window
            .challenge(start + Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(
            window
                .challenge(start + Duration::from_secs(20))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            window.accept(&grant(&challenge), start + Duration::from_secs(30)),
            Err(Error::LeaseExpired)
        );
        assert_eq!(window.deadline(), start + Duration::from_secs(30));
        assert_eq!(window.progress().sequence, 0);
        assert!(matches!(
            window.challenge(start + Duration::from_secs(31)),
            Err(Error::LeaseExpired)
        ));
        assert_eq!(
            window.accept(&grant(&challenge), start + Duration::from_secs(11)),
            Err(Error::LeaseExpired)
        );
    }
    #[test]
    fn no_response_or_replayed_response_can_extend_the_current_window() {
        let start = Instant::now();
        let mut window = window(start, 120_000);
        let challenge = window
            .challenge(start + Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(window.deadline(), start + Duration::from_secs(30));
        let response = grant(&challenge);
        window
            .accept(&response, start + Duration::from_secs(11))
            .unwrap();
        assert_eq!(
            window.accept(&response, start + Duration::from_secs(12)),
            Err(Error::InvalidRequest)
        );
        let next = window
            .challenge(start + Duration::from_secs(20))
            .unwrap()
            .unwrap();
        assert_ne!(next.nonce, challenge.nonce);
        assert_eq!(next.sequence, 2);
        assert_eq!(
            window.accept(&response, start + Duration::from_secs(21)),
            Err(Error::InvalidRequest)
        );
        assert_eq!(window.deadline(), start + Duration::from_secs(40));
    }
    #[test]
    fn another_execution_or_sequence_cannot_authorize_a_renewal() {
        let start = Instant::now();
        let mut window = window(start, 120_000);
        let challenge = window
            .challenge(start + Duration::from_secs(10))
            .unwrap()
            .unwrap();
        for changed in [
            Challenge {
                startup_grant_digest: format!("sha256:{}", "b".repeat(64)),
                ..challenge.clone()
            },
            Challenge {
                sequence: challenge.sequence + 1,
                ..challenge.clone()
            },
            Challenge {
                nonce: "b".repeat(64),
                ..challenge.clone()
            },
        ] {
            assert_eq!(
                window.accept(&grant(&changed), start + Duration::from_secs(11)),
                Err(Error::InvalidRequest)
            );
            assert_eq!(window.deadline(), start + Duration::from_secs(30));
        }
    }
    #[test]
    fn every_extension_remains_bounded_by_the_immutable_hard_limit() {
        let start = Instant::now();
        let mut window = window(start, 35_000);
        let challenge = window
            .challenge(start + Duration::from_secs(10))
            .unwrap()
            .unwrap();
        window
            .accept(&grant(&challenge), start + Duration::from_secs(11))
            .unwrap();
        assert_eq!(window.deadline(), window.hard_deadline());
        assert_eq!(window.deadline(), start + Duration::from_secs(35));
        assert!(
            window
                .challenge(start + Duration::from_secs(20))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            window.challenge(start + Duration::from_secs(35)),
            Err(Error::LeaseExpired)
        ));
    }
    #[test]
    fn rejects_shortened_or_unbounded_grants_without_mutating_progress() {
        let start = Instant::now();
        let mut window = window(start, 120_000);
        let challenge = window
            .challenge(start + Duration::from_secs(10))
            .unwrap()
            .unwrap();
        for lease_budget_ms in [0, 1, 20_000, WINDOW_MS + 1, u32::MAX] {
            let response = Grant {
                lease_budget_ms,
                ..grant(&challenge)
            };
            assert_eq!(
                window.accept(&response, start + Duration::from_secs(11)),
                Err(Error::InvalidRequest)
            );
            assert_eq!(window.progress().sequence, 0);
            assert_eq!(window.deadline(), start + Duration::from_secs(30));
        }
    }
    #[test]
    fn control_frames_reject_unknown_duplicate_and_oversized_input() {
        let start = Instant::now();
        let mut window = window(start, 120_000);
        let challenge = window
            .challenge(start + Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let bytes = serde_json::to_vec(&challenge).unwrap();
        assert_eq!(Challenge::parse(&bytes).unwrap(), challenge);
        let mut value = serde_json::to_value(&challenge).unwrap();
        value["renew_ms"] = 1.into();
        assert!(Challenge::parse(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut repeated = String::from_utf8(bytes).unwrap();
        repeated.insert_str(1, "\"sequence\":1,");
        assert!(Challenge::parse(repeated.as_bytes()).is_err());
        assert!(Challenge::parse(&vec![b' '; MAX_FRAME_BYTES + 1]).is_err());
        let response = grant(&challenge);
        assert_eq!(
            Grant::parse(&serde_json::to_vec(&response).unwrap()).unwrap(),
            response
        );
    }
    #[test]
    fn initial_budget_and_hard_ceiling_require_explicit_bounds() {
        let start = Instant::now();
        for (initial, hard) in [
            (0, 30_000),
            (30_001, 40_000),
            (30_000, 29_999),
            (1, MAX_EXECUTION_BUDGET_MS + 1),
        ] {
            assert!(
                Window::new(format!("sha256:{}", "a".repeat(64)), initial, hard, start).is_err()
            );
        }
        let mut short =
            Window::new(format!("sha256:{}", "a".repeat(64)), 300, 1000, start).unwrap();
        assert!(
            short
                .challenge(start + Duration::from_millis(99))
                .unwrap()
                .is_none()
        );
        assert!(
            short
                .challenge(start + Duration::from_millis(100))
                .unwrap()
                .is_some()
        );
    }

    struct NoInput;
    impl std::io::Read for NoInput {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
    }
    #[test]
    fn fragmented_response_is_bounded_and_expires_during_delivery() {
        let start = Instant::now();
        let mut w = window(start, 120_000);
        let mut buffer = Vec::new();
        let mut output = Vec::new();
        poll(&mut w, &mut buffer, &mut NoInput, &mut output, &mut || {
            start + Duration::from_secs(10)
        })
        .unwrap();
        let challenge = ChallengeFrame::parse(&output[..output.len() - 1])
            .unwrap()
            .renewal;
        let mut bytes = serde_json::to_vec(&grant(&challenge)).unwrap();
        bytes.push(b'\n');
        let mut first = &bytes[..bytes.len() - 1];
        poll(&mut w, &mut buffer, &mut first, &mut output, &mut || {
            start + Duration::from_secs(11)
        })
        .unwrap();
        assert_eq!(w.progress().sequence, 0);
        let mut last = &bytes[bytes.len() - 1..];
        let mut calls = 0;
        assert_eq!(
            poll(&mut w, &mut buffer, &mut last, &mut output, &mut || {
                calls += 1;
                start + Duration::from_secs(if calls == 1 { 29 } else { 30 })
            }),
            Err(Error::LeaseExpired)
        );
        assert_eq!(w.progress().sequence, 0);
        assert_eq!(w.deadline(), start + Duration::from_secs(30));
    }
    #[test]
    fn eof_flood_multiple_frames_and_backpressure_fail_without_extension() {
        let start = Instant::now();
        let mut w = window(start, 120_000);
        let mut buffer = Vec::new();
        let mut output = Vec::new();
        assert_eq!(
            poll(&mut w, &mut buffer, &mut &b""[..], &mut output, &mut || {
                start
            }),
            Err(Error::Setup)
        );
        let challenge = w
            .challenge(start + Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let mut response = serde_json::to_vec(&grant(&challenge)).unwrap();
        response.extend_from_slice(b"\n\n");
        assert_eq!(
            poll(
                &mut w,
                &mut buffer,
                &mut response.as_slice(),
                &mut output,
                &mut || start + Duration::from_secs(11)
            ),
            Err(Error::InvalidRequest)
        );
        buffer.clear();
        let flood = [b' '; MAX_FRAME_BYTES];
        poll(
            &mut w,
            &mut buffer,
            &mut flood.as_slice(),
            &mut output,
            &mut || start + Duration::from_secs(11),
        )
        .unwrap();
        assert_eq!(
            poll(
                &mut w,
                &mut buffer,
                &mut &b" "[..],
                &mut output,
                &mut || start + Duration::from_secs(12)
            ),
            Err(Error::InvalidRequest)
        );
        assert_eq!(w.progress().sequence, 0);
        let mut short_output = [0u8; 1];
        let mut w = window(start, 120_000);
        buffer.clear();
        assert_eq!(
            poll(
                &mut w,
                &mut buffer,
                &mut NoInput,
                &mut short_output.as_mut_slice(),
                &mut || start + Duration::from_secs(10)
            ),
            Err(Error::Setup)
        );
        assert_eq!(w.deadline(), start + Duration::from_secs(30));
    }
}
