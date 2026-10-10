//! Opt-in disposable K3s probe. The fixture policy is not database authorization.
use agent_computer_browser::{Action, Binding, Callback, Decision, Grant, Operation, Policy};
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_kubernetes::{BrowserPlan, Client, Deployment, InstanceIdentity, PodPhase};
use agent_computer_node::{BrowserGuard, Configuration, arm_browser, read_recorded_browser_stop};
use agent_computer_watchdog::boottime_ms;
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};

struct Fixture<'a> {
    guard: &'a mut BrowserGuard,
    binding: Binding,
    callbacks: Vec<Value>,
    dispatches: usize,
    deny_launch: bool,
    lose_completion: bool,
}
impl Policy for Fixture<'_> {
    async fn answer(&mut self, callback: &Callback) -> agent_computer_browser::Result<Decision> {
        let remaining = self
            .guard
            .remaining_budget_ms()
            .map_err(|_| agent_computer_browser::Error::Denied)?;
        if remaining < 1000 {
            return Ok(Decision::Deny);
        }
        Ok(match callback {
            Callback::Authorize(request) => {
                assert_eq!(request.scope, self.binding.scope);
                assert_eq!(request.actor, self.binding.actor);
                self.callbacks.push(json!({"authorize":request}));
                if self.deny_launch {
                    Decision::Deny
                } else {
                    Decision::Grant(Grant {
                        scope: request.scope.clone(),
                        actor: request.actor.clone(),
                        permission: request.permission,
                        control_epoch: 1,
                        valid_for_ms: remaining.min(30000),
                    })
                }
            }
            Callback::Begin(intent) => {
                assert_eq!(intent.scope, self.binding.scope);
                assert_eq!(intent.actor, self.binding.actor);
                assert_eq!(intent.control_epoch, 1);
                self.callbacks.push(json!({"begin":intent}));
                self.dispatches += 1;
                Decision::Dispatch
            }
            Callback::Complete(receipt) => {
                self.callbacks.push(json!({"complete":receipt}));
                if self.lose_completion {
                    Decision::Deny
                } else {
                    Decision::Recorded
                }
            }
        })
    }
}
fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().expect("private configuration field")
}

#[tokio::test]
async fn real_browser_pod_attach_node_guard_and_physical_stop() {
    let path = std::env::var("AGENT_COMPUTER_BROWSER_LIVE_CONFIG")
        .expect("explicit isolated root-owned VM configuration required");
    let config: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let node: Configuration = serde_json::from_value(config["node"].clone()).unwrap();
    let deployment: Deployment = serde_json::from_value(config["deployment"].clone()).unwrap();
    let client = Client::new(
        field(&config, "endpoint"),
        &fs::read(field(&config, "ca_file")).unwrap(),
        fs::read_to_string(field(&config, "token_file"))
            .unwrap()
            .trim(),
        deployment.clone(),
    )
    .unwrap();
    client.probe().await.unwrap();
    let image = field(&config, "image");
    let definition=validate_bytes(&serde_json::to_vec(&json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"browser-live"},"spec":{"sandboxes":[{"name":"browser","runtimeClass":"gvisor","image":image,"resources":{"cpuMillis":3000,"memoryMiB":4096},"networkPolicyRef":"deny-all"}]}})).unwrap(),Format::Json).unwrap();
    let mut results = Vec::new();
    for case in ["normal", "completion-lost", "launch-denied", "deadline"] {
        let instance = format!("browser_{}_{}_{}", std::process::id(), boottime_ms(), case);
        let binding:Binding=serde_json::from_value(json!({"scope":{"organization_id":"fixture_org","computer_id":"fixture_computer","computer_generation":1,"app_instance_id":instance,"app_generation":1,"profile_owner_id":"fixture_owner"},"actor":{"principal_id":"fixture_owner","connection_session_id":"fixture_connection"},"viewport":{"width":800,"height":600},"runtime_ms":60000})).unwrap();
        let plan = BrowserPlan::new(
            &definition,
            "browser",
            InstanceIdentity {
                organization: binding.scope.organization_id.clone(),
                computer: binding.scope.computer_id.clone(),
                sandbox: "fixture_sandbox".into(),
                instance,
                generation: 1,
                spec_revision: 1,
            },
            &deployment.namespace,
            image,
            binding.clone(),
            node.node.clone(),
        )
        .unwrap();
        let created = client.create(plan.pod_plan()).await.unwrap();
        let until = tokio::time::Instant::now() + Duration::from_secs(120);
        let identity = loop {
            let observed = client
                .observe(plan.pod_plan(), Some(created.uid()))
                .await
                .unwrap()
                .unwrap();
            assert!(
                !matches!(observed.phase(), PodPhase::Failed | PodPhase::Succeeded),
                "{case}: premature Pod exit"
            );
            if observed.phase() == PodPhase::Running
                && let Ok(identity) = client.observe_browser_runtime(&plan, created.uid()).await
            {
                break identity;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "{case}: Pod never became admissible"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let mut channel = client.attach_browser(&plan, &created).await.unwrap();
        assert_eq!(channel.runtime().pod(), identity.pod());
        let mut guard =
            arm_browser(&node, channel.runtime().clone(), boottime_ms() + 25000).unwrap();
        let arm = serde_json::to_value(guard.evidence()).unwrap();
        assert_eq!(arm["runtime"]["kind"], "browser");
        assert!(arm["runtime"].get("workspace_mount").is_none());
        assert_eq!(arm["runtime"]["profile"]["filesystem"], 0x0102_1994u64);
        // A separate trusted node inspector may inspect live CRI/cgroups while
        // this probe owns the guard. No product acceptance comes from this file.
        fs::write(
            field(&config, "observation_file"),
            serde_json::to_vec(&json!({"case":case,"arm":arm,"pod":plan.pod_plan().manifest()}))
                .unwrap(),
        )
        .unwrap();
        let mut policy = Fixture {
            guard: &mut guard,
            binding: binding.clone(),
            callbacks: Vec::new(),
            dispatches: 0,
            deny_launch: case == "launch-denied",
            lose_completion: case == "completion-lost",
        };
        let launch = channel
            .session_mut()
            .call(Operation::Launch, &mut policy)
            .await
            .unwrap();
        let mut result =
            json!({"case":case,"arm":arm,"launch_ok":launch.ok(),"launch":launch.value()});
        if case == "launch-denied" {
            assert!(!launch.ok());
            assert!(channel.session_mut().is_closed());
            assert_eq!(policy.dispatches, 0);
        } else {
            assert!(launch.ok(), "{case}: {}", launch.value());
            let pages = channel
                .session_mut()
                .call(
                    Operation::Pages {
                        actor: binding.actor.clone(),
                    },
                    &mut policy,
                )
                .await
                .unwrap();
            assert!(pages.ok());
            let page = pages.value()[0]["page_id"].as_str().unwrap().to_owned();
            let observation = channel
                .session_mut()
                .call(
                    Operation::Observe {
                        actor: binding.actor.clone(),
                        page_id: page,
                    },
                    &mut policy,
                )
                .await
                .unwrap();
            assert!(observation.ok(), "{}", observation.value());
            assert_eq!(observation.value()["scope"], json!(binding.scope));
            assert!(
                observation.value()["screenshot_base64"]
                    .as_str()
                    .unwrap()
                    .len()
                    > 100
            );
            result["observation"] = json!({"scope":observation.value()["scope"],"screenshot_sha256":observation.value()["screenshot_sha256"],"viewport":observation.value()["viewport"]});
            let action = Operation::Act {
                actor: binding.actor.clone(),
                action_id: "new_page".into(),
                action: Action::CreatePage {},
            };
            let first = channel
                .session_mut()
                .call(action.clone(), &mut policy)
                .await
                .unwrap();
            assert!(first.ok(), "{}", first.value());
            assert_eq!(first.value()["recorded"], !policy.lose_completion);
            assert_eq!(
                first.value()["receipt"]["outcome"],
                if policy.lose_completion {
                    "unknown"
                } else {
                    "delivered"
                }
            );
            let retry = channel
                .session_mut()
                .call(action, &mut policy)
                .await
                .unwrap();
            assert_eq!(retry.value(), first.value());
            assert_eq!(policy.dispatches, 1);
            if policy.lose_completion {
                assert!(
                    channel
                        .session_mut()
                        .call(
                            Operation::Act {
                                actor: binding.actor.clone(),
                                action_id: "second_page".into(),
                                action: Action::CreatePage {}
                            },
                            &mut policy
                        )
                        .await
                        .is_err()
                );
                assert_eq!(policy.dispatches, 1);
            }
            result["action"] = first.value().clone();
            if case != "deadline" {
                let close = channel
                    .session_mut()
                    .call(Operation::Close, &mut policy)
                    .await
                    .unwrap();
                assert!(close.ok());
                assert_eq!(close.value()["physical_termination"], false);
                result["close"] = close.value().clone();
            }
        }
        result["callbacks"] = json!(policy.callbacks);
        drop(policy);
        // Keep the idle pipe open for expiry: EOF would otherwise let Kubelet
        // stop the Pod naturally before the independent watchdog deadline.
        if case == "deadline" {
            let until = tokio::time::Instant::now() + Duration::from_secs(30);
            while !guard.process_termination_observed() {
                assert!(
                    tokio::time::Instant::now() < until,
                    "watchdogs failed to terminate original domain"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(guard.remaining_budget_ms().is_err());
        }
        // A closed transport still does not substitute for the original pidfds.
        drop(channel);
        let stopped = guard.stop_browser().unwrap();
        let stop = serde_json::to_value(stopped.evidence()).unwrap();
        let stored = read_recorded_browser_stop(&node.spool, &arm)
            .unwrap()
            .unwrap();
        assert_eq!(stored.evidence(), &stop);
        assert!(
            agent_computer_node::read_recorded_seal(&node.spool, &arm)
                .unwrap()
                .is_none()
        );
        result["physical_stop"] = stop;
        results.push(result);
        fs::write(
            Path::new(field(&config, "result_file")),
            serde_json::to_vec_pretty(&json!({"results":results,"status":"in_progress"})).unwrap(),
        )
        .unwrap();
        let observed = client
            .observe(plan.pod_plan(), Some(created.uid()))
            .await
            .unwrap()
            .unwrap();
        client.delete(plan.pod_plan(), &observed).await.unwrap();
        // Operator-only fixture cleanup after the live StoppedBrowser proof.
        // Preserve UID preconditions; never treat this API deletion as fencing.
        // It releases stale kubelet accounting before the next 3-CPU test Pod.
        let cleanup = tempfile::NamedTempFile::new().unwrap();
        fs::write(cleanup.path(),serde_json::to_vec(&json!({"apiVersion":"v1","kind":"DeleteOptions","gracePeriodSeconds":0,"preconditions":{"uid":created.uid()}})).unwrap()).unwrap();
        let path = format!(
            "/api/v1/namespaces/{}/pods/{}",
            deployment.namespace,
            plan.pod_plan().pod_name()
        );
        let deleted = std::process::Command::new("/usr/local/bin/k3s")
            .args([
                "kubectl",
                "--request-timeout=10s",
                "delete",
                "--raw",
                &path,
                "-f",
            ])
            .arg(cleanup.path())
            .output()
            .unwrap();
        assert!(deleted.status.success(), "operator fixture cleanup failed");
        let until = tokio::time::Instant::now() + Duration::from_secs(30);
        while client
            .observe(plan.pod_plan(), Some(created.uid()))
            .await
            .unwrap()
            .is_some()
        {
            assert!(
                tokio::time::Instant::now() < until,
                "UID-bound fixture deletion not reconciled"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // Allow the node's admission manager to retire the completed fixture.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    fs::write(Path::new(field(&config,"result_file")),serde_json::to_vec_pretty(&json!({"results":results,"limitations":["fixture callback authority; no database admission or production App dispatch","private volatile profile; no encrypted checkpoint/restore","one disposable K3s node"]})).unwrap()).unwrap();
    println!("BROWSER_KUBERNETES_NODE_COMPONENT_PASSED");
}
