use crate::{Error, Result};
use agent_computer_kubernetes::PodRuntimeIdentity;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct RuntimeObservation {
    pub identity: PodRuntimeIdentity,
    pub sandbox_id: String,
    pub sentry_pid: u32,
    pub sentry_start_ticks: u64,
    pub cgroup_path: String,
    pub cgroup_inode: u64,
    pub workspace_inode: u64,
    pub volume_path: String,
    pub runtime_processes: Vec<ProcessIdentity>,
}

#[derive(Debug, Serialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_ticks: u64,
    pub role: String,
}

pub(crate) struct RuntimeFields {
    pub sandbox: String,
    pub pid: u32,
    pub parent: String,
    pub workspace: String,
}

pub(crate) fn hex_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn string<'a>(value: &'a Value, pointer: &str) -> Result<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or(Error::InvalidObservation)
}

pub(crate) fn sandbox_id(container: &Value) -> Result<&str> {
    let value = string(container, "/info/sandboxID")?;
    if !hex_id(value) {
        return Err(Error::IdentityMismatch);
    }
    Ok(value)
}

pub(crate) fn fields(
    id: &PodRuntimeIdentity,
    command: &Value,
    volume_path: &str,
    container: &Value,
    sandbox: &Value,
    metadata: &Value,
) -> Result<RuntimeFields> {
    parse(
        &Identity {
            namespace: id.namespace(),
            pod: id.pod_name(),
            uid: id.pod_uid(),
            container: id.container_id(),
        },
        command,
        volume_path,
        container,
        sandbox,
        metadata,
    )
}

struct Identity<'a> {
    namespace: &'a str,
    pod: &'a str,
    uid: &'a str,
    container: &'a str,
}
impl Identity<'_> {
    fn namespace(&self) -> &str {
        self.namespace
    }
    fn pod_name(&self) -> &str {
        self.pod
    }
    fn pod_uid(&self) -> &str {
        self.uid
    }
    fn container_id(&self) -> &str {
        self.container
    }
}
fn parse(
    id: &Identity<'_>,
    command: &Value,
    volume_path: &str,
    container: &Value,
    sandbox: &Value,
    metadata: &Value,
) -> Result<RuntimeFields> {
    let sid = sandbox_id(container)?;
    let labels = container
        .pointer("/status/labels")
        .ok_or(Error::InvalidObservation)?;
    let slabels = sandbox
        .pointer("/status/labels")
        .ok_or(Error::InvalidObservation)?;
    for (key, expected) in [
        ("io.kubernetes.pod.uid", id.pod_uid()),
        ("io.kubernetes.pod.name", id.pod_name()),
        ("io.kubernetes.pod.namespace", id.namespace()),
    ] {
        if labels[key] != expected || slabels[key] != expected {
            return Err(Error::IdentityMismatch);
        }
    }
    if string(container, "/status/id")? != id.container_id()
        || string(container, "/status/state")? != "CONTAINER_RUNNING"
        || string(container, "/status/metadata/name")? != "sandbox"
        || container
            .pointer("/status/metadata/attempt")
            .and_then(Value::as_u64)
            != Some(0)
        || string(container, "/info/runtimeType")? != "io.containerd.runsc.v1"
        || string(sandbox, "/status/id")? != sid
        || string(sandbox, "/status/state")? != "SANDBOX_READY"
        || string(sandbox, "/status/runtimeHandler")? != "runsc"
        || string(sandbox, "/status/metadata/uid")? != id.pod_uid()
        || string(sandbox, "/info/runtimeType")? != "io.containerd.runsc.v1"
        || string(metadata, "/ID")? != id.container_id()
        || string(metadata, "/Runtime/Name")? != "io.containerd.runsc.v1"
        || container.pointer("/info/removing").and_then(Value::as_bool) != Some(false)
    {
        return Err(Error::IdentityMismatch);
    }
    let pid = container
        .pointer("/info/pid")
        .and_then(Value::as_u64)
        .ok_or(Error::InvalidObservation)?;
    // Qualified runsc containers share the same host sentry with their sandbox.
    if pid <= 1
        || pid > i32::MAX as u64
        || sandbox.pointer("/info/pid").and_then(Value::as_u64) != Some(pid)
    {
        return Err(Error::IdentityMismatch);
    }
    let spec = container
        .pointer("/info/runtimeSpec")
        .ok_or(Error::InvalidObservation)?;
    if spec.pointer("/process/args") != Some(command)
        || spec.pointer("/root/readonly").and_then(Value::as_bool) != Some(true)
        || spec.pointer("/process/user/uid").and_then(Value::as_u64) != Some(1000)
        || spec.pointer("/process/user/gid").and_then(Value::as_u64) != Some(1000)
        || spec
            .pointer("/process/noNewPrivileges")
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err(Error::IdentityMismatch);
    }
    for kind in [
        "bounding",
        "effective",
        "inheritable",
        "permitted",
        "ambient",
    ] {
        // containerd may omit an empty capability set, never a nonempty set.
        if spec["process"]["capabilities"]
            .get(kind)
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
        {
            return Err(Error::IdentityMismatch);
        }
    }
    let parent = string(sandbox, "/info/config/linux/cgroup_parent")?;
    let uid = id.pod_uid().replace('-', "_");
    let allowed = [
        format!("/kubepods.slice/kubepods-pod{uid}.slice"),
        format!("/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod{uid}.slice"),
        format!("/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod{uid}.slice"),
    ];
    if !allowed.iter().any(|p| p == parent) {
        return Err(Error::IdentityMismatch);
    }
    let leaf = parent.rsplit('/').next().ok_or(Error::InvalidObservation)?;
    for (object, cid) in [(container, id.container_id()), (sandbox, sid)] {
        if string(object, "/info/runtimeSpec/linux/cgroupsPath")?
            != format!("{leaf}:cri-containerd:{cid}")
        {
            return Err(Error::IdentityMismatch);
        }
    }
    let mounts = spec["mounts"].as_array().ok_or(Error::InvalidObservation)?;
    let workspace: Vec<_> = mounts
        .iter()
        .filter(|m| m["destination"] == "/workspace")
        .collect();
    if workspace.len() != 1 || workspace[0]["type"] != "bind" {
        return Err(Error::IdentityMismatch);
    }
    let source = workspace[0]["source"]
        .as_str()
        .ok_or(Error::InvalidObservation)?;
    if source
        != format!(
            "/var/lib/kubelet/pods/{}/volume-subpaths/{volume_path}/sandbox/2",
            id.pod_uid()
        )
    {
        return Err(Error::IdentityMismatch);
    }
    let options = workspace[0]["options"]
        .as_array()
        .ok_or(Error::InvalidObservation)?;
    if options.iter().any(|v| v == "ro") || !options.iter().any(|v| v == "rw") {
        return Err(Error::IdentityMismatch);
    }
    Ok(RuntimeFields {
        sandbox: sid.into(),
        pid: pid as u32,
        parent: parent[1..].into(),
        workspace: source.into(),
    })
}

pub(crate) fn process(pid: u32, sandbox: &str, parent: &str) -> Result<u64> {
    let base = std::path::PathBuf::from(format!("/proc/{pid}"));
    let stat = std::fs::read_to_string(base.join("stat")).map_err(|_| Error::RuntimeUnavailable)?;
    let argv = std::fs::read(base.join("cmdline")).map_err(|_| Error::RuntimeUnavailable)?;
    let args: Vec<_> = argv.split(|b| *b == 0).collect();
    let group =
        std::fs::read_to_string(base.join("cgroup")).map_err(|_| Error::RuntimeUnavailable)?;
    if !args.contains(&b"boot".as_slice())
        || !args.contains(&sandbox.as_bytes())
        || group != format!("0::/{parent}/cri-containerd-{sandbox}.scope\n")
    {
        return Err(Error::IdentityMismatch);
    }
    start_ticks(&stat)
}

pub(crate) fn runtime_processes(
    fields: &RuntimeFields,
    container: &str,
    deadline: std::time::Instant,
) -> Result<Vec<ProcessIdentity>> {
    let mut found = Vec::new();
    let mut sandbox_gofer = false;
    let mut container_gofer = false;
    let mut count = 0;
    for entry in std::fs::read_dir("/proc").map_err(|_| Error::RuntimeUnavailable)? {
        if std::time::Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        let entry = entry.map_err(|_| Error::RuntimeUnavailable)?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        count += 1;
        if count > 65536 {
            return Err(Error::ResponseLimit);
        }
        use std::io::Read;
        let bytes = match std::fs::File::open(entry.path().join("cmdline")).and_then(|file| {
            let mut bytes = Vec::new();
            file.take(65537).read_to_end(&mut bytes)?;
            Ok(bytes)
        }) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(Error::RuntimeUnavailable),
        };
        if bytes.len() > 65536 {
            return Err(Error::ResponseLimit);
        }
        let args: Vec<_> = bytes.split(|b| *b == 0).collect();
        let sid = args.contains(&fields.sandbox.as_bytes());
        let cid = args.contains(&container.as_bytes());
        if !sid && !cid {
            continue;
        }
        let role = if args.contains(&b"gofer".as_slice()) {
            "gofer"
        } else if args.contains(&b"boot".as_slice()) {
            "sentry"
        } else {
            continue;
        };
        let group = std::fs::read_to_string(entry.path().join("cgroup"))
            .map_err(|_| Error::RuntimeUnavailable)?;
        if group
            != format!(
                "0::/{}/cri-containerd-{}.scope\n",
                fields.parent, fields.sandbox
            )
            || (role == "sentry" && pid != fields.pid)
        {
            return Err(Error::IdentityMismatch);
        }
        let start_ticks = start_ticks(
            &std::fs::read_to_string(entry.path().join("stat"))
                .map_err(|_| Error::RuntimeUnavailable)?,
        )?;
        if role == "gofer" {
            sandbox_gofer |= sid;
            container_gofer |= cid;
        }
        found.push(ProcessIdentity {
            pid,
            start_ticks,
            role: role.into(),
        });
    }
    if !sandbox_gofer
        || !container_gofer
        || found.iter().filter(|p| p.role == "sentry").count() != 1
    {
        return Err(Error::IdentityMismatch);
    }
    found.sort_by_key(|p| p.pid);
    Ok(found)
}

fn start_ticks(stat: &str) -> Result<u64> {
    let tail = stat.rsplit_once(") ").ok_or(Error::InvalidObservation)?.1;
    let fields: Vec<_> = tail.split_whitespace().collect();
    if fields.first().is_none_or(|s| matches!(*s, "Z" | "X" | "x")) {
        return Err(Error::RuntimeUnavailable);
    }
    fields
        .get(19)
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .ok_or(Error::InvalidObservation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn runtime_mapping_rejects_foreign_ids_processes_mounts_and_privilege() {
        let container_id = "a".repeat(64);
        let sid = "b".repeat(64);
        let uid = "12345678-1234-1234-1234-123456789abc";
        let id = Identity {
            namespace: "runtime",
            pod: "pod",
            uid,
            container: &container_id,
        };
        let command = json!(["/bin/agent-computer-sandbox", "--attach-startup-json", "{}"]);
        let labels = json!({"io.kubernetes.pod.name":"pod","io.kubernetes.pod.uid":uid,"io.kubernetes.pod.namespace":"runtime"});
        let leaf = format!("kubepods-pod{}.slice", uid.replace('-', "_"));
        let container = json!({"status":{"id":container_id,"state":"CONTAINER_RUNNING","metadata":{"name":"sandbox","attempt":0},"labels":labels},"info":{"sandboxID":sid,"runtimeType":"io.containerd.runsc.v1","removing":false,"pid":42,"runtimeSpec":{"root":{"readonly":true},"process":{"args":command,"user":{"uid":1000,"gid":1000},"noNewPrivileges":true,"capabilities":{}},"linux":{"cgroupsPath":format!("{leaf}:cri-containerd:{container_id}")},"mounts":[{"destination":"/workspace","type":"bind","source":format!("/var/lib/kubelet/pods/{uid}/volume-subpaths/pvc-id/sandbox/2"),"options":["rw","rbind"]}]}}});
        let sandbox = json!({"status":{"id":sid,"state":"SANDBOX_READY","metadata":{"uid":uid},"runtimeHandler":"runsc","labels":labels},"info":{"runtimeType":"io.containerd.runsc.v1","pid":42,"config":{"linux":{"cgroup_parent":format!("/kubepods.slice/{leaf}")}},"runtimeSpec":{"linux":{"cgroupsPath":format!("{leaf}:cri-containerd:{sid}")}}}});
        let metadata = json!({"ID":container_id,"Runtime":{"Name":"io.containerd.runsc.v1"}});
        assert_eq!(
            parse(&id, &command, "pvc-id", &container, &sandbox, &metadata)
                .unwrap()
                .pid,
            42
        );
        for (pointer, value) in [
            ("/status/id", json!("c".repeat(64))),
            ("/status/labels/io.kubernetes.pod.uid", json!("replacement")),
            ("/status/metadata/attempt", json!(1)),
            ("/status/state", json!("CONTAINER_EXITED")),
            ("/info/sandboxID", json!("c".repeat(64))),
            ("/info/runtimeType", json!("io.containerd.runc.v2")),
            ("/info/pid", json!(43)),
            ("/info/removing", json!(true)),
            ("/info/runtimeSpec/process/args", json!(["/bin/true"])),
            ("/info/runtimeSpec/process/user/uid", json!(0)),
            ("/info/runtimeSpec/process/noNewPrivileges", json!(false)),
            (
                "/info/runtimeSpec/process/capabilities",
                json!({"effective":["CAP_SYS_ADMIN"]}),
            ),
            ("/info/runtimeSpec/root/readonly", json!(false)),
            (
                "/info/runtimeSpec/linux/cgroupsPath",
                json!("/kubepods.slice"),
            ),
            ("/info/runtimeSpec/mounts/0/source", json!("/foreign")),
            ("/info/runtimeSpec/mounts/0/options", json!(["ro"])),
        ] {
            let mut changed = container.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            assert!(
                parse(&id, &command, "pvc-id", &changed, &sandbox, &metadata).is_err(),
                "{pointer}"
            );
        }
        let mut changed = sandbox.clone();
        changed["info"]["config"]["linux"]["cgroup_parent"] = json!("/kubepods.slice");
        assert!(parse(&id, &command, "pvc-id", &container, &changed, &metadata).is_err());
        let mut changed = metadata.clone();
        changed["Runtime"]["Name"] = json!("io.containerd.runc.v2");
        assert!(parse(&id, &command, "pvc-id", &container, &sandbox, &changed).is_err());
    }
    #[test]
    fn process_identity_uses_start_ticks_and_rejects_dead_or_partial_records() {
        let tail = (1..=19)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(start_ticks(&format!("42 (a ) tricky) S {tail}")), Ok(19));
        assert!(start_ticks(&format!("42 (test) Z {tail}")).is_err());
        assert!(start_ticks("42 (test) S 1 2").is_err());
    }
}
