use crate::{
    Deployment, EphemeralSandboxPlan, Error, PodObservation, PodPhase, Result,
    plan::{BINDING, IDENTITY, opaque},
};
use serde_json::{Value, json};

pub(crate) fn deployment(
    d: &Deployment,
    ns: &Value,
    runtime: &Value,
    policies: &Value,
) -> Result<()> {
    let items = policies["items"].as_array().ok_or(Error::InvalidResponse)?;
    if ns["metadata"]["name"] != d.namespace
        || ns["metadata"]["uid"] != d.namespace_uid
        || !ns["metadata"]["deletionTimestamp"].is_null()
        || ns["status"]["phase"] != "Active"
        || ns["metadata"]["labels"]["pod-security.kubernetes.io/enforce"] != "restricted"
        || runtime["metadata"]["name"] != "gvisor"
        || runtime["metadata"]["uid"] != d.runtime_class_uid
        || !runtime["metadata"]["deletionTimestamp"].is_null()
        || runtime["handler"] != "runsc"
        || !runtime["overhead"].is_null()
        || !runtime["scheduling"].is_null()
        || items.len() != 1
        || !policies["metadata"]["continue"]
            .as_str()
            .unwrap_or("")
            .is_empty()
    {
        return Err(Error::PreconditionFailed);
    }
    let policy = &items[0];
    let spec = &policy["spec"];
    let types = spec["policyTypes"]
        .as_array()
        .ok_or(Error::InvalidResponse)?;
    if policy["metadata"]["namespace"] != d.namespace
        || policy["metadata"]["uid"] != d.deny_policy_uid
        || !policy["metadata"]["deletionTimestamp"].is_null()
        || spec["podSelector"] != json!({})
        || types.len() != 2
        || !types.contains(&json!("Ingress"))
        || !types.contains(&json!("Egress"))
        || !empty_or_absent(&spec["ingress"])
        || !empty_or_absent(&spec["egress"])
    {
        return Err(Error::PreconditionFailed);
    }
    Ok(())
}

fn empty_or_absent(v: &Value) -> bool {
    v.is_null() || v.as_array().is_some_and(Vec::is_empty)
}

pub(crate) fn pod(
    plan: &EphemeralSandboxPlan,
    pod: &Value,
    expected_uid: Option<&str>,
) -> Result<PodObservation> {
    let meta = &pod["metadata"];
    let uid = meta["uid"]
        .as_str()
        .filter(|s| opaque(s))
        .ok_or(Error::InvalidResponse)?;
    let version = meta["resourceVersion"]
        .as_str()
        .filter(|s| opaque(s))
        .ok_or(Error::InvalidResponse)?;
    if pod["apiVersion"] != "v1"
        || pod["kind"] != "Pod"
        || meta["name"] != plan.name
        || meta["namespace"] != plan.namespace
        || meta["annotations"][BINDING] != plan.binding
        || meta["annotations"] != plan.pod["metadata"]["annotations"]
        || meta["labels"][IDENTITY] != plan.pod["metadata"]["labels"][IDENTITY]
        || expected_uid.is_some_and(|expected| expected != uid)
        || !matches_spec(&plan.pod["spec"], &pod["spec"], "")
    {
        return Err(Error::IdentityMismatch);
    }
    let phase = if !meta["deletionTimestamp"].is_null() {
        PodPhase::Deleting
    } else {
        match pod["status"]["phase"].as_str() {
            Some("Pending") => PodPhase::Pending,
            Some("Running") => PodPhase::Running,
            Some("Succeeded") => PodPhase::Succeeded,
            Some("Failed") => PodPhase::Failed,
            _ => PodPhase::Unknown,
        }
    };
    Ok(PodObservation {
        uid: uid.into(),
        resource_version: version.into(),
        binding: plan.binding.clone(),
        phase,
    })
}

// Exact controlled fields, plus a finite set of harmless Kubernetes defaults. Unknown
// spec fields, sidecars, init/ephemeral containers, mounts and injected env are rejected.
fn matches_spec(expected: &Value, actual: &Value, path: &str) -> bool {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            e.iter().all(|(key, value)| {
                matches_spec(
                    value,
                    a.get(key).unwrap_or(&Value::Null),
                    &format!("{path}/{key}"),
                )
            }) && a.iter().all(|(key, value)| {
                e.contains_key(key) || allowed_default(&format!("{path}/{key}"), value)
            })
        }
        (Value::Array(e), Value::Array(a)) => {
            e.len() == a.len()
                && e.iter()
                    .zip(a)
                    .all(|(e, a)| matches_spec(e, a, &format!("{path}/*")))
        }
        // Only fields whose Kubernetes default is false may use omitempty. In
        // particular, absent allowPrivilegeEscalation/automountServiceAccountToken/
        // enableServiceLinks must never be interpreted as explicitly disabled.
        (Value::Bool(false), Value::Null) => matches!(
            path,
            "/hostNetwork"
                | "/hostPID"
                | "/hostIPC"
                | "/shareProcessNamespace"
                | "/containers/*/securityContext/privileged"
        ),
        _ => expected == actual,
    }
}

fn allowed_default(path: &str, value: &Value) -> bool {
    match path {
        "/dnsPolicy" => value == "ClusterFirst",
        "/schedulerName" => value == "default-scheduler",
        "/serviceAccount" | "/serviceAccountName" => value == "default",
        "/priority" => value == 0,
        "/preemptionPolicy" => value == "PreemptLowerPriority",
        "/nodeName" => value
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= 253),
        "/containers/*/terminationMessagePath" => value == "/dev/termination-log",
        "/containers/*/terminationMessagePolicy" => value == "File",
        "/containers/*/volumeMounts/*/readOnly" => value == false,
        "/tolerations" => value.as_array().is_some_and(|items| {
            items.len() <= 2
                && items.iter().all(|v| {
                    (v["key"] == "node.kubernetes.io/not-ready"
                        || v["key"] == "node.kubernetes.io/unreachable")
                        && v["operator"] == "Exists"
                        && v["effect"] == "NoExecute"
                        && v["tolerationSeconds"] == 300
                        && v.as_object().is_some_and(|o| o.len() == 4)
                })
        }),
        _ => false,
    }
}
