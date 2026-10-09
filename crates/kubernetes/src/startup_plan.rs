//! Ephemeral startup transport plan. Candidate/CSI mounts remain a separate plan.
use crate::{EphemeralSandboxPlan, Error, InstanceIdentity, Result, plan::BINDING};
use agent_computer_definitions::ValidatedDefinition;
use agent_computer_sandbox::{Bootstrap, MAX_REQUEST_BYTES};
use serde_json::json;

/// This plan requires an operator-approved image containing the trusted
/// supervisor AND its tools. It never trusts a tenant image merely because it
/// contains a binary at the expected path. It does not mount a Candidate.
#[derive(Clone, Debug)]
pub struct StartupSandboxPlan {
    pub(crate) pod: EphemeralSandboxPlan,
    pub(crate) bootstrap: Bootstrap,
}
impl StartupSandboxPlan {
    pub fn new(
        definition: &ValidatedDefinition,
        sandbox_name: &str,
        identity: InstanceIdentity,
        namespace: &str,
        approved_supervisor_image: &str,
        bootstrap: Bootstrap,
    ) -> Result<Self> {
        bootstrap.validate().map_err(|_| Error::InvalidCommand)?;
        if identity.instance != bootstrap.request.execution_id
            || identity.generation != bootstrap.request.generation
        {
            return Err(Error::InvalidIdentity);
        }
        let bytes = serde_json::to_string(&bootstrap).map_err(|_| Error::InvalidCommand)?;
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(Error::InvalidCommand);
        }
        let mut pod = EphemeralSandboxPlan::new(
            definition,
            sandbox_name,
            identity,
            namespace,
            vec![
                "/bin/agent-computer-sandbox".into(),
                "--attach-startup-json".into(),
            ],
        )?;
        if pod.pod["spec"]["containers"][0]["image"] != approved_supervisor_image {
            return Err(Error::UnsupportedSandbox);
        }
        let base: serde_json::Value =
            serde_json::from_str(&pod.binding).map_err(|_| Error::InvalidIdentity)?;
        pod.binding=serde_json::to_string(&json!({"version":2,"identity":base["identity"],"sandbox":base["sandbox"],"namespace":namespace,"bootstrap":bootstrap,"approved_supervisor_image":approved_supervisor_image,"workspace":"ephemeral"})).map_err(|_|Error::InvalidIdentity)?;
        pod.pod["metadata"]["annotations"][BINDING] = json!(pod.binding);
        let container = &mut pod.pod["spec"]["containers"][0];
        container["command"] = json!([
            "/bin/agent-computer-sandbox",
            "--attach-startup-json",
            bytes
        ]);
        container["stdin"] = json!(true);
        container["stdinOnce"] = json!(true);
        container["tty"] = json!(false);
        container["volumeMounts"]
            .as_array_mut()
            .ok_or(Error::InvalidIdentity)?
            .push(json!({"name":"workspace","mountPath":"/workspace"}));
        pod.pod["spec"]["volumes"]
            .as_array_mut()
            .ok_or(Error::InvalidIdentity)?
            .push(json!({"name":"workspace","emptyDir":{"medium":"Memory","sizeLimit":"64Mi"}}));
        Ok(Self { pod, bootstrap })
    }
    pub fn pod_plan(&self) -> &EphemeralSandboxPlan {
        &self.pod
    }
    pub fn bootstrap(&self) -> &Bootstrap {
        &self.bootstrap
    }
}
