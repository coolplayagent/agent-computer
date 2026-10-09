use super::*;

#[test]
fn duplicate_names_are_rejected_within_each_kind() {
    for kind in [
        "volumes",
        "workspaces",
        "sandboxes",
        "apps",
        "agents",
        "computers",
    ] {
        let mut value = example();
        let first = value["spec"][kind][0].clone();
        value["spec"][kind].as_array_mut().unwrap().push(first);
        invalid(&value, "duplicate_name");
    }
}

#[test]
fn names_in_different_kinds_are_independent() {
    let mut value = example();
    value["spec"]["workspaces"][0]["name"] = json!("work");
    value["spec"]["computers"][0]["workspaceRef"] = json!("work");
    check(&value).unwrap();
}

#[test]
fn local_references_have_typed_targets_and_missing_names_fail() {
    for pointer in [
        "/spec/workspaces/0/volumeRef",
        "/spec/apps/0/sandboxRef",
        "/spec/computers/0/workspaceRef",
        "/spec/computers/0/sandboxRefs/0",
        "/spec/computers/0/appRefs/0",
    ] {
        let mut value = example();
        *value.pointer_mut(pointer).unwrap() = json!("absent");
        let report = invalid(&value, "missing_reference");
        assert!(report.diagnostics.iter().any(|d| d.path == pointer));
    }
}

#[test]
fn explicit_ids_are_deferred_for_authorized_resolution_and_cannot_select_a_tenant() {
    let mut value = example();
    value["spec"]["computers"][0]["workspaceRef"] = json!("id:ws_existing");
    let validated = check(&value).unwrap();
    assert!(
        validated
            .report()
            .external_references
            .iter()
            .any(|r| r.kind == ResourceKind::Workspace && r.reference == "id:ws_existing")
    );
    assert!(
        validated
            .report()
            .external_references
            .iter()
            .any(|r| r.kind == ResourceKind::BrowserProfile)
    );
    for target in [
        "id:",
        "id:other-org/ws_1",
        "other-org/work",
        "id:ws_1?token=secret",
    ] {
        value["spec"]["computers"][0]["workspaceRef"] = json!(target);
        invalid(&value, "invalid_reference");
    }
}

#[test]
fn app_sandbox_must_be_attached_and_references_must_be_unique() {
    let mut value = example();
    value["spec"]["computers"][0]["sandboxRefs"] = json!(["exec-env"]);
    invalid(&value, "app_sandbox_missing");
    value["spec"]["computers"][0]["sandboxRefs"] = json!(["browser-env", "browser-env"]);
    invalid(&value, "duplicate_reference");
    value["spec"]["computers"][0]["sandboxRefs"] = json!(["browser-env"]);
    value["spec"]["computers"][0]["appRefs"] = json!(["browser", "browser"]);
    invalid(&value, "duplicate_reference");
}

#[test]
fn names_and_expected_revisions_are_bounded_without_leaking_input() {
    for name in [
        "",
        "-bad",
        "bad-",
        "Upper",
        "a/b",
        "name secret",
        &"a".repeat(64),
    ] {
        let mut value = example();
        value["metadata"]["name"] = json!(name);
        let report = invalid(&value, "invalid_name");
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("name secret")
        );
    }
    for revision in [0, u64::MAX] {
        let mut value = example();
        value["spec"]["sandboxes"][0]["expectedRevision"] = json!(revision);
        invalid(&value, "invalid_revision");
    }
    let mut value = example();
    value["metadata"]["expectedRevision"] = json!(3);
    check(&value).unwrap();
}

#[test]
fn hosted_and_external_agents_have_distinct_runtime_locations() {
    let mut value = example();
    value["spec"]["agents"][0]["sandboxRef"] = json!("exec-env");
    invalid(&value, "agent_location");
    value["spec"]["agents"][0]["mode"] = json!("hosted");
    check(&value).unwrap();
    value["spec"]["agents"][0]
        .as_object_mut()
        .unwrap()
        .remove("sandboxRef");
    invalid(&value, "agent_location");
}

#[test]
fn capabilities_and_secret_references_do_not_accept_unknown_or_duplicate_values() {
    let mut value = example();
    value["spec"]["agents"][0]["capabilities"] = json!(["browser.act", "browser.act"]);
    invalid(&value, "duplicate_capability");
    value["spec"]["agents"][0]["capabilities"] = json!(["cluster.admin"]);
    invalid(&value, "schema_violation");
    value["spec"]["agents"][0]["capabilities"] = json!([]);
    value["spec"]["agents"][0]["secretRefs"] = json!(["api-key", "api-key"]);
    invalid(&value, "duplicate_reference");
    value["spec"]["agents"][0]["secretRefs"] = json!(["id:secret_1"]);
    let validated = check(&value).unwrap();
    assert!(
        validated
            .report()
            .external_references
            .iter()
            .any(|r| r.kind == ResourceKind::Secret)
    );
}

#[test]
fn each_layer_can_be_declared_without_creating_a_computer() {
    let mut value = example();
    value["spec"] = json!({"volumes":value["spec"]["volumes"]});
    assert_eq!(check(&value).unwrap().report().resource_count, 1);
}
