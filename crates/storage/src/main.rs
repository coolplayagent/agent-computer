use agent_computer_storage::{
    Error, Manifest, MountedVolume, ObjectCache, PrepareRequest, Result,
    quota::{JuiceFsConfig, JuiceFsQuota, read_private},
};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    mount_root: PathBuf,
    volume_path: String,
    filesystem_uuid: String,
    volume_uid: String,
    object_cache: PathBuf,
    writer_uid: u32,
    writer_gid: u32,
    quota: JuiceFsConfig,
}
fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() == 3 && args[0] == "manifest-digest" && args[1] == "--file" {
        let manifest: Manifest =
            serde_json::from_slice(&read_private(&PathBuf::from(&args[2]), 4_194_304)?)
                .map_err(|_| Error::InvalidRequest)?;
        println!("{}", manifest.digest()?);
        return Ok(());
    }
    if args.len() != 5
        || args[0] != "prepare"
        || args[1] != "--config-file"
        || args[3] != "--request-file"
    {
        return Err(Error::InvalidRequest);
    }
    let config: Config = serde_json::from_slice(&read_private(&PathBuf::from(&args[2]), 8192)?)
        .map_err(|_| Error::InvalidRequest)?;
    let request: PrepareRequest =
        serde_json::from_slice(&read_private(&PathBuf::from(&args[4]), 4_194_304)?)
            .map_err(|_| Error::InvalidRequest)?;
    let volume = MountedVolume::open(
        &config.mount_root,
        &config.volume_path,
        &config.filesystem_uuid,
        &config.volume_uid,
        config.writer_uid,
        config.writer_gid,
    )?;
    let source = ObjectCache::open(&config.object_cache)?;
    let quota = JuiceFsQuota::new(config.quota)?;
    let prepared = volume.prepare(&request, &source, &quota)?;
    println!(
        "{}",
        serde_json::to_string(&prepared).map_err(|_| Error::Io)?
    );
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
