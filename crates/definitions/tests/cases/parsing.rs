use super::*;

#[test]
fn documented_example_passes_without_an_agent_or_workflow() {
    let original = validate_bytes(EXAMPLE, Format::Yaml).unwrap();
    assert_eq!(original.report().resource_count, 7);
    assert_eq!(original.report().scope, "static");
    let mut human = example();
    human["spec"].as_object_mut().unwrap().remove("agents");
    let validated = check(&human).unwrap();
    assert_eq!(validated.report().resource_count, 6);
    assert!(validated.document().spec.agents.is_empty());
}

#[test]
fn unknown_and_server_owned_fields_fail_closed_at_every_level() {
    for (pointer, field) in [
        ("", "status"),
        ("/metadata", "organizationId"),
        ("/spec", "unknown"),
        ("/spec/sandboxes/0", "podUid"),
        ("/spec/sandboxes/0/resources", "privileged"),
        ("/spec/agents/0", "secrets"),
        ("/spec/computers/0", "principal"),
    ] {
        let mut value = example();
        value.pointer_mut(pointer).unwrap()[field] = json!("must-not-appear-in-diagnostic");
        let report = invalid(&value, "schema_violation");
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("must-not-appear")
        );
    }
}

#[test]
fn unknown_versions_drivers_modes_and_destructive_defaults_are_rejected() {
    for (pointer, replacement) in [
        ("/apiVersion", "agent-computer/v99"),
        ("/kind", "Pod"),
        ("/spec/apps/0/driver", "desktop"),
        ("/spec/agents/0/adapter", "unknown-harness"),
        ("/spec/agents/0/mode", "local"),
        ("/spec/volumes/0/reclaimPolicy", "Delete"),
        ("/spec/workspaces/0/conflictPolicy", "last-write-wins"),
        ("/spec/computers/0/desiredState", "Deleted"),
    ] {
        let mut value = example();
        *value.pointer_mut(pointer).unwrap() = json!(replacement);
        invalid(&value, "schema_violation");
    }
}

#[test]
fn duplicate_keys_are_rejected_before_json_or_yaml_can_overwrite_them() {
    for (input, format) in [
        (
            br#"{"metadata":{"name":"first","name":"second"}}"#.as_slice(),
            Format::Json,
        ),
        (
            b"metadata:\n  name: first\n  name: second\n".as_slice(),
            Format::Yaml,
        ),
    ] {
        let report = validate_bytes(input, format).unwrap_err();
        assert_eq!(report.diagnostics[0].code, "invalid_document");
        assert!(report.diagnostics[0].line.is_some());
    }
}

#[test]
fn multiple_documents_tags_and_recursive_aliases_do_not_get_ignored() {
    for input in [
        b"---\n{}\n---\n{}\n".as_slice(),
        b"!untrusted {}",
        b"&loop [*loop]",
    ] {
        assert_eq!(
            validate_bytes(input, Format::Yaml).unwrap_err().diagnostics[0].code,
            "invalid_document"
        );
    }
    assert_eq!(
        validate_bytes(b"{} {}", Format::Json)
            .unwrap_err()
            .diagnostics[0]
            .code,
        "invalid_document"
    );
}

#[test]
fn document_depth_node_count_and_yaml_alias_expansion_are_bounded() {
    let too_large = vec![b' '; MAX_DOCUMENT_BYTES + 1];
    assert_eq!(
        validate_bytes(&too_large, Format::Yaml)
            .unwrap_err()
            .diagnostics[0]
            .code,
        "document_too_large"
    );
    let nested = format!("{}0{}", "[".repeat(40), "]".repeat(40));
    assert_eq!(
        validate_bytes(nested.as_bytes(), Format::Json)
            .unwrap_err()
            .diagnostics[0]
            .code,
        "invalid_document"
    );
    let nodes = format!("[{}0]", "0,".repeat(65_536));
    assert_eq!(
        validate_bytes(nodes.as_bytes(), Format::Json)
            .unwrap_err()
            .diagnostics[0]
            .code,
        "invalid_document"
    );
    let aliases = format!(
        "first: &payload {}\nrepeated: [{}]\n",
        "s".repeat(65_536),
        vec!["*payload"; 20].join(",")
    );
    assert!(aliases.len() < MAX_DOCUMENT_BYTES);
    assert_eq!(
        validate_bytes(aliases.as_bytes(), Format::Yaml)
            .unwrap_err()
            .diagnostics[0]
            .code,
        "invalid_document"
    );
}

#[test]
fn required_types_and_integer_ranges_are_not_coerced() {
    for replacement in [json!("4096"), json!(-1), json!(1.5), Value::Null] {
        let mut value = example();
        value["spec"]["sandboxes"][0]["resources"]["memoryMiB"] = replacement;
        invalid(&value, "schema_violation");
    }
    let mut value = example();
    value["spec"]["apps"][0]
        .as_object_mut()
        .unwrap()
        .remove("sandboxRef");
    invalid(&value, "schema_violation");
}

#[test]
fn diagnostics_and_total_resources_are_bounded() {
    let mut value = example();
    value["spec"] = json!({});
    invalid(&value, "resource_limit");
    let volume =
        json!({"name":"bad name","storageClass":"juicefs","quotaBytes":0,"reclaimPolicy":"Retain"});
    value["spec"]["volumes"] = json!(vec![volume.clone(); 200]);
    let report = invalid(&value, "invalid_name");
    assert_eq!(report.diagnostics.len(), 100);
    assert!(report.diagnostics_truncated);
    value["spec"]["volumes"] = json!(vec![volume; 1025]);
    invalid(&value, "resource_limit");
}
