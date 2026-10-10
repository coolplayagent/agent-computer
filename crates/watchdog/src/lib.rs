//! Trusted Linux node component. Local kernel observations are not durable fences.
//! Journals retain observations, never Kubernetes authority or a restart permit.
#![forbid(unsafe_code)]

pub mod admission;
mod cgroup;
pub mod journal;
pub mod reaper;
pub mod renewal;
mod request;
pub mod termination;

pub use request::{MAX_BUDGET_MS, MAX_REQUEST_BYTES, Request};
use rustix::{
    fd::{AsFd, OwnedFd},
    time,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum Error {
    InvalidRequest,
    IdentityMismatch,
    RootRequired,
    CgroupRequired,
    UntrustedCgroup,
    PipeRequired,
    Setup,
    KillFailed,
    ObservationFailed,
    DrainTimeout,
    OutputUnavailable,
    JournalUnavailable,
    UntrustedJournal,
    InvalidJournal,
    ReaperAlreadyRunning,
    ReaperUnavailable,
    LeaseExpired,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "node watchdog: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<rustix::io::Errno> for Error {
    fn from(_: rustix::io::Errno) -> Self {
        Self::Setup
    }
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum Trigger {
    Deadline,
    ReceiptUnavailable,
    TimerFailure,
    Recovery,
    ControlUnavailable,
}

#[derive(Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum Observation {
    EmptyObserved,
    Unknown,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub version: u8,
    pub request: Request,
    pub cgroup_device: u64,
    pub armed_boottime_ms: u64,
    pub kill_boottime_ms: u64,
    pub observed_boottime_ms: u64,
    pub trigger: Trigger,
    pub observation: Observation,
    pub error: Option<Error>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<journal::Reference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renewal: Option<renewal::Receipt>,
}

#[derive(Serialize)]
struct Armed<'a> {
    version: u8,
    event: &'static str,
    request: &'a Request,
    cgroup_device: u64,
    armed_boottime_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    journal: Option<&'a journal::Reference>,
}

/// Runs outside the target cgroup. The trusted caller must prevent migration into
/// or out of that tree and launch this process independently from its controller.
/// Output must be a pipe; neither closed readers nor backpressure may delay kill.
pub fn run(request: Request, output: &impl AsFd) -> Result<Report> {
    run_inner(request, output, None)
}

/// Journal setup precedes arming. Completion IO only occurs after termination
/// and bounded observation, so journal backpressure cannot delay the kill.
pub fn run_journaled(
    request: Request,
    output: &impl AsFd,
    journal: journal::Journal,
) -> Result<Report> {
    if &request != journal.request() {
        return Err(Error::InvalidJournal);
    }
    let report = run_inner(request, output, Some(&journal))?;
    journal.complete(&report)?;
    Ok(report)
}

fn run_inner(
    request: Request,
    output: &impl AsFd,
    journal: Option<&journal::Journal>,
) -> Result<Report> {
    if request.renewal.is_some() && journal.is_none() {
        return Err(Error::InvalidRequest);
    }
    if !rustix::process::geteuid().is_root() {
        return Err(Error::RootRequired);
    }
    prepare_output(output)?;
    let boot_id = cgroup::read_small("/proc/sys/kernel/random/boot_id")?;
    let own = cgroup::read_small("/proc/self/cgroup")?;
    let own = own
        .strip_prefix("0::")
        .ok_or(Error::CgroupRequired)?
        .trim_end_matches('\n');
    request.check_node(boot_id.trim_end(), boottime_ms(), own)?;
    let group = cgroup::Cgroup::open(&request)?;
    // Only validated kernel identities are enrolled. This IO precedes timer
    // arming/authorization and consumes the original, unrenewable budget.
    if let Some(journal) = journal {
        journal.enroll(group.device)?;
    }
    let timer = timer(request.deadline_boottime_ms)?;
    let mut control = if request.renewal.is_some() {
        Some(renewal::Control::new(
            request.clone(),
            journal.ok_or(Error::InvalidJournal)?.duplicate()?,
            &timer,
        )?)
    } else {
        None
    };
    let journal = journal.map(journal::Journal::reference);
    let armed_boottime_ms = boottime_ms();
    let armed = Armed {
        version: 1,
        event: "armed",
        request: &request,
        cgroup_device: group.device,
        armed_boottime_ms,
        journal,
    };
    let trigger = if write_frame(output, &armed).is_err() {
        Trigger::ReceiptUnavailable
    } else if let Some(control) = &mut control {
        control.wait(&timer, output)
    } else if wait(&timer).is_err() {
        Trigger::TimerFailure
    } else {
        Trigger::Deadline
    };
    let kill_boottime_ms = boottime_ms();
    let result = group.kill().and_then(|()| observe_empty(&group));
    Ok(Report {
        version: 1,
        request,
        cgroup_device: group.device,
        armed_boottime_ms,
        kill_boottime_ms,
        observed_boottime_ms: boottime_ms(),
        trigger,
        observation: if result.is_ok() {
            Observation::EmptyObserved
        } else {
            Observation::Unknown
        },
        error: result.err(),
        journal: journal.cloned(),
        renewal: control.and_then(|c| c.state.latest),
    })
}

pub fn boottime_ms() -> u64 {
    let now = time::clock_gettime(time::ClockId::Boottime);
    (now.tv_sec as u64).saturating_mul(1000) + now.tv_nsec as u64 / 1_000_000
}

fn timer(deadline_ms: u64) -> Result<OwnedFd> {
    let fd = time::timerfd_create(time::TimerfdClockId::Boottime, time::TimerfdFlags::CLOEXEC)?;
    reset_timer(&fd, deadline_ms)?;
    Ok(fd)
}

fn reset_timer(fd: &impl AsFd, deadline_ms: u64) -> Result<time::Itimerspec> {
    Ok(time::timerfd_settime(
        fd,
        time::TimerfdTimerFlags::ABSTIME,
        &time::Itimerspec {
            it_interval: time::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: time::Timespec {
                tv_sec: (deadline_ms / 1000)
                    .try_into()
                    .map_err(|_| Error::InvalidRequest)?,
                tv_nsec: ((deadline_ms % 1000) * 1_000_000) as _,
            },
        },
    )?)
}

fn wait(timer: &OwnedFd) -> Result<()> {
    let mut ticks = [0; 8];
    loop {
        match rustix::io::read(timer, &mut ticks) {
            Ok(8) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            _ => return Err(Error::Setup),
        }
    }
}

fn observe_empty(group: &cgroup::Cgroup) -> Result<()> {
    let deadline = boottime_ms().saturating_add(5000);
    loop {
        if !group.populated()? {
            return Ok(());
        }
        if boottime_ms() >= deadline {
            return Err(Error::DrainTimeout);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn prepare_output(output: &impl AsFd) -> Result<()> {
    use rustix::fs::{FileType, OFlags, fcntl_getfl, fcntl_setfl, fstat};
    if FileType::from_raw_mode(fstat(output)?.st_mode) != FileType::Fifo {
        return Err(Error::PipeRequired);
    }
    fcntl_setfl(output, fcntl_getfl(output)? | OFlags::NONBLOCK)?;
    Ok(())
}

/// One atomic nonblocking pipe frame, smaller than Linux PIPE_BUF (4096 bytes).
pub fn write_frame(output: &impl AsFd, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| Error::OutputUnavailable)?;
    bytes.push(b'\n');
    if bytes.len() > 4096 {
        return Err(Error::OutputUnavailable);
    }
    match rustix::io::write(output, &bytes) {
        Ok(n) if n == bytes.len() => Ok(()),
        _ => Err(Error::OutputUnavailable),
    }
}
