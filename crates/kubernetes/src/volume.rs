//! Retained, dynamically provisioned JuiceFS CSI volumes. No data deletion or
//! mounting is exposed here; Bound is a provisioning fact, not a durability proof.
use crate::{
    Client, Error, Result,
    plan::{dns_label, opaque},
};
use agent_computer_definitions::ValidatedDefinition;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const LABEL: &str = "agent-computer.io/volume";
const BINDING: &str = "agent-computer.io/volume-binding";

/// Operator-owned mapping from an authorized catalog reference to a qualified
/// CSI deployment. Secrets remain with CSI; this client never reads them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageClassBinding {
    pub reference: String,
    pub name: String,
    pub uid: String,
    pub driver_uid: String,
    pub secret_name: String,
    pub secret_namespace: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct VolumeIdentity {
    pub organization: String,
    pub resource_id: String,
    pub revision: i64,
    pub step_id: String,
    pub spec_digest: String,
}

#[derive(Clone, Debug)]
pub struct VolumePlan {
    pub(crate) pvc: Value,
    pub(crate) name: String,
    pub(crate) namespace: String,
    pub(crate) storage: StorageClassBinding,
    pub(crate) quota: u64,
}

impl VolumePlan {
    pub fn new(
        definition: &ValidatedDefinition,
        volume_name: &str,
        identity: VolumeIdentity,
        namespace: &str,
        storage: StorageClassBinding,
    ) -> Result<Self> {
        let volume = definition
            .document()
            .spec
            .volumes
            .iter()
            .find(|v| v.name == volume_name)
            .ok_or(Error::UnsupportedVolume)?;
        if identity.revision != 1
            || !volume.quota_bytes.is_multiple_of(1 << 30)
            || volume.storage_class != storage.reference
        {
            return Err(Error::UnsupportedVolume);
        }
        if ![
            &identity.organization,
            &identity.resource_id,
            &identity.step_id,
            &storage.uid,
            &storage.driver_uid,
        ]
        .into_iter()
        .all(|s| opaque(s))
            || ![
                namespace,
                &storage.name,
                &storage.secret_name,
                &storage.secret_namespace,
            ]
            .into_iter()
            .all(dns_label)
            || identity.spec_digest.len() != 71
            || !identity.spec_digest.starts_with("sha256:")
            || !identity.spec_digest[7..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::InvalidIdentity);
        }
        let key = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(&identity.organization, &identity.resource_id))
                    .map_err(|_| Error::InvalidIdentity)?
            )
        );
        let name = format!("acv-{}", &key[..52]);
        let binding = serde_json::to_string(&json!({
            "version": 1, "identity": identity, "volume": volume,
            "storage": storage, "namespace": namespace,
        }))
        .map_err(|_| Error::InvalidIdentity)?;
        let pvc = json!({"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":name,"namespace":namespace,
            "labels":{LABEL:&key[..52]},"annotations":{BINDING:binding}},
            "spec":{"storageClassName":storage.name,"accessModes":["ReadWriteMany"],"volumeMode":"Filesystem",
                "resources":{"requests":{"storage":format!("{}Gi",volume.quota_bytes>>30)}}}});
        Ok(Self {
            pvc,
            name,
            namespace: namespace.into(),
            storage,
            quota: volume.quota_bytes,
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn manifest(&self) -> Value {
        self.pvc.clone()
    }
}

/// A PVC identity is observable before binding; persist its UID at that point.
#[derive(Clone, Debug)]
pub struct ClaimObservation {
    pub(crate) uid: String,
    pub(crate) name: String,
    pub(crate) namespace_uid: String,
    pub(crate) volume_name: Option<String>,
    binding: Value,
}
impl ClaimObservation {
    pub fn uid(&self) -> &str {
        &self.uid
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn namespace_uid(&self) -> &str {
        &self.namespace_uid
    }
    pub fn volume_name(&self) -> Option<&str> {
        self.volume_name.as_deref()
    }
}

#[derive(Clone, Debug)]
pub struct VolumeObservation {
    pub(crate) name: String,
    pub(crate) uid: String,
    pub(crate) handle: String,
}
impl VolumeObservation {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn uid(&self) -> &str {
        &self.uid
    }
    pub fn handle(&self) -> &str {
        &self.handle
    }
}

impl Client {
    pub async fn probe_volume(&self, plan: &VolumePlan) -> Result<()> {
        if plan.namespace != self.deployment.namespace {
            return Err(Error::PreconditionFailed);
        }
        self.probe().await?;
        let sc = self
            .get(&format!(
                "/apis/storage.k8s.io/v1/storageclasses/{}",
                plan.storage.name
            ))
            .await?
            .ok_or(Error::PreconditionFailed)?;
        let driver = self
            .get("/apis/storage.k8s.io/v1/csidrivers/csi.juicefs.com")
            .await?
            .ok_or(Error::PreconditionFailed)?;
        verify_storage(&plan.storage, &sc, &driver)
    }

    /// One POST after preflight; uncertainty never triggers an automatic retry.
    pub async fn create_volume(&self, plan: &VolumePlan) -> Result<ClaimObservation> {
        self.probe_volume(plan).await?;
        let path = format!(
            "/api/v1/namespaces/{}/persistentvolumeclaims",
            plan.namespace
        );
        match self.request(Method::POST, &path, Some(&plan.pvc)).await {
            Ok((409, _)) => Err(Error::ExistingObject),
            Ok((201, pvc)) => verify_claim(plan, &self.deployment.namespace_uid, &pvc, None)
                .map_err(|_| Error::MutationUnconfirmed),
            _ => Err(Error::MutationUnconfirmed),
        }
    }

    pub async fn observe_volume_claim(
        &self,
        plan: &VolumePlan,
        uid: Option<&str>,
    ) -> Result<Option<ClaimObservation>> {
        self.probe_volume(plan).await?;
        self.get(&format!(
            "/api/v1/namespaces/{}/persistentvolumeclaims/{}",
            plan.namespace, plan.name
        ))
        .await?
        .map(|pvc| verify_claim(plan, &self.deployment.namespace_uid, &pvc, uid))
        .transpose()
    }

    /// Cross-check the bidirectional PVC/PV relationship and actual CSI source.
    /// No mount capability is issued: a Candidate still needs separate admission.
    pub async fn observe_bound_volume(
        &self,
        plan: &VolumePlan,
        claim: &ClaimObservation,
        uid: Option<&str>,
    ) -> Result<Option<VolumeObservation>> {
        if claim.name != plan.name
            || claim.namespace_uid != self.deployment.namespace_uid
            || claim.binding != plan.pvc["metadata"]["annotations"][BINDING]
        {
            return Err(Error::IdentityMismatch);
        }
        let Some(name) = &claim.volume_name else {
            return Ok(None);
        };
        self.get(&format!("/api/v1/persistentvolumes/{name}"))
            .await?
            .map(|pv| verify_volume(plan, claim, &pv, uid))
            .transpose()
    }
}

fn parameters(s: &StorageClassBinding) -> Value {
    json!({"csi.storage.k8s.io/fstype":"juicefs",
        "csi.storage.k8s.io/provisioner-secret-name":s.secret_name,
        "csi.storage.k8s.io/provisioner-secret-namespace":s.secret_namespace,
        "csi.storage.k8s.io/node-publish-secret-name":s.secret_name,
        "csi.storage.k8s.io/node-publish-secret-namespace":s.secret_namespace})
}

pub(crate) fn verify_storage(s: &StorageClassBinding, sc: &Value, driver: &Value) -> Result<()> {
    if sc["metadata"]["name"] != s.name
        || sc["metadata"]["uid"] != s.uid
        || !sc["metadata"]["deletionTimestamp"].is_null()
        || sc["provisioner"] != "csi.juicefs.com"
        || sc["reclaimPolicy"] != "Retain"
        || sc["volumeBindingMode"] != "Immediate"
        || sc["mountOptions"] != json!(["writeback=false"])
        || sc["parameters"] != parameters(s)
        || driver["metadata"]["name"] != "csi.juicefs.com"
        || driver["metadata"]["uid"] != s.driver_uid
        || !driver["metadata"]["deletionTimestamp"].is_null()
        || driver["spec"]["attachRequired"] != false
        || !driver["spec"]["volumeLifecycleModes"]
            .as_array()
            .is_some_and(|m| m.contains(&json!("Persistent")))
    {
        return Err(Error::PreconditionFailed);
    }
    Ok(())
}

pub(crate) fn verify_claim(
    plan: &VolumePlan,
    namespace_uid: &str,
    pvc: &Value,
    expected_uid: Option<&str>,
) -> Result<ClaimObservation> {
    let uid = pvc["metadata"]["uid"]
        .as_str()
        .filter(|s| opaque(s))
        .ok_or(Error::InvalidResponse)?;
    let spec = &pvc["spec"];
    if pvc["apiVersion"] != "v1"
        || pvc["kind"] != "PersistentVolumeClaim"
        || pvc["metadata"]["name"] != plan.name
        || pvc["metadata"]["namespace"] != plan.namespace
        || !pvc["metadata"]["deletionTimestamp"].is_null()
        || pvc["metadata"]["annotations"][BINDING] != plan.pvc["metadata"]["annotations"][BINDING]
        || pvc["metadata"]["labels"][LABEL] != plan.pvc["metadata"]["labels"][LABEL]
        || expected_uid.is_some_and(|u| u != uid)
        || spec["storageClassName"] != plan.storage.name
        || spec["accessModes"] != json!(["ReadWriteMany"])
        || spec["volumeMode"] != "Filesystem"
        || quantity(&spec["resources"]["requests"]["storage"]) != Some(plan.quota)
        || !spec.as_object().is_some_and(|m| {
            m.keys().all(|k| {
                [
                    "storageClassName",
                    "accessModes",
                    "volumeMode",
                    "resources",
                    "volumeName",
                ]
                .contains(&k.as_str())
            })
        })
        || pvc["status"]["phase"] == "Lost"
    {
        return Err(Error::IdentityMismatch);
    }
    let volume_name = if pvc["status"]["phase"] == "Bound" {
        if quantity(&pvc["status"]["capacity"]["storage"]).is_none_or(|q| q < plan.quota) {
            return Err(Error::IdentityMismatch);
        }
        Some(
            spec["volumeName"]
                .as_str()
                .filter(|s| dns_label(s))
                .ok_or(Error::InvalidResponse)?
                .into(),
        )
    } else {
        None
    };
    Ok(ClaimObservation {
        uid: uid.into(),
        name: plan.name.clone(),
        namespace_uid: namespace_uid.into(),
        volume_name,
        binding: plan.pvc["metadata"]["annotations"][BINDING].clone(),
    })
}

pub(crate) fn verify_volume(
    plan: &VolumePlan,
    claim: &ClaimObservation,
    pv: &Value,
    expected_uid: Option<&str>,
) -> Result<VolumeObservation> {
    let uid = pv["metadata"]["uid"]
        .as_str()
        .filter(|s| opaque(s))
        .ok_or(Error::InvalidResponse)?;
    let name = pv["metadata"]["name"]
        .as_str()
        .filter(|s| dns_label(s))
        .ok_or(Error::InvalidResponse)?;
    let spec = &pv["spec"];
    let csi = &spec["csi"];
    let binding = &spec["claimRef"];
    let handle = csi["volumeHandle"]
        .as_str()
        .filter(|s| opaque(s))
        .ok_or(Error::InvalidResponse)?;
    if claim.binding != plan.pvc["metadata"]["annotations"][BINDING]
        || pv["apiVersion"] != "v1"
        || pv["kind"] != "PersistentVolume"
        || pv["status"]["phase"] != "Bound"
        || !pv["metadata"]["deletionTimestamp"].is_null()
        || claim.volume_name.as_deref() != Some(name)
        || expected_uid.is_some_and(|u| u != uid)
        || spec["storageClassName"] != plan.storage.name
        || spec["persistentVolumeReclaimPolicy"] != "Retain"
        || spec["volumeMode"] != "Filesystem"
        || spec["accessModes"] != json!(["ReadWriteMany"])
        || spec["mountOptions"] != json!(["writeback=false"])
        || quantity(&spec["capacity"]["storage"]).is_none_or(|q| q < plan.quota)
        || binding["uid"] != claim.uid
        || binding["name"] != plan.name
        || binding["namespace"] != plan.namespace
        || csi["driver"] != "csi.juicefs.com"
        || csi["fsType"] != "juicefs"
        || csi["readOnly"] == true
        || csi["nodePublishSecretRef"]
            != json!({"name":plan.storage.secret_name,"namespace":plan.storage.secret_namespace})
        || csi["volumeAttributes"]["subPath"] != name
        || quantity(&csi["volumeAttributes"]["capacity"]) != Some(plan.quota)
        || !csi["volumeAttributes"].as_object().is_some_and(|attrs| {
            attrs.iter().all(|(key, value)| match key.as_str() {
                "subPath" | "capacity" => true,
                "juicefs/controller-quota-set" => value == "true",
                "storage.kubernetes.io/csiProvisionerIdentity" => value.as_str().is_some_and(|s| {
                    !s.is_empty()
                        && s.len() <= 256
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                }),
                _ => false,
            })
        })
        || !csi.as_object().is_some_and(|m| {
            m.keys().all(|k| {
                [
                    "driver",
                    "fsType",
                    "volumeHandle",
                    "readOnly",
                    "volumeAttributes",
                    "nodePublishSecretRef",
                ]
                .contains(&k.as_str())
            })
        })
        || !spec.as_object().is_some_and(|m| {
            m.keys().all(|k| {
                [
                    "capacity",
                    "accessModes",
                    "persistentVolumeReclaimPolicy",
                    "storageClassName",
                    "volumeMode",
                    "mountOptions",
                    "claimRef",
                    "csi",
                ]
                .contains(&k.as_str())
            })
        })
    {
        return Err(Error::IdentityMismatch);
    }
    Ok(VolumeObservation {
        name: name.into(),
        uid: uid.into(),
        handle: handle.into(),
    })
}

// Byte quantities returned by this adapter's integer-GiB provisioning path.
// Reject unrecognized/fractional quantities instead of rounding budgets upward.
pub(crate) fn quantity(v: &Value) -> Option<u64> {
    let value = v.as_str()?;
    let split = value
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(value.len());
    let n = value[..split].parse::<u64>().ok()?;
    let multiplier = match &value[split..] {
        "" => 1,
        "Ki" => 1 << 10,
        "Mi" => 1 << 20,
        "Gi" => 1 << 30,
        "Ti" => 1 << 40,
        "Pi" => 1 << 50,
        "Ei" => 1 << 60,
        "k" | "K" => 1000,
        "M" => 1_000_000,
        "G" => 1_000_000_000,
        "T" => 1_000_000_000_000,
        _ => return None,
    };
    n.checked_mul(multiplier)
}
