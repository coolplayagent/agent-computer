use super::*;
use agent_computer_storage::files::{FileEdit, FileVersion};
use agent_computer_store::runtime::{artifacts::*, connections::*, writers::*};
use agent_computer_worker::files::{SaveRequest, save_once};

pub struct Context<'a> {
    pub store: &'a Store,
    pub pool: &'a sqlx::PgPool,
    pub token: &'a str,
    pub org: &'a OrganizationId,
    pub computers: [&'a str; 2],
    pub workspace: &'a str,
    pub worker: &'a Value,
    pub config: &'a Value,
    pub root: &'a std::path::Path,
    pub owner: &'a WorkerId,
}
struct Run {
    start: StartReceipt,
    prepared: Prepared,
    session: ConnectionSession,
}
impl Context<'_> {
    async fn head(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT revision FROM workspace_input_heads WHERE organization=$1 AND workspace_id=$2",
        )
        .bind(self.org.as_str())
        .bind(self.workspace)
        .fetch_one(self.pool)
        .await
        .unwrap()
    }
    async fn start(
        &self,
        index: usize,
        revision: i64,
        artifact: Option<&str>,
        key_name: &str,
    ) -> Run {
        let start = self
            .store
            .admit_computer_start(
                self.token,
                &key(key_name),
                self.computers[index],
                &StartRequest {
                    expected_revision: revision,
                    expected_spec_revision: 1,
                    max_runtime_seconds: 300,
                    input_artifact_id: artifact.map(str::to_owned),
                },
            )
            .await
            .unwrap();
        let mut config = self.worker.clone();
        config["artifacts"] = self.config["artifacts"].clone();
        assert!(matches!(
            prepare_once(
                self.store,
                self.org,
                &start.request_id,
                self.owner,
                serde_json::from_value(config).unwrap()
            )
            .await
            .unwrap(),
            WorkResult::Prepared
        ));
        let prepared:Prepared=serde_json::from_value(sqlx::query_scalar::<_,Value>("SELECT receipt FROM candidate_preparations WHERE organization=$1 AND request_id=$2").bind(self.org.as_str()).bind(&start.request_id).fetch_one(self.pool).await.unwrap()).unwrap();
        let session = self
            .store
            .create_connection_session(
                self.token,
                &key(&format!("{key_name}-connect")),
                self.computers[index],
                &ConnectRequest {
                    requested_capabilities: vec![
                        RuntimePermission::Connect,
                        RuntimePermission::Read,
                        RuntimePermission::Modify,
                    ],
                    lifetime_seconds: 900,
                },
            )
            .await
            .unwrap();
        Run {
            start,
            prepared,
            session,
        }
    }
    async fn acquire(&self, run: &Run, name: &str) -> WriterLease {
        self.store
            .acquire_candidate_writer(
                self.token,
                &key(name),
                &run.start.computer_id,
                &AcquireWriterLease {
                    scope: WriterScope::Modify,
                    connection_session_id: run.session.session_id.clone(),
                    generation: run.start.generation,
                    candidate_id: run.start.candidate_id.clone(),
                    duration_seconds: 30,
                },
            )
            .await
            .unwrap()
    }
    async fn save(
        &self,
        lease: &WriterLease,
        name: &str,
        bytes: &[u8],
        expected: Option<FileVersion>,
    ) -> FileVersion {
        let done = save_once(
            self.store,
            self.token,
            &lease.lease_id,
            SaveRequest {
                lease: WriterLeaseCommand {
                    connection_session_id: lease.connection_session_id.clone(),
                    generation: lease.generation,
                    epoch: lease.epoch,
                    expected_revision: lease.revision,
                },
                dispatch_id: name.into(),
                edit: FileEdit {
                    path: "shared.txt".into(),
                    expected,
                    content: bytes.into(),
                    executable: false,
                },
            },
            serde_json::from_value(
                json!({"target":self.worker["target"],"mount_root":self.worker["mount_root"]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(done.state, WriterLeaseState::Released);
        done.file_edit.unwrap().version.unwrap()
    }
    async fn seal(&self, run: &Run, name: &str, publish_current: bool) -> ArtifactCommit {
        let current = self
            .store
            .computer_runtime(self.token, &run.start.computer_id)
            .await
            .unwrap();
        self.store
            .commit_workspace_artifact(
                self.token,
                &key(name),
                self.workspace,
                &CommitArtifact {
                    request_id: run.start.request_id.clone(),
                    expected_revision: current.revision,
                    base_revision: run.start.input_revision.unwrap(),
                    base_manifest: run.start.input_manifest_digest.clone().unwrap(),
                    publish_current,
                },
            )
            .await
            .unwrap()
    }
    async fn publish(&self, commit: &ArtifactCommit) -> ArtifactCommit {
        agent_computer_worker::artifacts::publish_once(self.store,self.org,&commit.commit_id,self.owner,serde_json::from_value(json!({
            "storage":{"target":self.worker["target"],"mount_root":self.worker["mount_root"]},"objects":self.config["artifacts"],"spool":self.config["artifact_spool"],
        })).unwrap()).await.unwrap().unwrap()
    }
    async fn stop(&self, run: &Run, name: &str) -> ComputerStopReceipt {
        self.store
            .close_connection_session(self.token, &run.session.session_id)
            .await
            .unwrap();
        let current = self
            .store
            .computer_runtime(self.token, &run.start.computer_id)
            .await
            .unwrap();
        self.store
            .stop_prepared_computer(
                self.token,
                &key(name),
                &run.start.computer_id,
                &StopPreparedComputer {
                    expected_revision: current.revision,
                    request_id: run.start.request_id.clone(),
                },
            )
            .await
            .unwrap()
    }
    fn file(&self, run: &Run) -> PathBuf {
        self.root.join(&run.prepared.path_ref).join("shared.txt")
    }
}

pub async fn verify(c: Context<'_>) -> Value {
    let (one, two) = tokio::join!(
        c.start(0, 1, None, "parallel-one"),
        c.start(1, 1, None, "parallel-two")
    );
    assert_ne!(one.start.candidate_id, two.start.candidate_id);
    assert_ne!(one.prepared.path_ref, two.prepared.path_ref);
    let (lease_one, lease_two) = tokio::join!(
        c.acquire(&one, "parallel-one-writer"),
        c.acquire(&two, "parallel-two-writer")
    );
    assert_ne!(lease_one.lease_id, lease_two.lease_id);
    // The same Workspace never lets a connection borrow another Candidate.
    assert!(
        c.store
            .acquire_candidate_writer(
                c.token,
                &key("cross-candidate"),
                &one.start.computer_id,
                &AcquireWriterLease {
                    scope: WriterScope::Modify,
                    connection_session_id: one.session.session_id.clone(),
                    generation: one.start.generation,
                    candidate_id: two.start.candidate_id.clone(),
                    duration_seconds: 30,
                }
            )
            .await
            .is_err()
    );
    let (version_one, version_two) = tokio::join!(
        c.save(&lease_one, "parallel-one-save", b"first author\n", None),
        c.save(&lease_two, "parallel-two-save", b"second author\n", None)
    );
    assert_eq!(fs::read(c.file(&one)).unwrap(), b"first author\n");
    assert_eq!(fs::read(c.file(&two)).unwrap(), b"second author\n");
    assert_ne!(
        c.file(&one).metadata().unwrap().ino(),
        c.file(&two).metadata().unwrap().ino()
    );
    let first = c.seal(&one, "parallel-first-artifact", true).await;
    let second = c.seal(&two, "parallel-second-artifact", true).await;
    let first = c.publish(&first).await;
    let second = c.publish(&second).await;
    assert_eq!(first.state, ArtifactState::Committed);
    assert_eq!(second.state, ArtifactState::Conflict);
    assert_eq!(c.head().await, 2);
    let stopped_one = c.stop(&one, "parallel-first-stop").await;
    let stopped_two = c.stop(&two, "parallel-conflict-stop").await;
    assert_eq!(
        stopped_two.checkpoint.as_ref().unwrap().artifact_id,
        second.commit_id
    );
    assert_eq!(stopped_two.input_revision, 3);
    let (default, selected) = tokio::join!(
        c.start(
            0,
            stopped_one.control_revision,
            None,
            "parallel-default-restart"
        ),
        c.start(
            1,
            stopped_two.control_revision,
            Some(&second.commit_id),
            "parallel-conflict-restart"
        )
    );
    assert_eq!(
        default.start.input_artifact_id,
        Some(first.commit_id.clone())
    );
    assert_eq!(
        selected.start.input_artifact_id,
        Some(second.commit_id.clone())
    );
    assert_eq!(fs::read(c.file(&default)).unwrap(), b"first author\n");
    assert_eq!(fs::read(c.file(&selected)).unwrap(), b"second author\n");
    assert_ne!(
        c.file(&selected).metadata().unwrap().ino(),
        c.file(&two).metadata().unwrap().ino()
    );
    assert_eq!(c.head().await, 2);
    let next_lease = c.acquire(&selected, "parallel-continued-writer").await;
    c.save(
        &next_lease,
        "parallel-continued-save",
        b"second author continued\n",
        Some(version_two),
    )
    .await;
    assert_eq!(fs::read(c.file(&two)).unwrap(), b"second author\n");
    assert_eq!(fs::read(c.file(&default)).unwrap(), b"first author\n");
    let branch = c.seal(&selected, "parallel-branch-artifact", false).await;
    let branch = c.publish(&branch).await;
    assert_eq!(branch.state, ArtifactState::Committed);
    assert_eq!(branch.input_revision, Some(4));
    assert_eq!(c.head().await, 2);
    let stopped_branch = c.stop(&selected, "parallel-branch-stop").await;
    let branch_again = c
        .start(
            1,
            stopped_branch.control_revision,
            Some(&branch.commit_id),
            "parallel-branch-restart",
        )
        .await;
    assert_eq!(branch_again.start.generation, 3);
    assert_eq!(
        fs::read(c.file(&branch_again)).unwrap(),
        b"second author continued\n"
    );
    assert_ne!(
        c.file(&branch_again).metadata().unwrap().ino(),
        c.file(&selected).metadata().unwrap().ino()
    );
    assert_eq!(c.head().await, 2);
    // Explicit user merge on a Candidate based on the current head can publish.
    // Merely selecting a conflicting Artifact never overrides its CAS baseline.
    let merge_lease = c.acquire(&default, "parallel-merge-writer").await;
    c.save(
        &merge_lease,
        "parallel-merge-save",
        b"first author\nsecond author continued\n",
        Some(version_one),
    )
    .await;
    let merged = c.seal(&default, "parallel-merge-artifact", true).await;
    let merged = c.publish(&merged).await;
    assert_eq!(merged.state, ArtifactState::Committed);
    assert_eq!(merged.input_revision, Some(5));
    assert_eq!(c.head().await, 5);
    let merged_stop = c.stop(&default, "parallel-merged-stop").await;
    assert_eq!(
        fs::read(c.file(&branch_again)).unwrap(),
        b"second author continued\n"
    );
    json!({"same_workspace":c.workspace,"first":first,"conflict":second,"branch":branch,"explicit_merge":merged,
        "conflict_checkpoint":stopped_two,"branch_checkpoint":stopped_branch,"merged_checkpoint":merged_stop,
        "default_restart":default.start,"selected_restart":selected.start,"continued_restart":branch_again.start,
        "prepared_candidates":[one.prepared,two.prepared,default.prepared,selected.prepared,branch_again.prepared],
        "simultaneous_independent_writer_leases":true,"cross_candidate_authority_rejected":true,"preserved_independent_content_and_inodes":true,
        "selection_and_branch_did_not_move_head":true,"explicit_merge_from_current_base_published":true})
}
