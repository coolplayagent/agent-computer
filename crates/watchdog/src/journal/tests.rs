use super::*;

#[test]
fn publication_does_not_replace_existing_or_partial_records() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = File::open(tmp.path()).unwrap();
    publish(&dir, "report.json", "report.pending", b"first").unwrap();
    assert!(publish(&dir, "report.json", "report.pending", b"second").is_err());
    assert_eq!(
        std::fs::read(tmp.path().join("report.json")).unwrap(),
        b"first"
    );
    assert!(publish(&dir, "other.json", "report.pending", b"third").is_err());
    assert!(!tmp.path().join("other.json").exists());
    assert!(publish(&dir, "big.json", "big.pending", &vec![0; 8193]).is_err());
    assert!(!tmp.path().join("big.pending").exists());
}

#[test]
fn journal_references_are_confined_single_components() {
    for id in ["", ".", "..", "a/b", "/root", "a\\b", "a.b", "a\n"] {
        assert!(valid_id(id).is_err(), "{id}");
    }
    assert!(valid_id(&"a".repeat(65)).is_err());
    assert!(valid_id("journal-abc_12").is_ok());
}

fn fixture() -> (tempfile::TempDir, Journal, Report) {
    let tmp = tempfile::tempdir().unwrap();
    let request = Request {
        version: 1,
        execution_id: "exec".into(),
        boot_id: "12345678-1234-1234-1234-123456789abc".into(),
        cgroup_path: "fixture".into(),
        cgroup_inode: 12,
        deadline_boottime_ms: 1000,
    };
    let reference = Reference {
        id: "journal-fixture".into(),
        device: 4,
        inode: 5,
        intent_digest: format!("sha256:{}", "a".repeat(64)),
    };
    let report = Report {
        version: 1,
        request: request.clone(),
        cgroup_device: 3,
        armed_boottime_ms: 500,
        kill_boottime_ms: 1000,
        observed_boottime_ms: 1001,
        trigger: Trigger::Deadline,
        observation: Observation::EmptyObserved,
        error: None,
        journal: Some(reference.clone()),
    };
    let journal = Journal {
        directory: File::open(tmp.path()).unwrap(),
        reference,
        intent: Intent {
            version: 1,
            request,
            watchdog_pid: 22,
        },
    };
    (tmp, journal, report)
}

#[test]
fn report_identity_time_and_error_must_agree() {
    let (_tmp, journal, report) = fixture();
    journal.validate_report(&report).unwrap();
    let original = serde_json::to_value(&report).unwrap();
    for (pointer, value) in [
        ("/version", serde_json::json!(2)),
        ("/request/execution_id", serde_json::json!("other")),
        ("/request/deadline_boottime_ms", serde_json::json!(2000)),
        ("/journal/inode", serde_json::json!(99)),
        ("/cgroup_device", serde_json::json!(0)),
        ("/armed_boottime_ms", serde_json::json!(1002)),
        ("/kill_boottime_ms", serde_json::json!(999)),
        ("/observed_boottime_ms", serde_json::json!(999)),
        ("/error", serde_json::json!("KillFailed")),
        ("/observation", serde_json::json!("Unknown")),
    ] {
        let mut bad = original.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            journal
                .validate_report(&serde_json::from_value(bad).unwrap())
                .is_err(),
            "{pointer}"
        );
    }
    let mut unknown = report;
    unknown.observation = Observation::Unknown;
    unknown.error = Some(Error::KillFailed);
    journal.validate_report(&unknown).unwrap();
}
