//! Explicit root-owned disposable environment; fixture grants, real JuiceFS/CSI.
use agent_computer_definitions::{Format, validate_bytes};
use agent_computer_kubernetes::{
    CandidateMount, Client, InstanceIdentity, PodPhase, StartupSandboxPlan,
    volume::{StorageClassBinding, VolumeIdentity, VolumePlan},
};
use agent_computer_sandbox::{Bootstrap, Request, StartupGrant};
use agent_computer_storage::{
    Entry, Manifest, MountedVolume, ObjectCache, PrepareRequest,
    quota::{JuiceFsConfig, JuiceFsQuota},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().expect("private configuration field missing")
}
fn metadata(path: &Path) -> Value {
    let m = fs::metadata(path).unwrap();
    json!({"inode":m.ino(),"uid":m.uid(),"gid":m.gid(),"mode":m.mode()})
}
#[tokio::test]
async fn candidate_data_leaf_mount_preserves_private_metadata_and_independent_files() {
    let config: Value = serde_json::from_slice(
        &fs::read(
            std::env::var("AGENT_COMPUTER_CANDIDATE_MOUNT_CONFIG")
                .expect("explicit disposable root-owned VM required"),
        )
        .unwrap(),
    )
    .unwrap();
    let kube = &config["kubernetes"];
    let client = Client::new(
        field(kube, "api_url"),
        &fs::read(field(kube, "ca_file")).unwrap(),
        fs::read_to_string(field(kube, "token_file"))
            .unwrap()
            .trim(),
        serde_json::from_value(kube["deployment"].clone()).unwrap(),
    )
    .unwrap();
    let storage: StorageClassBinding = serde_json::from_value(kube["storage"].clone()).unwrap();
    let image = field(&config, "image");
    let organization = format!(
        "mount_probe_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let declaration = json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"candidate-probe"},"spec":{
        "volumes":[{"name":"data","storageClass":storage.reference,"quotaBytes":4294967296u64,"reclaimPolicy":"Retain"}],
        "sandboxes":[{"name":"sandbox","runtimeClass":"gvisor","image":image,"resources":{"cpuMillis":500,"memoryMiB":256},"networkPolicyRef":"deny-all",
            "mounts":[{"workspaceRef":"id:workspace_probe","path":"/workspace","readOnly":false}]}]}});
    let definition =
        validate_bytes(&serde_json::to_vec(&declaration).unwrap(), Format::Json).unwrap();
    let volume = VolumePlan::new(
        &definition,
        "data",
        VolumeIdentity {
            organization: organization.clone(),
            resource_id: "volume_probe".into(),
            revision: 1,
            step_id: "step_probe".into(),
            spec_digest: format!("sha256:{}", "a".repeat(64)),
        },
        client.namespace(),
        storage,
    )
    .unwrap();
    let created = client.create_volume(&volume).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let (claim, pv) = loop {
        let claim = client
            .observe_volume_claim(&volume, Some(created.uid()))
            .await
            .unwrap()
            .unwrap();
        if let Some(pv) = client
            .observe_bound_volume(&volume, &claim, None)
            .await
            .unwrap()
        {
            break (claim, pv);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "CSI provision timeout"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let local = &config["candidate"];
    let mount_root = Path::new(field(local, "mount_root"));
    let root = mount_root.join(pv.handle());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let uuid = field(&local["target"], "filesystem_uuid");
    let mount =
        MountedVolume::open(mount_root, pv.handle(), uuid, claim.uid(), 1000, 1000).unwrap();
    let objects = Path::new(field(local, "object_cache"));
    let source = ObjectCache::open(objects).unwrap();
    let quota_config: JuiceFsConfig = serde_json::from_value(local["quota"].clone()).unwrap();
    let quota = JuiceFsQuota::new(quota_config).unwrap();
    let mut prepared = Vec::new();
    for name in ["one", "two"] {
        let data = format!("input-{name}");
        let digest = format!("sha256:{:x}", Sha256::digest(data.as_bytes()));
        fs::write(objects.join(&digest[7..]), data).unwrap();
        let mut manifest = Manifest::default();
        manifest.entries.insert(
            "input.txt".into(),
            Entry::File {
                sha256: digest,
                size: 9,
                executable: false,
            },
        );
        let request = PrepareRequest {
            organization: organization.clone(),
            volume_uid: claim.uid().into(),
            workspace: "workspace_probe".into(),
            candidate: format!("candidate_{name}"),
            computer: format!("computer_{name}"),
            generation: 1,
            quota_bytes: 1 << 30,
            manifest_digest: manifest.digest().unwrap(),
            manifest,
        };
        let receipt = mount.prepare(&request, &source, &quota).unwrap();
        prepared.push((request, receipt));
    }
    let mut records = Vec::new();
    for (index, (request, receipt)) in prepared.iter().enumerate() {
        let name = if index == 0 { "one" } else { "two" };
        let path = root.join(&receipt.path_ref);
        let parent = path.parent().unwrap();
        let before = json!({"root":metadata(&root),"parent":metadata(parent),"receipt":metadata(&parent.join("receipt.json")),"data":metadata(&path)});
        let execution_id = format!("exec_{name}");
        let command = format!(
            "test \"$(cat input.txt)\" = input-{name} && test ! -e ../receipt.json && test ! -e /.config && test ! -e /var/run/secrets/kubernetes.io/serviceaccount/token && printf persisted-{name} > saved.txt && sync saved.txt && sync . && printf candidate-{name}-ok"
        );
        let candidate = CandidateMount::new(
            volume.clone(),
            client.namespace_uid(),
            pv.uid(),
            pv.handle(),
            request,
            receipt,
        )
        .unwrap();
        let plan = StartupSandboxPlan::with_candidate(
            &definition,
            "sandbox",
            InstanceIdentity {
                organization: organization.clone(),
                computer: request.computer.clone(),
                sandbox: "sandbox_probe".into(),
                instance: execution_id.clone(),
                generation: 1,
                spec_revision: 1,
            },
            client.namespace(),
            image,
            Bootstrap {
                version: 1,
                hard_budget_ms: None,
                intent_digest: format!("sha256:{}", "b".repeat(64)),
                request: Request {
                    execution_id,
                    generation: 1,
                    argv: vec!["/bin/sh".into(), "-c".into(), command],
                    cwd: String::new(),
                    timeout_seconds: 10,
                    lease_budget_ms: 30000,
                    term_grace_ms: 100,
                    output_limit_bytes: 128,
                },
            },
            candidate,
        )
        .unwrap();
        let pod = client.create(plan.pod_plan()).await.unwrap();
        let result:Result<Value,String>=async {
            let deadline=tokio::time::Instant::now()+Duration::from_secs(90);
            let observed=loop {
                let p=client.observe(plan.pod_plan(),Some(pod.uid())).await.map_err(|e|e.to_string())?.ok_or("pod disappeared")?;
                if p.phase()==PodPhase::Running {break p;}
                if matches!(p.phase(),PodPhase::Failed|PodPhase::Succeeded)||tokio::time::Instant::now()>=deadline{return Err(format!("startup {:?}",p.phase()));}
                tokio::time::sleep(Duration::from_millis(200)).await;
            };
            if index==0 && let Some(marker)=config["observation_file"].as_str() {
                let marker=Path::new(marker);fs::write(marker,serde_json::to_vec(&json!({"namespace":client.namespace(),"name":plan.pod_plan().pod_name(),"uid":pod.uid()})).unwrap()).map_err(|e|e.to_string())?;
                let until=tokio::time::Instant::now()+Duration::from_secs(15);
                while !marker.with_extension("inspected").exists() {if tokio::time::Instant::now()>=until{return Err("node inspection timeout".into());}tokio::time::sleep(Duration::from_millis(100)).await;}
            }
            let channel=client.attach_startup(&plan,&observed).await.map_err(|e|e.to_string())?;
            let grant=StartupGrant {version:1, hard_budget_ms: None,challenge_digest:channel.challenge().digest().unwrap(),lease_budget_ms:15000};
            let observation=channel.run(&grant).await.map_err(|e|e.to_string())?;
            serde_json::from_slice(observation.report_bytes()).map_err(|e|e.to_string())
        }.await;
        let latest = client
            .observe(plan.pod_plan(), Some(pod.uid()))
            .await
            .unwrap()
            .unwrap();
        client.delete(plan.pod_plan(), &latest).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while client
            .observe(plan.pod_plan(), Some(pod.uid()))
            .await
            .unwrap()
            .is_some()
        {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let observed = result.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(observed["report"]["outcome"], "succeeded", "{observed}");
        assert_eq!(
            observed["report"]["stdout"]["bytes"],
            json!(format!("candidate-{name}-ok").into_bytes())
        );
        assert_eq!(
            fs::read(path.join("saved.txt")).unwrap(),
            format!("persisted-{name}").as_bytes()
        );
        assert_eq!(
            before,
            json!({"root":metadata(&root),"parent":metadata(parent),"receipt":metadata(&parent.join("receipt.json")),"data":metadata(&path)})
        );
        assert_eq!(
            mount.observe_prepared(request, &quota).unwrap().as_ref(),
            Some(receipt)
        );
        records.push(json!({"request":request,"receipt":receipt,"pod":plan.pod_plan().manifest(),"pod_uid":pod.uid(),"report":observed,"metadata_before_and_after":before}));
    }
    assert_ne!(
        metadata(&root.join(&prepared[0].1.path_ref).join("saved.txt"))["inode"],
        metadata(&root.join(&prepared[1].1.path_ref).join("saved.txt"))["inode"]
    );
    fs::write(field(&config,"result_file"),serde_json::to_vec_pretty(&json!({"organization":organization,"pvc_name":volume.name(),"pvc_uid":claim.uid(),"pv_uid":pv.uid(),"volume_path":pv.handle(),"filesystem_uuid":uuid,"image":image,"cases":records,"limits":"fixture grants; no DB admission, execution completion or physical fencing"})).unwrap()).unwrap();
    println!(
        "2 real CSI Candidate mounts passed: independent files, fsync and unchanged private metadata; retained Volume remains for inspection"
    );
}
