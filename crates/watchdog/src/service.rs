use agent_computer_watchdog::{Error, Result, reaper::Reaper};
use std::{
    os::{
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixDatagram},
    },
    path::Path,
    time::Duration,
};

fn notify(value: &[u8]) -> Result<()> {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let address = if let Some(name) = bytes.strip_prefix(b"@") {
        SocketAddr::from_abstract_name(name)
    } else {
        SocketAddr::from_pathname(Path::new(&path))
    }
    .map_err(|_| Error::Setup)?;
    let socket = UnixDatagram::unbound().map_err(|_| Error::Setup)?;
    socket
        .set_write_timeout(Some(Duration::from_millis(100)))
        .map_err(|_| Error::Setup)?;
    socket
        .send_to_addr(value, &address)
        .map_err(|_| Error::Setup)?;
    Ok(())
}

pub fn run(spool: &Path, once: bool) -> Result<bool> {
    let mut reaper = Reaper::open(spool)?;
    if !once {
        notify(b"READY=1\nSTATUS=Expiry reaper scanning; not fencing authority")?;
    }
    let mut errors = 0;
    loop {
        let batch = reaper.step()?;
        errors += batch
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e.outcome,
                    agent_computer_watchdog::reaper::Outcome::Unavailable { .. }
                )
            })
            .count();
        if once {
            // Operator output is diagnostic; persistent results live in journals.
            println!(
                "{}",
                serde_json::to_string(&batch).map_err(|_| Error::OutputUnavailable)?
            );
            if batch.pass_complete {
                return Ok(errors == 0);
            }
        } else {
            // No output pipe/journald backpressure on the persistent scan path.
            notify(
                format!(
                    "WATCHDOG=1\nSTATUS=Scanned {} entries; {} unavailable; pass complete {}",
                    batch.scanned, errors, batch.pass_complete
                )
                .as_bytes(),
            )?;
            if batch.pass_complete {
                std::thread::sleep(Duration::from_millis(250));
                errors = 0;
            }
        }
    }
}
