use super::*;

fn fixture() -> (Probe, Receipt) {
    let request = Request {
        version: 1,
        execution_id: "e".into(),
        boot_id: "12345678-1234-1234-1234-123456789abc".into(),
        cgroup_path: "fixture".into(),
        cgroup_inode: 1,
        deadline_boottime_ms: 2000,
    };
    let first = Reference {
        id: "journal-first".into(),
        device: 7,
        inode: 1,
        intent_digest: format!("sha256:{}", "a".repeat(64)),
    };
    let second = Reference {
        id: "journal-second".into(),
        inode: 2,
        ..first.clone()
    };
    let query = Probe {
        version: 1,
        nonce: "a".repeat(64),
        request,
        journals: [first, second],
        cgroup_device: 42,
    };
    let reply = Receipt {
        version: 1,
        instance: "b".repeat(64),
        nonce: query.nonce.clone(),
        request: query.request.clone(),
        journals: query.journals.clone(),
        cgroup_device: 42,
        pid: 100,
        spool_device: 7,
        spool_inode: 8,
        observed_boottime_ms: 1000,
    };
    (query, reply)
}

#[test]
fn probe_rejects_replayed_foreign_and_stale_replies() {
    let (query, reply) = fixture();
    validate(&query, &reply, 990, 1010, 7, 8).unwrap();
    let original = serde_json::to_value(&reply).unwrap();
    for (pointer, value) in [
        ("/nonce", serde_json::json!("c".repeat(64))),
        ("/instance", serde_json::json!("invalid")),
        ("/request/execution_id", serde_json::json!("other")),
        ("/request/deadline_boottime_ms", serde_json::json!(2001)),
        ("/journals/0/inode", serde_json::json!(3)),
        ("/cgroup_device", serde_json::json!(43)),
        ("/pid", serde_json::json!(0)),
        ("/spool_device", serde_json::json!(9)),
        ("/spool_inode", serde_json::json!(9)),
        ("/observed_boottime_ms", serde_json::json!(989)),
        ("/observed_boottime_ms", serde_json::json!(1011)),
    ] {
        let mut bad = original.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            validate(
                &query,
                &serde_json::from_value(bad).unwrap(),
                990,
                1010,
                7,
                8
            )
            .is_err(),
            "{pointer}"
        );
    }
    assert!(validate(&query, &reply, 990, 1191, 7, 8).is_err());
    assert!(validate(&query, &reply, 990, 2000, 7, 8).is_err());
}

#[test]
fn admission_frames_are_bounded_and_challenges_are_full_length_hex() {
    let (mut query, _) = fixture();
    assert!(frame(&query).is_ok());
    query.nonce = "a".repeat(MAX_FRAME);
    assert!(frame(&query).is_err());
    for value in ["", "deadbeef", &"A".repeat(64), &"a".repeat(65)] {
        assert!(!hex(value));
    }
    assert!(hex(&random().unwrap()));
}
