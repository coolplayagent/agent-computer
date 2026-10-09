//! Explicitly invoked component probe, never part of the default test suite.
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_kubernetes::{
    Client, Deployment, EphemeralSandboxPlan, Error, InstanceIdentity, PodPhase,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn field<'a>(value: &'a Value, name: &str) -> &'a str {
    value[name]
        .as_str()
        .expect("missing live configuration field")
}

#[tokio::test]
async fn real_kubernetes_instance_create_observe_delete() {
    let path = std::env::var("AGENT_COMPUTER_KUBE_TEST_CONFIG")
        .expect("explicit isolated Kubernetes configuration required");
    let config: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let ca = std::fs::read(field(&config, "ca_file")).unwrap();
    let bearer = std::fs::read_to_string(field(&config, "token_file")).unwrap();
    let namespace = field(&config, "namespace");
    let client = Client::new(
        field(&config, "endpoint"),
        &ca,
        bearer.trim(),
        Deployment {
            namespace: namespace.into(),
            namespace_uid: field(&config, "namespace_uid").into(),
            runtime_class_uid: field(&config, "runtime_class_uid").into(),
            deny_policy_uid: field(&config, "deny_policy_uid").into(),
            network_policy_ref: "deny-all".into(),
        },
    )
    .unwrap();
    client.probe().await.unwrap();
    let declaration = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet",
        "metadata":{"name":"component-probe"},"spec":{"sandboxes":[{
            "name":"probe","runtimeClass":"gvisor","image":field(&config,"image"),
            "resources":{"cpuMillis":500,"memoryMiB":256},"networkPolicyRef":"deny-all"}]}});
    let definition =
        validate_bytes(&serde_json::to_vec(&declaration).unwrap(), Format::Json).unwrap();
    let instance = format!(
        "probe_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let plan = EphemeralSandboxPlan::new(&definition, "probe", InstanceIdentity {
        organization:"component_probe".into(), computer:"probe_computer".into(), sandbox:"probe_sandbox".into(),
        instance, generation:1, spec_revision:1,
    }, namespace, vec!["/bin/sh".into(), "-c".into(),
        "test \"$(id -u)\" = 1000 && test ! -e /var/run/secrets/kubernetes.io/serviceaccount/token && ! touch /root-write-probe && echo AC_SECURITY_PROBE_OK && sleep 600".into()]).unwrap();
    let created = client.create(&plan).await.unwrap();
    assert_eq!(
        client.create(&plan).await.unwrap_err(),
        Error::ExistingObject
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let observed = loop {
        let p = client
            .observe(&plan, Some(created.uid()))
            .await
            .unwrap()
            .expect("created object");
        if p.phase() == PodPhase::Running {
            break p;
        }
        assert!(!matches!(p.phase(), PodPhase::Failed | PodPhase::Succeeded));
        assert!(
            tokio::time::Instant::now() < deadline,
            "Pod did not run before deadline"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    assert_eq!(
        client
            .observe(&plan, Some("different-uid"))
            .await
            .unwrap_err(),
        Error::IdentityMismatch
    );
    // The optional marker lets an external node inspector capture runsc and logs while
    // this real Pod exists. It is not a product callback or an authorization mechanism.
    if let Some(path) = config["observation_file"].as_str() {
        let p = Path::new(path);
        std::fs::write(
            p,
            serde_json::to_vec(
                &json!({"name":plan.pod_name(),"uid":observed.uid(),"namespace":namespace}),
            )
            .unwrap(),
        )
        .unwrap();
        let inspected = p.with_extension("inspected");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !inspected.exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "external component inspection missing"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    // Status may have changed; re-read once to get the current resourceVersion.
    let current = client
        .observe(&plan, Some(observed.uid()))
        .await
        .unwrap()
        .unwrap();
    client.delete(&plan, &current).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while client
        .observe(&plan, Some(observed.uid()))
        .await
        .unwrap()
        .is_some()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "API object was not removed"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    println!(
        "component probe passed: UID-bound creation, readback, conflict, conditional deletion; API absence is not physical fencing"
    );
}
