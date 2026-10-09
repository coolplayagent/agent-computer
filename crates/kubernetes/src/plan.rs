use crate::{Error, Result};
use agent_computer_definitions::ValidatedDefinition;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const IDENTITY: &str = "agent-computer.io/instance";
pub(crate) const BINDING: &str = "agent-computer.io/binding";

/// Persist these values in authoritative storage before any Kubernetes mutation.
/// This structure carries identity, not authorization or a dispatch permit.
#[derive(Clone, Debug, Serialize)]
pub struct InstanceIdentity {
    pub organization: String,
    pub computer: String,
    pub sandbox: String,
    pub instance: String,
    pub generation: u64,
    pub spec_revision: u64,
}

#[derive(Clone, Debug)]
pub struct EphemeralSandboxPlan {
    pub(crate) pod: Value,
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) binding: String,
    pub(crate) network_policy_ref: String,
    pub(crate) candidate: Option<Box<crate::CandidateMount>>,
}

impl EphemeralSandboxPlan {
    /// Compile an already validated definition for an explicitly admitted runtime start.
    /// Only a deployment's registered deny-all policy and zero Workspace mounts are supported.
    pub fn new(
        definition: &ValidatedDefinition,
        sandbox_name: &str,
        identity: InstanceIdentity,
        namespace: &str,
        command: Vec<String>,
    ) -> Result<Self> {
        Self::compile(definition, sandbox_name, identity, namespace, command, None)
    }
    pub(crate) fn compile(
        definition: &ValidatedDefinition,
        sandbox_name: &str,
        identity: InstanceIdentity,
        namespace: &str,
        command: Vec<String>,
        candidate: Option<&crate::CandidateMount>,
    ) -> Result<Self> {
        if !dns_label(namespace)
            || ![
                &identity.organization,
                &identity.computer,
                &identity.sandbox,
                &identity.instance,
            ]
            .into_iter()
            .all(|s| opaque(s))
            || identity.generation == 0
            || identity.generation > i64::MAX as u64
            || identity.spec_revision == 0
            || identity.spec_revision > i64::MAX as u64
        {
            return Err(Error::InvalidIdentity);
        }
        if command.is_empty()
            || command.len() > 128
            || command[0].is_empty()
            || command.iter().any(|s| s.contains('\0'))
            || command.iter().map(String::len).sum::<usize>() > 32768
        {
            return Err(Error::InvalidCommand);
        }
        let sandbox = definition
            .document()
            .spec
            .sandboxes
            .iter()
            .find(|s| s.name == sandbox_name)
            .ok_or(Error::UnsupportedSandbox)?;
        if sandbox.runtime_class != "gvisor" {
            return Err(Error::UnsupportedSandbox);
        }
        if let Some(mount) = candidate {
            mount.check_sandbox(sandbox, &identity, namespace)?;
        } else if !sandbox.mounts.is_empty() {
            return Err(Error::UnsupportedSandbox);
        }
        // Names do not change with a command/spec retry: conflicting inputs must collide,
        // rather than silently creating a second process for the same instance.
        let identity_bytes = serde_json::to_vec(&(&identity.organization, &identity.instance))
            .map_err(|_| Error::InvalidIdentity)?;
        let key = format!("{:x}", Sha256::digest(&identity_bytes));
        let name = format!("ac-{}", &key[..52]);
        let binding = serde_json::to_string(&json!({
            "version": 1, "identity": identity, "sandbox": sandbox,
            "namespace": namespace, "command": command,
        }))
        .map_err(|_| Error::InvalidIdentity)?;
        let cpu = cpu_quantity(sandbox.resources.cpu_millis);
        let memory = format!("{}Mi", sandbox.resources.memory_mi_b);
        let pod = json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": namespace,
                "labels": {IDENTITY: &key[..52]}, "annotations": {BINDING: binding}},
            "spec": {
                "runtimeClassName": "gvisor", "restartPolicy": "Never",
                "automountServiceAccountToken": false, "enableServiceLinks": false,
                "hostNetwork": false, "hostPID": false, "hostIPC": false,
                "shareProcessNamespace": false, "terminationGracePeriodSeconds": 30,
                "activeDeadlineSeconds": 3600,
                "securityContext": {"runAsNonRoot": true, "runAsUser": 1000,
                    "runAsGroup": 1000, "fsGroup": 1000,
                    "seccompProfile": {"type": "RuntimeDefault"}},
                "containers": [{"name": "sandbox", "image": sandbox.image,
                    "command": command, "imagePullPolicy": "IfNotPresent",
                    "resources": {"requests": {"cpu": cpu, "memory": memory},
                        "limits": {"cpu": cpu, "memory": memory}},
                    "securityContext": {"allowPrivilegeEscalation": false,
                        "privileged": false, "readOnlyRootFilesystem": true,
                        "capabilities": {"drop": ["ALL"]}},
                    "volumeMounts": [{"name": "tmp", "mountPath": "/tmp"},
                        {"name": "shm", "mountPath": "/dev/shm"}]}],
                "volumes": [{"name": "tmp", "emptyDir": {"medium": "Memory", "sizeLimit": "64Mi"}},
                    {"name": "shm", "emptyDir": {"medium": "Memory", "sizeLimit": "64Mi"}}]
            }
        });
        Ok(Self {
            pod,
            namespace: namespace.into(),
            name,
            binding,
            network_policy_ref: sandbox.network_policy_ref.clone(),
            candidate: candidate.cloned().map(Box::new),
        })
    }

    pub fn pod_name(&self) -> &str {
        &self.name
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    /// For review/evidence only. Mutating this copy cannot change the compiled plan.
    pub fn manifest(&self) -> Value {
        self.pod.clone()
    }
}

fn cpu_quantity(millis: u32) -> String {
    if millis.is_multiple_of(1000) {
        (millis / 1000).to_string()
    } else {
        format!("{millis}m")
    }
}

pub(crate) fn opaque(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

pub(crate) fn dns_label(value: &str) -> bool {
    let end = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !value.is_empty()
        && value.len() <= 63
        && end(value.as_bytes()[0])
        && end(value.as_bytes()[value.len() - 1])
        && value.bytes().all(|b| end(b) || b == b'-')
}
