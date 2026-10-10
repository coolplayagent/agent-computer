use crate::{
    INSTANCE_KEY,
    local::{self, invalid},
    wire,
};
use agent_computer_fence::{MountReference, MountedFence};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PodBinding {
    pub namespace: String,
    pub name: String,
    pub node: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    reference: MountReference,
    pod: PodBinding,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    volume: String,
    target: PathBuf,
    uid: String,
}
/// Private, durable operator registry. Registrations require a live FUSE handle.
/// The stored JSON does not convey a writer grant or IO drain proof.
pub struct Registry {
    root: File,
    path: PathBuf,
    registrations: File,
    volumes: File,
}
impl Registry {
    pub fn open(path: &Path) -> io::Result<Self> {
        let root = local::directory(path)?;
        if root.metadata()?.mode() & 0o7777 != 0o700 {
            return Err(invalid("registry must be mode 0700"));
        }
        let registrations = local::mkdir(&root, "registrations")?;
        let volumes = local::mkdir(&root, "volumes")?;
        local::mkdir(&root, "mounts")?;
        Ok(Self {
            root,
            path: path.into(),
            registrations,
            volumes,
        })
    }
    pub fn prepare_mountpoint(&self, execution: &str) -> io::Result<PathBuf> {
        if !local::name(execution) {
            return Err(invalid("invalid execution"));
        }
        let mounts = local::child(&self.root, "mounts", true)?;
        // A previous controller's mountpoint must never be reused.
        rustix::fs::mkdirat(
            &mounts,
            execution,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
        )?;
        mounts.sync_all()?;
        Ok(self.path.join("mounts").join(execution))
    }
    pub fn register(&self, fence: &MountedFence, pod: PodBinding) -> io::Result<()> {
        let _lock = local::lock(&self.root)?;
        fence.verify()?;
        let reference = fence.reference();
        if reference.path.parent() != Some(self.path.join("mounts").as_path())
            || ![&pod.name, &pod.namespace, &pod.node]
                .into_iter()
                .all(|s| local::name(s))
        {
            return Err(invalid("invalid registration binding"));
        }
        let directory = local::mkdir(&self.registrations, &reference.instance)?;
        local::write_new(
            &directory,
            "registration.json",
            &Registration {
                reference: reference.clone(),
                pod,
            },
        )
    }
    /// Durable denial of future publication. Does not claim existing mounts drained.
    pub fn revoke(&self, instance: &str) -> io::Result<()> {
        let _lock = local::lock(&self.root)?;
        let dir = self.registration(instance)?;
        revoke(&dir)
    }
    fn registration(&self, instance: &str) -> io::Result<File> {
        if instance.len() != 64
            || !instance
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("invalid instance"));
        }
        local::child(&self.registrations, instance, true)
    }
    pub(crate) fn publish(&self, request: wire::Publish, node: &str) -> io::Result<()> {
        let _lock = local::lock(&self.root)?;
        let instance = request
            .volume_context
            .get(INSTANCE_KEY)
            .ok_or_else(|| invalid("missing mount instance"))?;
        let dir = self.registration(instance)?;
        let registration: Registration = local::read(&dir, "registration.json")?
            .ok_or_else(|| invalid("missing registration"))?;
        let claim = validate_publish(&request, &registration, node)?;
        if local::read::<bool>(&dir, "revoked.json")?.is_some() {
            return Err(invalid("mount publication revoked"));
        }
        let source = registration.reference.verify()?;
        let existing_claim = local::read::<Claim>(&dir, "claim.json")?;
        if existing_claim
            .as_ref()
            .is_some_and(|existing| existing != &claim)
        {
            return Err(invalid("mount already claimed by another Pod"));
        }
        let volume_key = volume_key(&claim.volume)?;
        match local::read::<String>(&self.volumes, &volume_key)? {
            Some(existing) if &existing == instance => (),
            Some(_) => return Err(invalid("volume already belongs to a different instance")),
            None => local::write_new(&self.volumes, &volume_key, instance)?,
        }
        if existing_claim.is_none() {
            local::write_new(&dir, "claim.json", &claim)?;
        }
        if let Some(mount) = mounted(&claim.target)? {
            return match_mount(&mount, &registration.reference);
        }
        let parent = claim
            .target
            .parent()
            .ok_or_else(|| invalid("invalid target"))?;
        let parent = local::directory(parent)?;
        match rustix::fs::mkdirat(
            &parent,
            "mount",
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
        ) {
            Ok(()) => (),
            Err(rustix::io::Errno::EXIST) => (),
            Err(e) => return Err(e.into()),
        }
        let target = local::directory(&claim.target)?;
        if rustix::fs::Dir::read_from(&target)?.any(|e| {
            e.is_err()
                || e.is_ok_and(|e| ![b".".as_slice(), b".."].contains(&e.file_name().to_bytes()))
        }) {
            return Err(invalid("mount target is not empty"));
        }
        // Clone the pinned source tree and attach it to a pinned target directory.
        // Both descriptors survive pathname changes; no recursive submount exposure.
        use rustix::mount::{MoveMountFlags, OpenTreeFlags, move_mount, open_tree};
        let tree = open_tree(
            &source,
            "",
            OpenTreeFlags::OPEN_TREE_CLONE
                | OpenTreeFlags::OPEN_TREE_CLOEXEC
                | OpenTreeFlags::AT_EMPTY_PATH,
        )?;
        use std::os::fd::AsRawFd;
        rustix::mount::mount_change(
            format!("/proc/self/fd/{}", tree.as_raw_fd()),
            rustix::mount::MountPropagationFlags::PRIVATE,
        )?;
        move_mount(
            &tree,
            "",
            &target,
            "",
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
        )?;
        // Attaching below a shared host parent can create a fresh peer group.
        // Clear it after attachment as well, before acknowledging publication.
        rustix::mount::mount_change(
            format!("/proc/self/fd/{}", tree.as_raw_fd()),
            rustix::mount::MountPropagationFlags::PRIVATE,
        )?;
        let observed =
            mounted(&claim.target)?.ok_or_else(|| invalid("published mount not observed"))?;
        match_mount(&observed, &registration.reference)
    }
    pub(crate) fn unpublish(&self, request: wire::Unpublish) -> io::Result<()> {
        let _lock = local::lock(&self.root)?;
        let key = volume_key(&request.volume_id)?;
        let target = PathBuf::from(&request.target_path);
        target_uid(&target)?;
        let Some(instance) = local::read::<String>(&self.volumes, &key)? else {
            return if mounted(&target)?.is_none() {
                Ok(())
            } else {
                Err(invalid("unregistered mount target"))
            };
        };
        let dir = self.registration(&instance)?;
        let registration: Registration = local::read(&dir, "registration.json")?
            .ok_or_else(|| invalid("missing registration"))?;
        // A crash before claim creation cannot have attached a mount.
        let claim = local::read::<Claim>(&dir, "claim.json")?;
        if let Some(claim) = claim {
            if claim.volume != request.volume_id || claim.target != target {
                return Err(invalid("unpublish claim mismatch"));
            }
        } else if mounted(&target)?.is_some() {
            return Err(invalid("unclaimed mount"));
        }
        revoke(&dir)?;
        if let Some(mount) = mounted(&target)? {
            match_mount(&mount, &registration.reference)?;
            // Source FUSE may already be disconnected. Never stat it during cleanup.
            local::directory(target.parent().ok_or_else(|| invalid("invalid target"))?)?;
            rustix::mount::unmount(
                &target,
                rustix::mount::UnmountFlags::DETACH | rustix::mount::UnmountFlags::NOFOLLOW,
            )?;
            if mounted(&target)?.is_some() {
                return Err(invalid("mount remains after unpublish"));
            }
        }
        Ok(())
    }
}
fn revoke(dir: &File) -> io::Result<()> {
    if local::read::<bool>(dir, "revoked.json")?.is_none() {
        local::write_new(dir, "revoked.json", &true)?;
    }
    Ok(())
}
fn volume_key(id: &str) -> io::Result<String> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(invalid("invalid CSI volume id"));
    }
    Ok(format!("{:x}.json", Sha256::digest(id.as_bytes())))
}
fn target_uid(path: &Path) -> io::Result<String> {
    let text = path.to_str().ok_or_else(|| invalid("invalid target"))?;
    let parts: Vec<_> = text.split('/').collect();
    if parts.len() != 10
        || parts[..5] != ["", "var", "lib", "kubelet", "pods"]
        || parts[6..] != ["volumes", "kubernetes.io~csi", "workspace", "mount"]
        || parts[5].len() != 36
        || !parts[5].bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return Err(invalid("unexpected kubelet target"));
    }
    Ok(parts[5].into())
}
fn validate_publish(request: &wire::Publish, reg: &Registration, node: &str) -> io::Result<Claim> {
    let context = &request.volume_context;
    let uid = target_uid(Path::new(&request.target_path))?;
    let expected = [
        (INSTANCE_KEY, reg.reference.instance.as_str()),
        ("csi.storage.k8s.io/pod.name", reg.pod.name.as_str()),
        (
            "csi.storage.k8s.io/pod.namespace",
            reg.pod.namespace.as_str(),
        ),
        ("csi.storage.k8s.io/pod.uid", uid.as_str()),
        ("csi.storage.k8s.io/serviceAccount.name", "default"),
        ("csi.storage.k8s.io/ephemeral", "true"),
    ];
    if context.len() != expected.len()
        || expected
            .iter()
            .any(|(k, v)| context.get(*k).map(String::as_str) != Some(*v))
        || reg.pod.node != node
        || !request.publish_context.is_empty()
        || !request.secrets.is_empty()
        || !request.staging_target_path.is_empty()
        || request.readonly
    {
        return Err(invalid("CSI request binding mismatch"));
    }
    let cap = request
        .volume_capability
        .as_ref()
        .ok_or_else(|| invalid("missing capability"))?;
    if cap.access_mode.as_ref().map(|m| m.mode) != Some(1)
        || !matches!(&cap.access,Some(wire::capability::Access::Mount(m)) if m.fs_type.is_empty() && m.mount_flags.is_empty() && m.volume_mount_group.is_empty())
    {
        return Err(invalid("unsupported volume capability"));
    }
    volume_key(&request.volume_id)?;
    Ok(Claim {
        volume: request.volume_id.clone(),
        target: request.target_path.clone().into(),
        uid,
    })
}
struct Mount {
    device: String,
    root: String,
    fs: String,
    source: String,
    propagating: bool,
}
fn mounted(path: &Path) -> io::Result<Option<Mount>> {
    let path = path.to_str().ok_or_else(|| invalid("invalid mount path"))?;
    let mut found = None;
    for line in std::fs::read_to_string("/proc/self/mountinfo")?.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.get(4) != Some(&path) {
            continue;
        }
        if found.is_some() {
            return Err(invalid("stacked mounts rejected"));
        }
        let separator = fields
            .iter()
            .position(|s| *s == "-")
            .ok_or_else(|| invalid("invalid mountinfo"))?;
        if fields.len() < separator + 4 {
            return Err(invalid("invalid mountinfo"));
        }
        found = Some(Mount {
            device: fields[2].into(),
            root: fields[3].into(),
            fs: fields[separator + 1].into(),
            source: fields[separator + 2].into(),
            propagating: fields[6..separator]
                .iter()
                .any(|field| field.starts_with("shared:") || field.starts_with("master:")),
        });
    }
    Ok(found)
}
fn match_mount(m: &Mount, r: &MountReference) -> io::Result<()> {
    if m.device
        != format!(
            "{}:{}",
            rustix::fs::major(r.device),
            rustix::fs::minor(r.device)
        )
        || m.propagating
        || m.root != "/"
        || m.fs != "fuse"
        || m.source != format!("agent-computer-{}", r.instance)
    {
        return Err(invalid("foreign mount at CSI target"));
    }
    Ok(())
}
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
