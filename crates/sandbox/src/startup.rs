//! One-shot, credential-free startup over a trusted runtime attach channel.
use crate::{Error, MAX_REQUEST_BYTES, Report, Request, Result, supervisor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    time::{Duration, Instant},
};

pub const STARTUP_PROTOCOL: u32 = 1;
pub const STARTUP_WAIT_MS: u32 = 30000;

/// Immutable, operator-delivered bootstrap. Its lease budget is a ceiling only;
/// a fresh database-authorized grant is required before spawning any child.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    pub version: u32,
    pub intent_digest: String,
    pub request: Request,
}
impl Bootstrap {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let value: Self = bounded(bytes)?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != STARTUP_PROTOCOL || !valid_digest(&self.intent_digest) {
            return Err(Error::InvalidRequest);
        }
        self.request.validate()
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        digest("agent-computer/sandbox-bootstrap-v1", self)
    }
}

/// Non-secret correlation challenge emitted by the trusted PID 1 before launch.
/// Possessing it is not authorization; the attach transport must authenticate
/// the immutable runtime instance, trusted supervisor and original admission.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupChallenge {
    pub version: u32,
    pub execution_id: String,
    pub generation: u64,
    pub bootstrap_digest: String,
    pub nonce: String,
}
impl StartupChallenge {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let value: Self = bounded(bytes)?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != STARTUP_PROTOCOL
            || self.execution_id.is_empty()
            || self.execution_id.len() > 128
            || !self
                .execution_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            || !(1..=i64::MAX as u64).contains(&self.generation)
            || !valid_digest(&self.bootstrap_digest)
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
        digest("agent-computer/sandbox-challenge-v1", self)
    }
    pub fn matches(&self, bootstrap: &Bootstrap) -> Result<bool> {
        self.validate()?;
        Ok(self.execution_id == bootstrap.request.execution_id
            && self.generation == bootstrap.request.generation
            && self.bootstrap_digest == bootstrap.digest()?)
    }
}

/// Response from the trusted control plane. No credentials travel into the Pod.
/// The budget must be computed from fresh database time AFTER reading this
/// challenge. The runtime charges it from BEFORE emitting the challenge, so all
/// attach/database/response delay reduces execution time without clock sync.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupGrant {
    pub version: u32,
    pub challenge_digest: String,
    pub lease_budget_ms: u32,
}
impl StartupGrant {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let grant: Self = bounded(bytes)?;
        grant.validate()?;
        Ok(grant)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != STARTUP_PROTOCOL
            || !valid_digest(&self.challenge_digest)
            || !(1..=STARTUP_WAIT_MS).contains(&self.lease_budget_ms)
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        digest("agent-computer/sandbox-startup-grant-v1", self)
    }
    fn accept(&self, challenge: &StartupChallenge, bootstrap: &Bootstrap) -> Result<()> {
        self.validate()?;
        if self.challenge_digest != challenge.digest()?
            || self.lease_budget_ms > bootstrap.request.lease_budget_ms
        {
            return Err(Error::InvalidRequest);
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupReport {
    pub version: u32,
    pub challenge_digest: String,
    pub grant_digest: String,
    pub report: Report,
}

fn bounded<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(Error::InvalidRequest);
    }
    serde_json::from_slice(bytes).map_err(|_| Error::InvalidRequest)
}
fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn digest<T: Serialize>(domain: &str, value: &T) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(b"\0");
    hash.update(serde_json::to_vec(value).map_err(|_| Error::InvalidRequest)?);
    Ok(format!("sha256:{:x}", hash.finalize()))
}

/// Attach clients send this non-authorizing hello before PID 1 emits a challenge.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupHello {
    pub version: u32,
    pub bootstrap_digest: String,
}
impl StartupHello {
    pub fn for_bootstrap(bootstrap: &Bootstrap) -> Result<Self> {
        Ok(Self {
            version: STARTUP_PROTOCOL,
            bootstrap_digest: bootstrap.digest()?,
        })
    }
}

struct Input {
    stdin: std::io::Stdin,
    term: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}
impl Input {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        let stdin = std::io::stdin();
        rustix::fs::fcntl_setfl(&stdin, rustix::fs::OFlags::NONBLOCK).map_err(|_| Error::Setup)?;
        Ok(Self {
            stdin,
            term: signal(SignalKind::terminate()).map_err(|_| Error::Setup)?,
            interrupt: signal(SignalKind::interrupt()).map_err(|_| Error::Setup)?,
            hangup: signal(SignalKind::hangup()).map_err(|_| Error::Setup)?,
        })
    }
    async fn read(&mut self, anchor: Instant) -> Result<Vec<u8>> {
        let mut input = Vec::new();
        loop {
            if anchor.elapsed() >= Duration::from_millis(STARTUP_WAIT_MS.into()) {
                return Err(Error::StartupExpired);
            }
            let mut chunk = [0u8; 4096];
            match self.stdin.lock().read(&mut chunk) {
                Ok(0) => return Err(Error::InvalidRequest),
                Ok(n) => {
                    input.extend_from_slice(&chunk[..n]);
                    if input.len() > MAX_REQUEST_BYTES {
                        return Err(Error::InvalidRequest);
                    }
                    if let Some(end) = input.iter().position(|b| *b == b'\n') {
                        if input[end + 1..].iter().any(|b| !b.is_ascii_whitespace()) {
                            return Err(Error::InvalidRequest);
                        }
                        return Ok(input[..end].to_vec());
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(Error::Setup),
            }
            tokio::select! {
                _=self.term.recv()=>return Err(Error::StartupCancelled),
                _=self.interrupt.recv()=>return Err(Error::StartupCancelled),
                _=self.hangup.recv()=>return Err(Error::StartupCancelled),
                _=tokio::time::sleep(Duration::from_millis(5))=>{},
            }
        }
    }
}

/// Immediate challenge mode for a pre-attached trusted local channel.
pub async fn startup(bootstrap: Bootstrap) -> Result<StartupReport> {
    serve(bootstrap, false).await
}
/// Attach mode waits for a bounded hello before emitting its one-shot challenge.
/// This avoids losing the challenge before Kubernetes connects container stdout.
pub async fn startup_attached(bootstrap: Bootstrap) -> Result<StartupReport> {
    serve(bootstrap, true).await
}
async fn serve(bootstrap: Bootstrap, attached: bool) -> Result<StartupReport> {
    bootstrap.validate()?;
    let namespace = supervisor::NamespaceInit::check()?;
    let mut input = Input::new()?;
    if attached {
        let hello: StartupHello = bounded(&input.read(Instant::now()).await?)?;
        if hello.version != STARTUP_PROTOCOL || hello.bootstrap_digest != bootstrap.digest()? {
            return Err(Error::InvalidRequest);
        }
    }
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|_| Error::Setup)?;
    let challenge = StartupChallenge {
        version: STARTUP_PROTOCOL,
        execution_id: bootstrap.request.execution_id.clone(),
        generation: bootstrap.request.generation,
        bootstrap_digest: bootstrap.digest()?,
        nonce: random.iter().map(|b| format!("{b:02x}")).collect(),
    };
    // This instant MUST precede challenge emission and is never reset by a grant.
    let anchor = Instant::now();
    {
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, &challenge).map_err(|_| Error::Setup)?;
        stdout
            .write_all(b"\n")
            .and_then(|_| stdout.flush())
            .map_err(|_| Error::Setup)?;
    }
    let grant = StartupGrant::parse(&input.read(anchor).await?)?;
    grant.accept(&challenge, &bootstrap)?;
    if anchor.elapsed() >= Duration::from_millis(grant.lease_budget_ms.into()) {
        return Err(Error::StartupExpired);
    }
    let challenge_digest = challenge.digest()?;
    let grant_digest = grant.digest()?;
    let mut request = bootstrap.request;
    request.lease_budget_ms = grant.lease_budget_ms;
    let report = supervisor::run_anchored(request, namespace, anchor).await?;
    Ok(StartupReport {
        version: STARTUP_PROTOCOL,
        challenge_digest,
        grant_digest,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bootstrap() -> Bootstrap {
        Bootstrap {
            version: 1,
            intent_digest: format!("sha256:{}", "a".repeat(64)),
            request: Request {
                execution_id: "exec_1".into(),
                generation: 2,
                argv: vec!["/bin/true".into()],
                cwd: String::new(),
                timeout_seconds: 10,
                lease_budget_ms: 30000,
                term_grace_ms: 100,
                output_limit_bytes: 10,
            },
        }
    }
    #[test]
    fn startup_binds_nonce_bootstrap_and_bounded_budget() {
        let bootstrap = bootstrap();
        let mut challenge = StartupChallenge {
            version: 1,
            execution_id: "exec_1".into(),
            generation: 2,
            bootstrap_digest: bootstrap.digest().unwrap(),
            nonce: "b".repeat(64),
        };
        let grant = StartupGrant {
            version: 1,
            challenge_digest: challenge.digest().unwrap(),
            lease_budget_ms: 300,
        };
        assert!(challenge.matches(&bootstrap).unwrap());
        grant.accept(&challenge, &bootstrap).unwrap();
        challenge.nonce = "c".repeat(64);
        assert!(grant.accept(&challenge, &bootstrap).is_err());
        challenge.generation += 1;
        assert!(!challenge.matches(&bootstrap).unwrap());
        let mut changed = bootstrap.clone();
        changed.intent_digest = format!("sha256:{}", "d".repeat(64));
        assert_ne!(changed.digest().unwrap(), bootstrap.digest().unwrap());
        changed.request.argv.push("different".into());
        assert!(!challenge.matches(&changed).unwrap());
    }
    #[test]
    fn startup_rejects_invalid_versions_duplicates_and_unbounded_frames() {
        for bytes in [
            br#"{"version":2,"challenge_digest":"bad","lease_budget_ms":1}"#.to_vec(),
            br#"{"version":1,"version":1}"#.to_vec(),
            vec![b' '; 65537],
        ] {
            assert!(StartupGrant::parse(&bytes).is_err());
        }
        let mut value = bootstrap();
        value.version = 2;
        assert!(value.digest().is_err());
        value.version = 1;
        value.intent_digest = "x".repeat(71);
        assert!(value.digest().is_err());
        assert!(
            StartupGrant {
                version: 1,
                challenge_digest: format!("sha256:{}", "a".repeat(64)),
                lease_budget_ms: 0
            }
            .validate()
            .is_err()
        );
    }
    #[tokio::test]
    async fn startup_refuses_the_host_without_emitting_a_challenge() {
        assert!(matches!(
            startup(bootstrap()).await,
            Err(Error::IsolationRequired)
        ));
    }
}
