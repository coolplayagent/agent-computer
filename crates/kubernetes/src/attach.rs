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

mod running;
pub use running::{ExecutionChannel, ExecutionEvent, OutputChunkObservation};

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
    limit: usize,
) -> Result<Option<Vec<u8>>> {
    loop {
        if *frames >= limit || Instant::now() >= deadline {
            return Err(Error::ResponseLimit);
        }
        let message = timeout_at(deadline, socket.next())
            .await
            .map_err(|_| Error::Transport)?;
        *frames += 1;
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
            let frame = next(&mut socket, &mut frames, deadline, MAX_FRAMES)
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
        if plan.pod.candidate.is_some() {
            self.probe_sandbox_storage(&plan.pod).await?;
        }
        Ok(())
    }
}
impl<'a> StartupChannel<'a> {
    pub fn pod_uid(&self) -> &str {
        &self.pod_uid
    }
    pub fn challenge(&self) -> &StartupChallenge {
        &self.challenge
    }

    /// Legacy convenience path. A renewal challenge requires the explicit event
    /// API and fresh external authorization; it is never answered automatically.
    pub async fn run(self, grant: &StartupGrant) -> Result<StartupObservation> {
        let mut running = self.start(grant).await?;
        match running.next_event().await? {
            ExecutionEvent::Complete(observation) => Ok(observation),
            ExecutionEvent::Renewal(_) | ExecutionEvent::Output(_) => {
                Err(Error::MutationUnconfirmed)
            }
        }
    }
    /// Consume the verified startup channel. Version 1 closes stdin immediately;
    /// version 2 keeps the same authenticated stream for one-shot renewal grants.
    pub async fn start(mut self, grant: &StartupGrant) -> Result<ExecutionChannel<'a>> {
        grant.validate().map_err(|_| Error::InvalidCommand)?;
        grant
            .accept(&self.challenge, &self.plan.bootstrap)
            .map_err(|_| Error::IdentityMismatch)?;
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
        if grant.hard_budget_ms.is_none()
            && grant.version != agent_computer_sandbox::STREAMING_PROTOCOL
        {
            send(&mut self.socket, vec![255, 0], send_deadline)
                .await
                .map_err(|_| Error::MutationUnconfirmed)?;
        }
        ExecutionChannel::new(self, grant.clone(), send_deadline)
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
    #[serde(default)]
    renewal: Option<agent_computer_sandbox::renewal::Progress>,
    #[serde(default)]
    stream: Option<agent_computer_sandbox::streaming::Progress>,
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
