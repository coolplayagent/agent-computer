//! Private Browser stdio over Kubernetes v5 attach. No exec, reconnect or retry.
use crate::{BrowserPlan, BrowserRuntimeIdentity, Client, Error, PodObservation, Result, attach};
use agent_computer_browser::{Session, Transport};
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

pub struct BrowserTransport {
    socket: attach::Socket,
    frames: usize,
    stderr_bytes: usize,
}
pub struct BrowserChannel {
    runtime: BrowserRuntimeIdentity,
    session: Session<BrowserTransport>,
}
impl BrowserChannel {
    pub fn runtime(&self) -> &BrowserRuntimeIdentity {
        &self.runtime
    }
    pub fn session_mut(&mut self) -> &mut Session<BrowserTransport> {
        &mut self.session
    }
}
impl Client {
    /// Observe exact Pod/Node/boot/container before and after the nonce exchange.
    /// Attaching never permits Chromium launch: the caller must first establish
    /// a live node guard and durable admission before answering its callback.
    pub async fn attach_browser(
        &self,
        plan: &BrowserPlan,
        observed: &PodObservation,
    ) -> Result<BrowserChannel> {
        let deadline = Instant::now() + Duration::from_secs(15);
        timeout_at(deadline, async {
            if observed.binding != plan.pod.binding {
                return Err(Error::IdentityMismatch);
            }
            let before = self.observe_browser_runtime(plan, observed.uid()).await?;
            let socket = self.open_attach(plan.pod_plan()).await?;
            let transport = BrowserTransport {
                socket,
                frames: 0,
                stderr_bytes: 0,
            };
            let session = Session::connect(transport, plan.binding().clone())
                .await
                .map_err(|_| Error::InvalidResponse)?;
            let after = self.observe_browser_runtime(plan, observed.uid()).await?;
            if before.pod() != after.pod() {
                return Err(Error::IdentityMismatch);
            }
            Ok(BrowserChannel {
                runtime: after,
                session,
            })
        })
        .await
        .map_err(|_| Error::Transport)?
    }
}
fn transport_error(error: Error) -> agent_computer_browser::Error {
    match error {
        Error::ResponseLimit => agent_computer_browser::Error::Limit,
        Error::Transport => agent_computer_browser::Error::Transport,
        _ => agent_computer_browser::Error::Protocol,
    }
}
impl Transport for BrowserTransport {
    async fn write(
        &mut self,
        bytes: &[u8],
        deadline: Instant,
    ) -> agent_computer_browser::Result<()> {
        // Leave room for the WebSocket header in the existing 8 KiB bounded
        // write buffer. Kubernetes channel bytes are outside the JSON framing.
        for chunk in bytes.chunks(4096) {
            let mut frame = Vec::with_capacity(chunk.len() + 1);
            frame.push(0);
            frame.extend_from_slice(chunk);
            attach::send(&mut self.socket, frame, deadline)
                .await
                .map_err(transport_error)?;
        }
        Ok(())
    }
    async fn read(&mut self, deadline: Instant) -> agent_computer_browser::Result<Vec<u8>> {
        loop {
            let frame = attach::next(&mut self.socket, &mut self.frames, deadline, 32768)
                .await
                .map_err(transport_error)?
                .ok_or(agent_computer_browser::Error::Closed)?;
            match frame[0] {
                1 if frame.len() > 1 => return Ok(frame[1..].to_vec()),
                1 => {}
                2 => {
                    self.stderr_bytes = self
                        .stderr_bytes
                        .checked_add(frame.len() - 1)
                        .ok_or(agent_computer_browser::Error::Limit)?;
                    if self.stderr_bytes > 65536 {
                        return Err(agent_computer_browser::Error::Limit);
                    }
                }
                // Exit/EOF is not a process fence or an accepted operation
                // result. A missing JSON result remains uncertain.
                3 => return Err(agent_computer_browser::Error::Closed),
                255 if frame.as_slice() == [255, 2] => {}
                255 if frame.as_slice() == [255, 1] => {
                    return Err(agent_computer_browser::Error::Closed);
                }
                _ => return Err(agent_computer_browser::Error::Protocol),
            }
        }
    }
}
