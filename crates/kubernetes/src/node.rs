//! API identities for a separate trusted node adapter. No host process claims here.
use crate::{Client, EphemeralSandboxPlan, Error, PodPhase, Result, plan, verify};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeIdentity {
    pub name: String,
    pub uid: String,
    pub boot_id: String,
}

impl NodeIdentity {
    fn validate(&self) -> Result<()> {
        if self.name.len() > 253
            || !self.name.split('.').all(plan::dns_label)
            || !plan::opaque(&self.uid)
            || !plan::opaque(&self.boot_id)
        {
            return Err(Error::InvalidIdentity);
        }
        Ok(())
    }
}

/// Only constructed after strict Pod and Node API readback. The host adapter must
/// still establish the CRI/runtime/process/cgroup mapping independently.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PodRuntimeIdentity {
    node: NodeIdentity,
    namespace: String,
    namespace_uid: String,
    pod_name: String,
    pod_uid: String,
    container_id: String,
}

impl PodRuntimeIdentity {
    pub fn node(&self) -> &NodeIdentity {
        &self.node
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn namespace_uid(&self) -> &str {
        &self.namespace_uid
    }
    pub fn pod_name(&self) -> &str {
        &self.pod_name
    }
    pub fn pod_uid(&self) -> &str {
        &self.pod_uid
    }
    pub fn container_id(&self) -> &str {
        &self.container_id
    }
}

impl Client {
    /// Bind a Running, never-restarted sandbox container to the operator's exact
    /// live Node UID/boot. ResourceNames-scoped GET nodes permission is required.
    pub async fn observe_runtime(
        &self,
        plan: &EphemeralSandboxPlan,
        pod_uid: &str,
        node: &NodeIdentity,
    ) -> Result<PodRuntimeIdentity> {
        node.validate()?;
        self.check_plan(plan)?;
        self.probe_sandbox_storage(plan).await?;
        let pod = self
            .get(&self.pod_path(plan, true))
            .await?
            .ok_or(Error::PreconditionFailed)?;
        let object = self
            .get(&format!("/api/v1/nodes/{}", node.name))
            .await?
            .ok_or(Error::PreconditionFailed)?;
        runtime(plan, pod_uid, node, self.namespace_uid(), &pod, &object)
    }
}

fn runtime(
    plan: &EphemeralSandboxPlan,
    pod_uid: &str,
    node: &NodeIdentity,
    namespace_uid: &str,
    pod: &Value,
    object: &Value,
) -> Result<PodRuntimeIdentity> {
    node.validate()?;
    let observed = verify::pod(plan, pod, Some(pod_uid))?;
    if observed.phase() != PodPhase::Running
        || pod.pointer("/spec/nodeName").and_then(Value::as_str) != Some(&node.name)
        || object.pointer("/metadata/name").and_then(Value::as_str) != Some(&node.name)
        || object.pointer("/metadata/uid").and_then(Value::as_str) != Some(&node.uid)
        || object
            .pointer("/metadata/deletionTimestamp")
            .is_some_and(|v| !v.is_null())
        || object
            .pointer("/status/nodeInfo/bootID")
            .and_then(Value::as_str)
            != Some(&node.boot_id)
    {
        return Err(Error::IdentityMismatch);
    }
    let conditions = object
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .ok_or(Error::InvalidResponse)?;
    let ready: Vec<_> = conditions.iter().filter(|c| c["type"] == "Ready").collect();
    if ready.len() != 1 || ready[0]["status"] != "True" {
        return Err(Error::PreconditionFailed);
    }
    let containers = pod
        .pointer("/status/containerStatuses")
        .and_then(Value::as_array)
        .ok_or(Error::InvalidResponse)?;
    if containers.len() != 1
        || containers[0]["name"] != "sandbox"
        || containers[0]["restartCount"].as_u64() != Some(0)
        || containers[0]["state"]
            .as_object()
            .is_none_or(|s| s.len() != 1 || !s.get("running").is_some_and(Value::is_object))
    {
        return Err(Error::PreconditionFailed);
    }
    let container = containers[0]["containerID"]
        .as_str()
        .and_then(|v| v.strip_prefix("containerd://"))
        .ok_or(Error::IdentityMismatch)?;
    if container.len() != 64
        || !container
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::IdentityMismatch);
    }
    Ok(PodRuntimeIdentity {
        node: node.clone(),
        namespace: plan.namespace.clone(),
        namespace_uid: namespace_uid.into(),
        pod_name: plan.name.clone(),
        pod_uid: observed.uid().into(),
        container_id: container.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (EphemeralSandboxPlan, NodeIdentity, Value, Value) {
        let plan = crate::tests::plan();
        let node = NodeIdentity {
            name: "node-a".into(),
            uid: "node-uid".into(),
            boot_id: "boot-id".into(),
        };
        let mut pod = crate::tests::pod(&plan);
        pod["spec"]["nodeName"] = json!(node.name);
        pod["status"] = json!({"phase":"Running","containerStatuses":[{"name":"sandbox","restartCount":0,"state":{"running":{}},"containerID":format!("containerd://{}", "a".repeat(64))}]});
        let object = json!({"metadata":{"name":node.name,"uid":node.uid},"status":{"nodeInfo":{"bootID":node.boot_id},"conditions":[{"type":"Ready","status":"True"}]}});
        (plan, node, pod, object)
    }

    #[test]
    fn binds_exact_node_boot_pod_and_container_without_host_claims() {
        let (plan, node, pod, object) = fixture();
        let identity = runtime(&plan, "pod-uid", &node, "namespace-uid", &pod, &object).unwrap();
        assert_eq!(identity.node(), &node);
        assert_eq!(identity.container_id(), "a".repeat(64));
        assert_eq!(identity.namespace_uid(), "namespace-uid");
        for (pointer, value) in [
            ("/metadata/uid", json!("replacement")),
            ("/metadata/name", json!("node-b")),
            ("/metadata/deletionTimestamp", json!("2026-10-10T00:00:00Z")),
            ("/status/nodeInfo/bootID", json!("reboot")),
            ("/status/conditions", json!([])),
            (
                "/status/conditions",
                json!([{"type":"Ready","status":"False"}]),
            ),
            (
                "/status/conditions",
                json!([{"type":"Ready","status":"True"},{"type":"Ready","status":"True"}]),
            ),
        ] {
            let mut changed = object.clone();
            if pointer == "/metadata/deletionTimestamp" {
                changed["metadata"]["deletionTimestamp"] = value;
            } else {
                *changed.pointer_mut(pointer).unwrap() = value;
            }
            assert!(
                runtime(&plan, "pod-uid", &node, "namespace-uid", &pod, &changed).is_err(),
                "{pointer}"
            );
        }
    }

    #[test]
    fn rejects_replacement_wrong_node_restarts_and_ambiguous_container_status() {
        let (plan, node, pod, object) = fixture();
        for (pointer, value) in [
            ("/metadata/uid", json!("replacement")),
            ("/spec/nodeName", json!("node-b")),
            ("/status/phase", json!("Succeeded")),
            ("/status/containerStatuses", json!([])),
            ("/status/containerStatuses/0/name", json!("other")),
            ("/status/containerStatuses/0/restartCount", json!(1)),
            ("/status/containerStatuses/0/restartCount", json!(null)),
            (
                "/status/containerStatuses/0/state",
                json!({"terminated":{}}),
            ),
            (
                "/status/containerStatuses/0/state",
                json!({"running":{},"waiting":{}}),
            ),
            (
                "/status/containerStatuses/0/containerID",
                json!(format!("docker://{}", "a".repeat(64))),
            ),
            (
                "/status/containerStatuses/0/containerID",
                json!("containerd://../escape"),
            ),
        ] {
            let mut changed = pod.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            assert!(
                runtime(&plan, "pod-uid", &node, "namespace-uid", &changed, &object).is_err(),
                "{pointer}"
            );
        }
    }
}
