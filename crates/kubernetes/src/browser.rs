//! A separate, approved Browser Pod. No Candidate/PVC or service credentials.
use crate::{
    Client, EphemeralSandboxPlan, Error, InstanceIdentity, NodeIdentity, PodRuntimeIdentity,
    Result, node, plan::BINDING,
};
use agent_computer_browser::Binding;
use agent_computer_definitions::ValidatedDefinition;
use serde::Serialize;
use serde_json::{Value, json};

pub const BROWSER_ENTRYPOINT: &str = "/opt/agent-computer/browser-driver/worker.js";

#[derive(Clone, Debug)]
pub struct BrowserPlan {
    pub(crate) pod: EphemeralSandboxPlan,
    binding: Binding,
    node: NodeIdentity,
}
impl BrowserPlan {
    /// Inputs must come from durable App admission and trusted operator bindings.
    /// A compiled plan is reviewable data, never permission to create a Pod.
    pub fn new(
        definition: &ValidatedDefinition,
        sandbox_name: &str,
        identity: InstanceIdentity,
        namespace: &str,
        approved_image: &str,
        binding: Binding,
        node: NodeIdentity,
    ) -> Result<Self> {
        binding.validate().map_err(|_| Error::InvalidIdentity)?;
        node.validate()?;
        if identity.organization != binding.scope.organization_id
            || identity.computer != binding.scope.computer_id
            || identity.instance != binding.scope.app_instance_id
            || identity.generation != binding.scope.app_generation
        {
            return Err(Error::InvalidIdentity);
        }
        let command = vec![
            "/usr/local/bin/node".into(),
            BROWSER_ENTRYPOINT.into(),
            "--stdio-binding-json".into(),
            binding.json().map_err(|_| Error::InvalidCommand)?,
        ];
        let mut pod =
            EphemeralSandboxPlan::new(definition, sandbox_name, identity, namespace, command)?;
        if pod.pod["spec"]["containers"][0]["image"] != approved_image {
            return Err(Error::UnsupportedSandbox);
        }
        let base: Value = serde_json::from_str(&pod.binding).map_err(|_| Error::InvalidIdentity)?;
        pod.binding = serde_json::to_string(&json!({"version":3,"kind":"browser", "base":base,
            "browser":binding,"approved_image":approved_image,"node":node}))
        .map_err(|_| Error::InvalidIdentity)?;
        pod.pod["metadata"]["annotations"][BINDING] = json!(pod.binding);
        pod.pod["spec"]["nodeName"] = json!(node.name);
        pod.pod["spec"]["activeDeadlineSeconds"] = json!(binding.runtime_ms.div_ceil(1000) + 45);
        // Non-root main cannot chmod the kubelet-owned emptyDir root. The same
        // approved image creates one fresh, private leaf before its subPath is
        // mounted. This init step has no input pipe or authority callbacks.
        let mut prepare = pod.pod["spec"]["containers"][0].clone();
        prepare["name"] = json!("prepare-profile");
        prepare["command"] = json!([
            "/usr/local/bin/node",
            BROWSER_ENTRYPOINT,
            "--prepare-profile-volume"
        ]);
        prepare["volumeMounts"] = json!([{"name":"browser-profile","mountPath":"/browser-volume"}]);
        pod.pod["spec"]["initContainers"] = json!([prepare]);
        let container = &mut pod.pod["spec"]["containers"][0];
        container["stdin"] = json!(true);
        container["stdinOnce"] = json!(true);
        container["tty"] = json!(false);
        container["volumeMounts"]
            .as_array_mut()
            .ok_or(Error::InvalidIdentity)?
            .push(
                json!({"name":"browser-profile","mountPath":"/browser-state","subPath":"private"}),
            );
        pod.pod["spec"]["volumes"][0]["emptyDir"]["sizeLimit"] = json!("512Mi");
        pod.pod["spec"]["volumes"].as_array_mut().ok_or(Error::InvalidIdentity)?
            .push(json!({"name":"browser-profile","emptyDir":{"medium":"Memory","sizeLimit":"512Mi"}}));
        Ok(Self { pod, binding, node })
    }
    pub fn pod_plan(&self) -> &EphemeralSandboxPlan {
        &self.pod
    }
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn node(&self) -> &NodeIdentity {
        &self.node
    }
    pub fn command(&self) -> &Value {
        &self.pod.pod["spec"]["containers"][0]["command"]
    }
}

/// Constructed only after exact Pod/Node readback, including successful private
/// profile preparation. The node adapter must still verify CRI, mounts and tasks.
#[derive(Clone, Debug, Serialize)]
pub struct BrowserRuntimeIdentity {
    pod: PodRuntimeIdentity,
    binding: Binding,
    command: Value,
}
impl BrowserRuntimeIdentity {
    pub fn pod(&self) -> &PodRuntimeIdentity {
        &self.pod
    }
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn command(&self) -> &Value {
        &self.command
    }
    pub fn into_parts(self) -> (PodRuntimeIdentity, Binding, Value) {
        (self.pod, self.binding, self.command)
    }
}

impl Client {
    pub async fn observe_browser_runtime(
        &self,
        plan: &BrowserPlan,
        pod_uid: &str,
    ) -> Result<BrowserRuntimeIdentity> {
        self.check_plan(&plan.pod)?;
        self.probe_sandbox_storage(&plan.pod).await?;
        let pod = self
            .get(&self.pod_path(&plan.pod, true))
            .await?
            .ok_or(Error::PreconditionFailed)?;
        let object = self
            .get(&format!("/api/v1/nodes/{}", plan.node.name))
            .await?
            .ok_or(Error::PreconditionFailed)?;
        browser_runtime(plan, pod_uid, self.namespace_uid(), &pod, &object)
    }
}
fn browser_runtime(
    plan: &BrowserPlan,
    uid: &str,
    namespace_uid: &str,
    pod: &Value,
    node: &Value,
) -> Result<BrowserRuntimeIdentity> {
    let identity = node::runtime(&plan.pod, uid, &plan.node, namespace_uid, pod, node)?;
    let init = pod
        .pointer("/status/initContainerStatuses")
        .and_then(Value::as_array)
        .ok_or(Error::InvalidResponse)?;
    if init.len() != 1
        || init[0]["name"] != "prepare-profile"
        || init[0]["restartCount"].as_u64() != Some(0)
        || init[0]["state"]
            .as_object()
            .is_none_or(|s| s.len() != 1 || !s.contains_key("terminated"))
        || init[0]["state"]["terminated"]["exitCode"].as_u64() != Some(0)
        || init[0]["state"]["terminated"]["reason"] != "Completed"
    {
        return Err(Error::PreconditionFailed);
    }
    Ok(BrowserRuntimeIdentity {
        pod: identity,
        binding: plan.binding.clone(),
        command: plan.command().clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (BrowserPlan, Value, Value) {
        let binding = serde_json::from_value(json!({"scope":{"organization_id":"org_a","computer_id":"computer_a","computer_generation":1,"app_instance_id":"instance_a","app_generation":1,"profile_owner_id":"owner"},"actor":{"principal_id":"owner","connection_session_id":"connection"},"viewport":{"width":800,"height":600},"runtime_ms":60000})).unwrap();
        let node = NodeIdentity {
            name: "node-a".into(),
            uid: "node-uid".into(),
            boot_id: "boot-id".into(),
        };
        let plan = BrowserPlan::new(
            &crate::tests::definition(false),
            "sandbox",
            crate::tests::identity(),
            "ac-test",
            &format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
            binding,
            node.clone(),
        )
        .unwrap();
        let mut pod = crate::tests::pod(&plan.pod);
        pod["spec"]["initContainers"][0]["securityContext"]
            .as_object_mut()
            .unwrap()
            .remove("privileged");
        pod["spec"]["initContainers"][0]["terminationMessagePath"] = json!("/dev/termination-log");
        pod["spec"]["initContainers"][0]["terminationMessagePolicy"] = json!("File");
        pod["status"] = json!({"phase":"Running","containerStatuses":[{"name":"sandbox","restartCount":0,"state":{"running":{}},"containerID":format!("containerd://{}", "a".repeat(64))}],"initContainerStatuses":[{"name":"prepare-profile","restartCount":0,"state":{"terminated":{"exitCode":0,"reason":"Completed"}}}]});
        let object = json!({"metadata":{"name":node.name,"uid":node.uid},"status":{"nodeInfo":{"bootID":node.boot_id},"conditions":[{"type":"Ready","status":"True"}]}});
        (plan, pod, object)
    }

    #[test]
    fn browser_plan_binds_approved_image_private_profile_and_never_candidate_mounts() {
        let (plan, pod, node) = fixture();
        let runtime = browser_runtime(&plan, "pod-uid", "namespace-uid", &pod, &node).unwrap();
        assert_eq!(runtime.binding(), plan.binding());
        assert_eq!(
            pod["spec"]["initContainers"][0]["image"],
            pod["spec"]["containers"][0]["image"]
        );
        assert_eq!(pod["spec"]["volumes"][2]["emptyDir"]["medium"], "Memory");
        assert_eq!(
            pod["spec"]["containers"][0]["volumeMounts"][2],
            json!({"name":"browser-profile","mountPath":"/browser-state","subPath":"private"})
        );
        assert_eq!(pod["spec"]["activeDeadlineSeconds"], 105);
        for (definition, mut identity, image) in [
            (
                crate::tests::definition(true),
                crate::tests::identity(),
                pod["spec"]["containers"][0]["image"].as_str().unwrap(),
            ),
            (
                crate::tests::definition(false),
                crate::tests::identity(),
                "unapproved",
            ),
        ] {
            assert!(
                BrowserPlan::new(
                    &definition,
                    "sandbox",
                    identity.clone(),
                    "ac-test",
                    image,
                    plan.binding.clone(),
                    plan.node.clone()
                )
                .is_err()
            );
            identity.generation = 2;
            assert!(
                BrowserPlan::new(
                    &definition,
                    "sandbox",
                    identity,
                    "ac-test",
                    image,
                    plan.binding.clone(),
                    plan.node.clone()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn browser_readback_rejects_failed_init_or_any_security_binding_change() {
        let (plan, pod, node) = fixture();
        for (pointer, value) in [
            ("/status/initContainerStatuses", json!([])),
            ("/status/initContainerStatuses/0/restartCount", json!(1)),
            (
                "/status/initContainerStatuses/0/state",
                json!({"running":{}}),
            ),
            (
                "/status/initContainerStatuses/0/state/terminated/exitCode",
                json!(1),
            ),
            (
                "/status/initContainerStatuses/0/state/terminated/reason",
                json!("Error"),
            ),
            ("/spec/initContainers/0/image", json!("unapproved")),
            ("/spec/initContainers/0/command", json!(["sh"])),
            ("/spec/containers/0/stdinOnce", json!(false)),
            ("/spec/containers/0/volumeMounts/2/subPath", json!("other")),
            (
                "/spec/volumes/2",
                json!({"name":"browser-profile","hostPath":{"path":"/"}}),
            ),
            ("/spec/nodeName", json!("other")),
        ] {
            let mut changed = pod.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            assert!(
                browser_runtime(&plan, "pod-uid", "namespace-uid", &changed, &node).is_err(),
                "{pointer}"
            );
        }
    }
}
