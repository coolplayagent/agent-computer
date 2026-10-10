//! Explicit component test: fixture grants, ephemeral workspace, no DB acceptance.
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_kubernetes::{
    Client, Deployment, InstanceIdentity, PodPhase, StartupSandboxPlan,
};
use agent_computer_sandbox::{Bootstrap, Request, StartupGrant};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn field<'a>(value: &'a Value, name: &str) -> &'a str {
    value[name].as_str().expect("missing live configuration")
}

#[tokio::test]
async fn real_kubernetes_v5_startup_attach() {
    let path = std::env::var("AGENT_COMPUTER_KUBE_STARTUP_CONFIG")
        .expect("explicit disposable Kubernetes configuration required");
    let config: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let ca = std::fs::read(field(&config, "ca_file")).unwrap();
    let bearer = std::fs::read_to_string(field(&config, "token_file")).unwrap();
    let ns = field(&config, "namespace");
    let client = Client::new(
        field(&config, "endpoint"),
        &ca,
        bearer.trim(),
        Deployment {
            namespace: ns.into(),
            namespace_uid: field(&config, "namespace_uid").into(),
            runtime_class_uid: field(&config, "runtime_class_uid").into(),
            deny_policy_uid: field(&config, "deny_policy_uid").into(),
            network_policy_ref: "deny-all".into(),
        },
    )
    .unwrap();
    let image = field(&config, "image");
    let definition=validate_bytes(&serde_json::to_vec(&json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet",
        "metadata":{"name":"startup-probe"},"spec":{"sandboxes":[{"name":"probe","runtimeClass":"gvisor","image":image,
        "resources":{"cpuMillis":500,"memoryMiB":256},"networkPolicyRef":"deny-all"}]}})).unwrap(), Format::Json).unwrap();
    let mut records = Vec::new();
    for (case, command, timeout, budget, outcome) in [
        (
            "success",
            "printf 'attach-output'; printf 'attach-error' >&2; test ! -e /var/run/secrets/kubernetes.io/serviceaccount/token && test -z \"${KUBERNETES_SERVICE_HOST+x}\" && test -z \"${HOSTNAME+x}\"",
            10,
            15000,
            "succeeded",
        ),
        ("timeout", "sleep 10", 1, 15000, "timed_out"),
        ("lease", "sleep 10", 10, 2000, "lease_expired"),
        (
            "bounded_output",
            "head -c 100000 /dev/zero",
            10,
            15000,
            "succeeded",
        ),
    ] {
        let execution_id = format!(
            "attach_{}_{}",
            case,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let plan = StartupSandboxPlan::new(
            &definition,
            "probe",
            InstanceIdentity {
                organization: "component_probe".into(),
                computer: "probe_computer".into(),
                sandbox: "probe_sandbox".into(),
                instance: execution_id.clone(),
                generation: 1,
                spec_revision: 1,
            },
            ns,
            image,
            Bootstrap {
                version: 1,
                hard_budget_ms: None,
                intent_digest: format!("sha256:{}", "a".repeat(64)),
                request: Request {
                    execution_id,
                    generation: 1,
                    argv: vec!["/bin/sh".into(), "-c".into(), command.into()],
                    cwd: String::new(),
                    timeout_seconds: timeout,
                    lease_budget_ms: 30000,
                    term_grace_ms: 100,
                    output_limit_bytes: 64,
                },
            },
        )
        .unwrap();
        let created = client.create(plan.pod_plan()).await.unwrap();
        // Keep cleanup outside the fallible probe so transport errors do not leave a Pod.
        let result: Result<Value,String> = async {
            let deadline=tokio::time::Instant::now()+Duration::from_secs(120);
            let observed=loop {
                let p=client.observe(plan.pod_plan(),Some(created.uid())).await.map_err(|e|e.to_string())?.ok_or("missing pod")?;
                if p.phase()==PodPhase::Running {break p;}
                if matches!(p.phase(),PodPhase::Failed|PodPhase::Succeeded) || tokio::time::Instant::now()>=deadline {return Err(format!("startup phase {:?}",p.phase()));}
                tokio::time::sleep(Duration::from_millis(200)).await;
            };
            if case=="success" {
                // Optional node inspection occurs before hello, while PID 1 waits.
                if let Some(path)=config["observation_file"].as_str() {
                    let p=Path::new(path);
                    std::fs::write(p,serde_json::to_vec(&json!({"name":plan.pod_plan().pod_name(),"uid":observed.uid(),"namespace":ns})).unwrap()).map_err(|e|e.to_string())?;
                    let deadline=tokio::time::Instant::now()+Duration::from_secs(15);
                    while !p.with_extension("inspected").exists() {
                        if tokio::time::Instant::now()>=deadline{return Err("node inspection timeout".into());}
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            let channel=client.attach_startup(&plan,&observed).await.map_err(|e|format!("attach: {e}"))?;
            let grant=StartupGrant {version:1, hard_budget_ms: None,challenge_digest:channel.challenge().digest().unwrap(),lease_budget_ms:budget};
            let observation=channel.run(&grant).await.map_err(|e|format!("run: {e}"))?;
            let report:Value=serde_json::from_slice(observation.report_bytes()).map_err(|e|e.to_string())?;
            Ok(json!({"case":case,"pod_name":plan.pod_plan().pod_name(),"pod_uid":observation.pod_uid(),"bootstrap":plan.bootstrap(),
                "grant":grant,"observation":report,"supervisor_stderr":observation.supervisor_stderr()}))
        }.await;
        let current = client
            .observe(plan.pod_plan(), Some(created.uid()))
            .await
            .unwrap()
            .unwrap();
        client.delete(plan.pod_plan(), &current).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while client
            .observe(plan.pod_plan(), Some(created.uid()))
            .await
            .unwrap()
            .is_some()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "API deletion timeout"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let record = result.unwrap_or_else(|e| panic!("{case}: {e}"));
        let report = &record["observation"]["report"];
        assert_eq!(report["outcome"], outcome, "{record}");
        assert_eq!(report["children_reaped"], true);
        assert_eq!(report["stdout"]["eof"], true);
        assert_eq!(report["stderr"]["eof"], true);
        if case == "success" {
            assert_eq!(report["stdout"]["bytes"], json!(b"attach-output".to_vec()));
            assert_eq!(report["stderr"]["bytes"], json!(b"attach-error".to_vec()));
        }
        if case == "bounded_output" {
            assert_eq!(report["stdout"]["bytes"], json!(vec![0; 64]));
            assert_eq!(report["stdout"]["observed_bytes"], 100000);
            assert_eq!(report["stdout"]["truncated"], true);
        }
        records.push(record);
    }
    std::fs::write(field(&config,"result_file"),serde_json::to_vec_pretty(&json!({"scope":"ephemeral startup attach component, fixture grants only","cases":records})).unwrap()).unwrap();
    println!(
        "4 real Kubernetes v5 attach cases passed; no Candidate, DB authorization or physical fence inferred"
    );
}
