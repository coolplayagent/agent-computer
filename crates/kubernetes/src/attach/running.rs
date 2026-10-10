use super::*;
use agent_computer_sandbox::renewal::{Challenge, ChallengeFrame, Grant, Progress};

#[derive(Debug)]
pub enum ExecutionEvent {
    Renewal(Challenge),
    Complete(StartupObservation),
}

/// One live attach stream. A received challenge does not extend its deadline.
/// This handle cannot be reconstructed from a serialized report or journal.
pub struct ExecutionChannel<'a> {
    channel: StartupChannel<'a>,
    grant: StartupGrant,
    deadline: Instant,
    hard_deadline: Instant,
    progress: Option<Progress>,
    pending: Option<(Challenge, Instant)>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Vec<u8>,
    report: bool,
    finished: bool,
}
impl fmt::Debug for ExecutionChannel<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionChannel")
            .field("pod_uid", &self.channel.pod_uid)
            .finish_non_exhaustive()
    }
}
impl<'a> ExecutionChannel<'a> {
    pub(super) fn new(
        channel: StartupChannel<'a>,
        grant: StartupGrant,
        deadline: Instant,
    ) -> Result<Self> {
        let hard_deadline = channel.created
            + Duration::from_millis(grant.hard_budget_ms.unwrap_or(grant.lease_budget_ms).into());
        let progress = grant
            .hard_budget_ms
            .map(|_| {
                grant.digest().map(|grant_digest| Progress {
                    sequence: 0,
                    grant_digest,
                })
            })
            .transpose()
            .map_err(|_| Error::InvalidCommand)?;
        Ok(Self {
            channel,
            grant,
            deadline,
            hard_deadline,
            progress,
            pending: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: Vec::new(),
            report: false,
            finished: false,
        })
    }
    pub fn pod_uid(&self) -> &str {
        &self.channel.pod_uid
    }

    /// Cancellation-safe while waiting: partial frames and counters remain in
    /// this handle. Callers may select this against authority/guard checks.
    pub async fn next_event(&mut self) -> Result<ExecutionEvent> {
        if self.finished || self.pending.is_some() {
            return Err(Error::MutationUnconfirmed);
        }
        let result = self.receive().await;
        if result.is_err() {
            self.finished = true;
        }
        result.map_err(|_| Error::MutationUnconfirmed)
    }
    async fn receive(&mut self) -> Result<ExecutionEvent> {
        let grace = Duration::from_millis(
            u64::from(self.channel.plan.bootstrap.request.term_grace_ms) + 2000,
        );
        let limit = 8 * self.channel.plan.bootstrap.request.output_limit_bytes + 16384;
        loop {
            if self.report
                && serde_json::from_slice::<RemoteStatus>(&self.status)
                    .is_ok_and(|s| s.status == "Success")
            {
                return self.complete();
            }
            let frame = next(
                &mut self.channel.socket,
                &mut self.channel.frames,
                self.deadline + grace,
            )
            .await?
            .ok_or(Error::InvalidResponse)?;
            match frame[0] {
                1 => {
                    if self.report {
                        return Err(Error::InvalidResponse);
                    }
                    append(&mut self.stdout, &frame[1..], limit)?;
                    if let Some(bytes) = line(&self.stdout)? {
                        if let Ok(control) = ChallengeFrame::parse(bytes) {
                            let progress = self.progress.as_ref().ok_or(Error::InvalidResponse)?;
                            let challenge = control.renewal;
                            if Instant::now() >= self.deadline
                                || Instant::now() >= self.hard_deadline
                                || challenge.sequence != progress.sequence + 1
                                || challenge.startup_grant_digest
                                    != self.grant.digest().map_err(|_| Error::InvalidCommand)?
                            {
                                return Err(Error::IdentityMismatch);
                            }
                            self.pending = Some((challenge.clone(), Instant::now()));
                            self.stdout.clear();
                            return Ok(ExecutionEvent::Renewal(challenge));
                        }
                        // A malformed control frame is rejected by the strict
                        // final envelope parser, never treated as authorization.
                        self.validate_report(bytes)?;
                        self.report = true;
                    }
                }
                2 => append(&mut self.stderr, &frame[1..], DIAGNOSTIC_LIMIT)?,
                3 => append(&mut self.status, &frame[1..], 4096)?,
                255 if frame.len() == 2 && matches!(frame[1], 1..=3) => {}
                _ => return Err(Error::InvalidResponse),
            }
        }
    }
    /// Only call after durable authorization and acknowledgment from both live
    /// node guards. A failed or cancelled write poisons this stream; no resend.
    pub async fn send_renewal(&mut self, grant: &Grant) -> Result<()> {
        if self.finished {
            return Err(Error::MutationUnconfirmed);
        }
        grant.validate().map_err(|_| Error::InvalidCommand)?;
        let (challenge, anchor) = self.pending.as_ref().ok_or(Error::PreconditionFailed)?;
        let deadline =
            (*anchor + Duration::from_millis(grant.lease_budget_ms.into())).min(self.hard_deadline);
        if grant.challenge_digest != challenge.digest().map_err(|_| Error::InvalidCommand)?
            || Instant::now() >= self.deadline
            || Instant::now() >= deadline
            || deadline <= self.deadline
        {
            return Err(Error::PreconditionFailed);
        }
        let progress = Progress {
            sequence: challenge.sequence,
            grant_digest: grant.digest().map_err(|_| Error::InvalidCommand)?,
        };
        let mut bytes = vec![0];
        serde_json::to_writer(&mut bytes, grant).map_err(|_| Error::InvalidCommand)?;
        bytes.push(b'\n');
        self.finished = true;
        send(&mut self.channel.socket, bytes, self.deadline.min(deadline))
            .await
            .map_err(|_| Error::MutationUnconfirmed)?;
        self.deadline = deadline;
        self.progress = Some(progress);
        self.pending = None;
        self.finished = false;
        Ok(())
    }
    fn validate_report(&self, bytes: &[u8]) -> Result<()> {
        let envelope: Envelope =
            serde_json::from_slice(bytes).map_err(|_| Error::InvalidResponse)?;
        let identity: ReportIdentity =
            serde_json::from_str(envelope.report.get()).map_err(|_| Error::InvalidResponse)?;
        let mut request = self.channel.plan.bootstrap.request.clone();
        request.lease_budget_ms = self.grant.lease_budget_ms;
        if envelope.version != self.grant.version
            || envelope.challenge_digest != self.grant.challenge_digest
            || envelope.grant_digest != self.grant.digest().map_err(|_| Error::InvalidCommand)?
            || envelope.renewal != self.progress
            || identity.version != 1
            || identity.execution_id != request.execution_id
            || identity.generation != request.generation
            || identity.request_digest != request.digest().map_err(|_| Error::InvalidCommand)?
        {
            return Err(Error::IdentityMismatch);
        }
        Ok(())
    }
    fn complete(&mut self) -> Result<ExecutionEvent> {
        self.finished = true;
        Ok(ExecutionEvent::Complete(StartupObservation {
            pod_uid: self.channel.pod_uid.clone(),
            report: std::mem::take(&mut self.stdout),
            supervisor_stderr: std::mem::take(&mut self.stderr),
        }))
    }
}
