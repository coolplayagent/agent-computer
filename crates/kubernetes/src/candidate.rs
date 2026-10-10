//! A prepared data leaf, never the full JuiceFS volume or preparation metadata.
use crate::{
    Client, EphemeralSandboxPlan, Error, InstanceIdentity, Result, plan::opaque, volume::VolumePlan,
};
use agent_computer_storage::{PrepareRequest, Prepared};
use serde_json::{Value, json};

/// Trusted storage evidence, not a writer grant. Callers must independently
/// verify the real mounted filesystem/receipt and obtain current authorization.
/// Parent directories must remain operator-owned and immutable for this generation.
#[derive(Clone, Debug)]
pub struct CandidateMount {
    volume: VolumePlan,
    namespace_uid: String,
    pv_uid: String,
    volume_path: String,
    organization: String,
    computer: String,
    workspace: String,
    candidate: String,
    generation: u64,
    prepared: Prepared,
    fence: Option<(agent_computer_fence::MountReference, crate::NodeIdentity)>,
}
impl CandidateMount {
    pub fn new(
        volume: VolumePlan,
        namespace_uid: &str,
        pv_uid: &str,
        volume_path: &str,
        request: &PrepareRequest,
        prepared: &Prepared,
    ) -> Result<Self> {
        let digest = request
            .binding_digest(volume_path, 1000, 1000)
            .map_err(|_| Error::InvalidIdentity)?;
        let volume_binding: Value = serde_json::from_str(
            volume.pvc["metadata"]["annotations"]["agent-computer.io/volume-binding"]
                .as_str()
                .ok_or(Error::InvalidIdentity)?,
        )
        .map_err(|_| Error::InvalidIdentity)?;
        if ![
            namespace_uid,
            pv_uid,
            volume_path,
            &prepared.filesystem_uuid,
        ]
        .into_iter()
        .all(opaque)
            || prepared.version != 1
            || prepared.request_digest != digest
            || prepared.volume_uid != request.volume_uid
            || prepared.path_ref != request.path_ref()
            || prepared.manifest_digest != request.manifest_digest
            || prepared.quota_bytes != request.quota_bytes
            || prepared.data_inode == 0
            || request.quota_bytes > volume.quota
            || volume_binding["identity"]["organization"] != request.organization
        {
            return Err(Error::IdentityMismatch);
        }
        Ok(Self {
            volume,
            namespace_uid: namespace_uid.into(),
            pv_uid: pv_uid.into(),
            volume_path: volume_path.into(),
            organization: request.organization.clone(),
            computer: request.computer.clone(),
            workspace: request.workspace.clone(),
            candidate: request.candidate.clone(),
            generation: request.generation,
            prepared: prepared.clone(),
            fence: None,
        })
    }
    /// Plan metadata only. Runtime admission must independently hold the live
    /// fence and verify the actual CSI-published device/inode on this node.
    pub fn with_fence(
        mut self,
        reference: agent_computer_fence::MountReference,
        node: crate::NodeIdentity,
    ) -> Result<Self> {
        reference.validate().map_err(|_| Error::InvalidIdentity)?;
        node.validate()?;
        if reference.prepared != self.prepared || reference.boot_id != node.boot_id {
            return Err(Error::IdentityMismatch);
        }
        self.fence = Some((reference, node));
        Ok(self)
    }
    pub(crate) fn check_sandbox(
        &self,
        sandbox: &agent_computer_definitions::model::Sandbox,
        identity: &InstanceIdentity,
        namespace: &str,
    ) -> Result<()> {
        if identity.organization != self.organization
            || identity.computer != self.computer
            || identity.generation != self.generation
            || namespace != self.volume.namespace
        {
            return Err(Error::IdentityMismatch);
        }
        if sandbox.mounts.len() != 1
            || sandbox.mounts[0].workspace_ref != format!("id:{}", self.workspace)
            || sandbox.mounts[0].path != "/workspace"
            || sandbox.mounts[0].read_only
        {
            return Err(Error::UnsupportedSandbox);
        }
        Ok(())
    }
    pub(crate) fn binding(&self) -> Value {
        // Do not embed the input manifest or its objects in Pod annotations.
        let mut binding = json!({"kind":"prepared_candidate","version":1,"organization":self.organization,"computer":self.computer,
            "workspace":self.workspace,"candidate":self.candidate,"generation":self.generation,
            "namespace_uid":self.namespace_uid,"pvc_name":self.volume.name,"pv_uid":self.pv_uid,
            "volume_path":self.volume_path,"prepared":self.prepared,
            "volume_binding":self.volume.pvc["metadata"]["annotations"]["agent-computer.io/volume-binding"]});
        if let Some((reference, node)) = &self.fence {
            binding["fence"] = json!({"mount":reference,"node":node});
        }
        binding
    }
    pub(crate) fn mount(&self, pod: &mut Value) -> Result<()> {
        // fsGroup can recursively change the whole CSI volume, including private
        // parent directories and receipts. The prepared leaf already belongs to 1000.
        pod["spec"]["securityContext"]
            .as_object_mut()
            .ok_or(Error::InvalidIdentity)?
            .remove("fsGroup");
        let volumes = pod["spec"]["volumes"]
            .as_array_mut()
            .ok_or(Error::InvalidIdentity)?;
        let volume = volumes
            .iter_mut()
            .find(|v| v["name"] == "workspace")
            .ok_or(Error::InvalidIdentity)?;
        *volume = json!({"name":"workspace","persistentVolumeClaim":{"claimName":self.volume.name,"readOnly":false}});
        let mounts = pod["spec"]["containers"][0]["volumeMounts"]
            .as_array_mut()
            .ok_or(Error::InvalidIdentity)?;
        let mount = mounts
            .iter_mut()
            .find(|v| v["name"] == "workspace")
            .ok_or(Error::InvalidIdentity)?;
        *mount = json!({"name":"workspace","mountPath":"/workspace","subPath":self.prepared.path_ref,"readOnly":false});
        if let Some((reference, node)) = &self.fence {
            *mount = json!({"name":"workspace","mountPath":"/workspace","readOnly":false});
            let volumes = pod["spec"]["volumes"]
                .as_array_mut()
                .ok_or(Error::InvalidIdentity)?;
            let volume = volumes
                .iter_mut()
                .find(|v| v["name"] == "workspace")
                .ok_or(Error::InvalidIdentity)?;
            *volume = json!({"name":"workspace","csi":{"driver":"csi.agent-computer.io","readOnly":false,"volumeAttributes":{"agent-computer.io/mount-instance":reference.instance}}});
            pod["spec"]["nodeName"] = json!(node.name);
        }
        Ok(())
    }
}
impl Client {
    /// API identity revalidation only. PVC/PV names are not atomic UID mount
    /// preconditions; exclusive operator RBAC and trusted CSI/node configuration
    /// remain required. This does not prove the mounted inode, quota or durability.
    pub async fn probe_sandbox_storage(&self, plan: &EphemeralSandboxPlan) -> Result<()> {
        self.check_plan(plan)?;
        let Some(mount) = &plan.candidate else {
            return self.probe().await;
        };
        if self.namespace_uid() != mount.namespace_uid {
            return Err(Error::IdentityMismatch);
        }
        let claim = self
            .observe_volume_claim(&mount.volume, Some(&mount.prepared.volume_uid))
            .await?
            .ok_or(Error::PreconditionFailed)?;
        let volume = self
            .observe_bound_volume(&mount.volume, &claim, Some(&mount.pv_uid))
            .await?
            .ok_or(Error::PreconditionFailed)?;
        // Qualified dynamic JuiceFS provisioning uses the same immutable PV
        // name, CSI handle and filesystem subdirectory. Reject alternative mappings.
        if volume.name() != mount.volume_path || volume.handle() != mount.volume_path {
            return Err(Error::IdentityMismatch);
        }
        Ok(())
    }
}
