use crate::{Error, Result};
use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    os::unix::{fs::MetadataExt, process::CommandExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Executable {
    pub path: PathBuf,
    pub sha256: String,
}

impl Executable {
    /// Operator-owned ELF only. Pin an open description for the actual exec,
    /// rather than checking one path and executing a possibly replaced file.
    pub(crate) fn open(&self) -> Result<File> {
        let mut file = trusted_file(&self.path, false)?;
        if file.metadata().map_err(|_| Error::Configuration)?.mode() & 0o111 == 0 {
            return Err(Error::Configuration);
        }
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        let mut first = true;
        let mut total = 0;
        loop {
            let n = file.read(&mut buffer).map_err(|_| Error::Configuration)?;
            if n == 0 {
                break;
            }
            if first && (n < 4 || &buffer[..4] != b"\x7fELF") {
                return Err(Error::Configuration);
            }
            first = false;
            total += n;
            if total > 256 * 1024 * 1024 {
                return Err(Error::Configuration);
            }
            hash.update(&buffer[..n]);
        }
        if first || format!("sha256:{:x}", hash.finalize()) != self.sha256 {
            return Err(Error::Configuration);
        }
        Ok(file)
    }
}

/// Every path ancestor is an operator-owned directory; no symlink resolution.
pub(crate) fn trusted_file(path: &Path, directory: bool) -> Result<File> {
    let components: Vec<_> = path.components().collect();
    if !path.is_absolute()
        || components.len() < 2
        || components
            .iter()
            .skip(1)
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(Error::Configuration);
    }
    let mut parent = File::open("/").map_err(|_| Error::Configuration)?;
    for (i, part) in components.iter().enumerate().skip(1) {
        let is_directory = i + 1 < components.len() || directory;
        let flags = OFlags::RDONLY
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | if is_directory {
                OFlags::DIRECTORY
            } else {
                OFlags::empty()
            };
        parent = File::from(
            openat2(
                &parent,
                part.as_os_str(),
                flags,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
            )
            .map_err(|_| Error::Configuration)?,
        );
        let meta = parent.metadata().map_err(|_| Error::Configuration)?;
        if meta.uid() != 0 || meta.mode() & 0o022 != 0 || (!is_directory && !meta.is_file()) {
            return Err(Error::Configuration);
        }
    }
    Ok(parent)
}

pub(crate) fn command(executable: &File, argv0: &str) -> Command {
    use std::os::fd::AsRawFd;
    let mut command = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()));
    command
        .arg0(argv0)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    command
}

pub(crate) fn read_json(
    executable: &File,
    argv0: &str,
    args: &[&str],
    deadline: Instant,
) -> Result<serde_json::Value> {
    let mut child = command(executable, argv0)
        .args(args)
        .process_group(0)
        .spawn()
        .map_err(|_| Error::RuntimeUnavailable)?;
    let pid = rustix::process::Pid::from_child(&child);
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or(Error::RuntimeUnavailable)?;
        rustix::fs::fcntl_setfl(&stdout, OFlags::NONBLOCK)
            .map_err(|_| Error::RuntimeUnavailable)?;
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        let mut eof = false;
        loop {
            if Instant::now() >= deadline {
                return Err(Error::Deadline);
            }
            match stdout.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    if bytes.len() + n > 1_048_576 {
                        return Err(Error::ResponseLimit);
                    };
                    bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(Error::RuntimeUnavailable),
            }
            if let Some(status) = unreaped_status(pid).map_err(|_| Error::RuntimeUnavailable)? {
                if status.exit_status() != Some(0) {
                    return Err(Error::RuntimeUnavailable);
                }
                if eof {
                    return serde_json::from_slice(&bytes).map_err(|_| Error::InvalidObservation);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    // Keep the leader waitable until all group signals are sent, so the kernel
    // cannot reuse its PID/PGID for an unrelated host process. If ownership was
    // lost to another reaper, send no signal using the old numeric identity.
    if result.is_err() && unreaped_status(pid).is_ok() {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

fn unreaped_status(
    pid: rustix::process::Pid,
) -> rustix::io::Result<Option<rustix::process::WaitIdStatus>> {
    use rustix::process::{WaitId, WaitIdOptions, waitid};
    loop {
        match waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Err(rustix::io::Errno::INTR) => continue,
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_observation_keeps_child_waitable_until_group_cleanup() {
        let mut child = Command::new("/usr/bin/false")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = rustix::process::Pid::from_child(&child);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = unreaped_status(pid).unwrap() {
                assert_eq!(status.exit_status(), Some(1));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        // A second observation must still find the same unreaped child. Reaping
        // on the first read would return ECHILD and release its numeric identity.
        assert_eq!(
            unreaped_status(pid).unwrap().unwrap().exit_status(),
            Some(1)
        );
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        assert_eq!(child.wait().unwrap().code(), Some(1));
        assert_eq!(unreaped_status(pid).unwrap_err(), rustix::io::Errno::CHILD);
        for (path, argv0, args, error) in [
            (
                "/usr/bin/printf",
                "printf",
                vec!["%s", "not-json"],
                Error::InvalidObservation,
            ),
            ("/usr/bin/false", "false", vec![], Error::RuntimeUnavailable),
        ] {
            assert_eq!(
                read_json(
                    &File::open(path).unwrap(),
                    argv0,
                    &args,
                    Instant::now() + Duration::from_secs(2)
                ),
                Err(error)
            );
        }
    }

    fn executable(path: &str) -> Executable {
        let path = std::fs::canonicalize(path).unwrap();
        Executable {
            sha256: format!("sha256:{:x}", Sha256::digest(std::fs::read(&path).unwrap())),
            path,
        }
    }

    #[test]
    fn invokes_the_pinned_description_and_rejects_wrong_hash() {
        let mut spec = executable("/usr/bin/printf");
        let file = spec.open().unwrap();
        assert_eq!(
            read_json(
                &file,
                "printf",
                &["%s", "{\"literal\":\"a;$(id)\"}"],
                Instant::now() + Duration::from_secs(2)
            )
            .unwrap()["literal"],
            "a;$(id)"
        );
        spec.sha256 = format!("sha256:{}", "0".repeat(64));
        assert!(matches!(spec.open(), Err(Error::Configuration)));
    }

    #[test]
    fn bounds_host_command_output_and_elapsed_time() {
        let file = executable("/usr/bin/head").open().unwrap();
        assert_eq!(
            read_json(
                &file,
                "head",
                &["-c", "1048577", "/dev/zero"],
                Instant::now() + Duration::from_secs(3)
            ),
            Err(Error::ResponseLimit)
        );
        let file = executable("/usr/bin/sleep").open().unwrap();
        let start = Instant::now();
        assert_eq!(
            read_json(&file, "sleep", &["10"], start + Duration::from_millis(30)),
            Err(Error::Deadline)
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
