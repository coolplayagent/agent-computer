//! Bounded Kubernetes v5 remote-command attach; no exec, TTY or retry fallback.
use crate::{Client, Error, PodObservation, PodPhase, Result, StartupSandboxPlan};
use agent_computer_sandbox::{StartupChallenge, StartupGrant, StartupHello};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Version, header::HeaderMap};
use serde::Deserialize;
use std::{fmt, time::Duration};
use tokio::time::{Instant, timeout_at};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        self, Message,
        protocol::{Role, WebSocketConfig},
    },
};

const PROTOCOL: &str = "v5.channel.k8s.io";
const FRAME_LIMIT: usize = 65536;
const CHALLENGE_LIMIT: usize = 4096;
const DIAGNOSTIC_LIMIT: usize = 65536;
const MAX_FRAMES: usize = 4096;
type Socket = WebSocketStream<reqwest::Upgraded>;

/// A single live channel to one verified Pod. It cannot be cloned/deserialized.
/// Dropping it closes transport, not a process or a Candidate writer lease.
pub struct StartupChannel<'a> {
    client: &'a Client,
    plan: &'a StartupSandboxPlan,
    pod_uid: String,
    socket: Socket,
    challenge: StartupChallenge,
    created: Instant,
    frames: usize,
}
impl fmt::Debug for StartupChannel<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartupChannel")
            .field("pod_uid", &self.pod_uid)
            .finish_non_exhaustive()
    }
}

/// Bounded raw local report, with correlation checked. This is NOT accepted
/// completion or evidence of physical process termination/storage drainage.
pub struct StartupObservation {
    pod_uid: String,
    report: Vec<u8>,
    supervisor_stderr: Vec<u8>,
}
impl StartupObservation {
    pub fn pod_uid(&self) -> &str {
        &self.pod_uid
    }
    pub fn report_bytes(&self) -> &[u8] {
        &self.report
    }
    pub fn supervisor_stderr(&self) -> &[u8] {
        &self.supervisor_stderr
    }
}
impl fmt::Debug for StartupObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartupObservation")
            .field("pod_uid", &self.pod_uid)
            .field("report_bytes", &self.report.len())
            .field("stderr_bytes", &self.supervisor_stderr.len())
            .finish()
    }
}

fn header<'a>(headers: &'a HeaderMap, key: &str) -> Result<&'a str> {
    let mut values = headers.get_all(key).iter();
    let value = values
        .next()
        .ok_or(Error::InvalidResponse)?
        .to_str()
        .map_err(|_| Error::InvalidResponse)?;
    if values.next().is_some() {
        return Err(Error::InvalidResponse);
    }
    Ok(value)
}
fn append(target: &mut Vec<u8>, data: &[u8], limit: usize) -> Result<()> {
    if data.len() > limit.saturating_sub(target.len()) {
        return Err(Error::ResponseLimit);
    }
    target.extend_from_slice(data);
    Ok(())
}
fn line(bytes: &[u8]) -> Result<Option<&[u8]>> {
    if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
        if bytes[end + 1..].iter().any(|b| !b.is_ascii_whitespace()) {
            return Err(Error::InvalidResponse);
        }
        Ok(Some(&bytes[..end]))
    } else {
        Ok(None)
    }
}
async fn next(
    socket: &mut Socket,
    frames: &mut usize,
    deadline: Instant,
) -> Result<Option<Vec<u8>>> {
    loop {
        if *frames >= MAX_FRAMES || Instant::now() >= deadline {
            return Err(Error::ResponseLimit);
        }
        *frames += 1;
        let message = timeout_at(deadline, socket.next())
            .await
            .map_err(|_| Error::Transport)?;
        match message {
            Some(Ok(Message::Binary(bytes))) if !bytes.is_empty() => {
                return Ok(Some(bytes.to_vec()));
            }
            Some(Ok(Message::Close(_))) | None => return Ok(None),
            Some(Ok(Message::Ping(_))) => {
                timeout_at(deadline, socket.flush())
                    .await
                    .map_err(|_| Error::Transport)?
                    .map_err(|_| Error::Transport)?;
            }
            Some(Ok(Message::Pong(_))) => {}
            Some(Err(tungstenite::Error::ConnectionClosed)) => return Ok(None),
            _ => return Err(Error::InvalidResponse),
        }
    }
}
async fn send(socket: &mut Socket, bytes: Vec<u8>, deadline: Instant) -> Result<()> {
    timeout_at(deadline, socket.send(Message::Binary(bytes.into())))
        .await
        .map_err(|_| Error::Transport)?
        .map_err(|_| Error::Transport)
}
impl Client {
    /// Observe the exact admitted Pod before attach and again after its challenge.
    /// Kubernetes attach itself has no UID query precondition. The namespace and
    /// attach RBAC must be exclusive to trusted controllers; restartPolicy=Never
    /// and stdinOnce are required. No tenant can provide a free-form exec command.
    pub async fn attach_startup<'a>(
        &'a self,
        plan: &'a StartupSandboxPlan,
        observed: &PodObservation,
    ) -> Result<StartupChannel<'a>> {
        let created = Instant::now();
        let deadline = created + Duration::from_secs(15);
        timeout_at(
            deadline,
            self.attach_inner(plan, observed, created, deadline),
        )
        .await
        .map_err(|_| Error::Transport)?
    }
    async fn attach_inner<'a>(
        &'a self,
        plan: &'a StartupSandboxPlan,
        observed: &PodObservation,
        created: Instant,
        deadline: Instant,
    ) -> Result<StartupChannel<'a>> {
        self.check_plan(&plan.pod)?;
        if observed.binding != plan.pod.binding {
            return Err(Error::IdentityMismatch);
        }
        self.running(plan, &observed.uid).await?;
        let mut url = self
            .endpoint
            .join(&format!("{}/attach", self.pod_path(&plan.pod, true)))
            .map_err(|_| Error::InvalidConfiguration)?;
        url.query_pairs_mut().extend_pairs([
            ("container", "sandbox"),
            ("stdin", "true"),
            ("stdout", "true"),
            ("stderr", "true"),
            ("tty", "false"),
        ]);
        let key = tungstenite::handshake::client::generate_key();
        let response = self
            .http
            .get(url)
            .version(Version::HTTP_11)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", &key)
            .header("Sec-WebSocket-Protocol", PROTOCOL)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(Error::AccessDenied);
        }
        if response.status().as_u16() != 101 {
            return Err(Error::ApiRejected);
        }
        let headers = response.headers();
        if !header(headers, "upgrade")?.eq_ignore_ascii_case("websocket")
            || !header(headers, "connection")?
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("upgrade"))
            || header(headers, "sec-websocket-protocol")? != PROTOCOL
            || header(headers, "sec-websocket-accept")?
                != tungstenite::handshake::derive_accept_key(key.as_bytes())
            || headers.contains_key("sec-websocket-extensions")
        {
            return Err(Error::InvalidResponse);
        }
        let upgraded = response.upgrade().await.map_err(|_| Error::Transport)?;
        let config = WebSocketConfig::default()
            .read_buffer_size(4096)
            .write_buffer_size(0)
            .max_write_buffer_size(8192)
            .max_message_size(Some(FRAME_LIMIT))
            .max_frame_size(Some(FRAME_LIMIT));
        let mut socket = Socket::from_raw_socket(upgraded, Role::Client, Some(config)).await;
        let mut hello = vec![0];
        serde_json::to_writer(
            &mut hello,
            &StartupHello::for_bootstrap(&plan.bootstrap).map_err(|_| Error::InvalidCommand)?,
        )
        .map_err(|_| Error::InvalidCommand)?;
        hello.push(b'\n');
        send(&mut socket, hello, deadline).await?;
        let mut buffer = Vec::new();
        let mut frames = 0;
        let challenge = loop {
            let frame = next(&mut socket, &mut frames, deadline)
                .await?
                .ok_or(Error::InvalidResponse)?;
            if frame[0] != 1 {
                return Err(Error::InvalidResponse);
            }
            append(&mut buffer, &frame[1..], CHALLENGE_LIMIT)?;
            if let Some(bytes) = line(&buffer)? {
                break StartupChallenge::parse(bytes).map_err(|_| Error::InvalidResponse)?;
            }
        };
        if !challenge
            .matches(&plan.bootstrap)
            .map_err(|_| Error::InvalidResponse)?
        {
            return Err(Error::IdentityMismatch);
        }
        self.running(plan, &observed.uid).await?;
        Ok(StartupChannel {
            client: self,
            plan,
            pod_uid: observed.uid.clone(),
            socket,
            challenge,
            created,
            frames,
        })
    }
    async fn running(&self, plan: &StartupSandboxPlan, uid: &str) -> Result<()> {
        let current = self
            .observe(&plan.pod, Some(uid))
            .await?
            .ok_or(Error::PreconditionFailed)?;
        if current.phase() != PodPhase::Running {
            return Err(Error::PreconditionFailed);
        }
        Ok(())
    }
}
impl StartupChannel<'_> {
    pub fn pod_uid(&self) -> &str {
        &self.pod_uid
    }
    pub fn challenge(&self) -> &StartupChallenge {
        &self.challenge
    }

    /// Consume this channel and send one fresh, database-authorized grant, then
    /// close stdin with v5's per-stream close. Any write/collection uncertainty
    /// is MutationUnconfirmed. There is no retry, reconnect, exec or fallback.
    /// An external watchdog must remain independent of this future and channel.
    pub async fn run(mut self, grant: &StartupGrant) -> Result<StartupObservation> {
        grant.validate().map_err(|_| Error::InvalidCommand)?;
        if grant.challenge_digest
            != self
                .challenge
                .digest()
                .map_err(|_| Error::InvalidResponse)?
            || grant.lease_budget_ms > self.plan.bootstrap.request.lease_budget_ms
        {
            return Err(Error::IdentityMismatch);
        }
        let send_deadline = self.created + Duration::from_millis(grant.lease_budget_ms.into());
        timeout_at(send_deadline, self.client.running(self.plan, &self.pod_uid))
            .await
            .map_err(|_| Error::PreconditionFailed)??;
        if Instant::now() >= send_deadline {
            return Err(Error::PreconditionFailed);
        }
        let mut bytes = vec![0];
        serde_json::to_writer(&mut bytes, grant).map_err(|_| Error::InvalidCommand)?;
        bytes.push(b'\n');
        send(&mut self.socket, bytes, send_deadline)
            .await
            .map_err(|_| Error::MutationUnconfirmed)?;
        send(&mut self.socket, vec![255, 0], send_deadline)
            .await
            .map_err(|_| Error::MutationUnconfirmed)?;
        let deadline = send_deadline
            + Duration::from_millis(u64::from(self.plan.bootstrap.request.term_grace_ms) + 2000);
        self.collect(grant, deadline)
            .await
            .map_err(|_| Error::MutationUnconfirmed)
    }
    async fn collect(
        mut self,
        grant: &StartupGrant,
        deadline: Instant,
    ) -> Result<StartupObservation> {
        let mut report = Vec::new();
        let mut stderr = Vec::new();
        let mut status = Vec::new();
        let limit = 8 * self.plan.bootstrap.request.output_limit_bytes + 16384;
        while let Some(frame) = next(&mut self.socket, &mut self.frames, deadline).await? {
            match frame[0] {
                1 => {
                    append(&mut report, &frame[1..], limit)?;
                    line(&report)?;
                }
                2 => append(&mut stderr, &frame[1..], DIAGNOSTIC_LIMIT)?,
                3 => append(&mut status, &frame[1..], 4096)?,
                255 if frame.len() == 2 && matches!(frame[1], 1..=3) => {}
                _ => return Err(Error::InvalidResponse),
            }
            // Status Success means the attach operation ended, not process
            // success. Stop after both complete channels; no fence is inferred.
            if line(&report)?.is_some()
                && serde_json::from_slice::<RemoteStatus>(&status)
                    .is_ok_and(|v| v.status == "Success")
            {
                break;
            }
        }
        let status: RemoteStatus =
            serde_json::from_slice(&status).map_err(|_| Error::InvalidResponse)?;
        if status.status != "Success" {
            return Err(Error::ApiRejected);
        }
        let bytes = line(&report)?.ok_or(Error::InvalidResponse)?;
        let envelope: Envelope =
            serde_json::from_slice(bytes).map_err(|_| Error::InvalidResponse)?;
        let identity: ReportIdentity =
            serde_json::from_str(envelope.report.get()).map_err(|_| Error::InvalidResponse)?;
        let mut request = self.plan.bootstrap.request.clone();
        request.lease_budget_ms = grant.lease_budget_ms;
        if envelope.version != 1
            || envelope.challenge_digest != grant.challenge_digest
            || envelope.grant_digest != grant.digest().map_err(|_| Error::InvalidCommand)?
            || identity.version != 1
            || identity.execution_id != request.execution_id
            || identity.generation != request.generation
            || identity.request_digest != request.digest().map_err(|_| Error::InvalidCommand)?
        {
            return Err(Error::IdentityMismatch);
        }
        Ok(StartupObservation {
            pod_uid: self.pod_uid,
            report,
            supervisor_stderr: stderr,
        })
    }
}
#[derive(Deserialize)]
struct RemoteStatus {
    status: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    challenge_digest: String,
    grant_digest: String,
    report: Box<serde_json::value::RawValue>,
}
// Only correlation is checked here. Result semantics and durable acceptance are
// the collector's responsibility; ignoring fields does not attest their validity.
#[derive(Deserialize)]
struct ReportIdentity {
    version: u32,
    execution_id: String,
    generation: u64,
    request_digest: String,
}
