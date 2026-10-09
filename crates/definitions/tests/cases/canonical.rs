use super::*;

#[test]
fn example_digest_matches_an_independent_python_sha256_vector() {
    let validated = validate_bytes(EXAMPLE, Format::Yaml).unwrap();
    assert_eq!(
        validated.report().definition_digest.as_deref(),
        Some("sha256:3da63e545b10efcd08eba7d050075450049b0837c4bd851078399761d64568ea")
    );
}

#[test]
fn formatting_and_unordered_resource_lists_do_not_change_the_digest() {
    let yaml = validate_bytes(EXAMPLE, Format::Yaml).unwrap();
    let mut value = example();
    value["spec"]["sandboxes"].as_array_mut().unwrap().reverse();
    value["spec"]["computers"][0]["sandboxRefs"]
        .as_array_mut()
        .unwrap()
        .reverse();
    value["spec"]["agents"][0]["capabilities"]
        .as_array_mut()
        .unwrap()
        .reverse();
    let json = check(&value).unwrap();
    assert_eq!(yaml.canonical_bytes(), json.canonical_bytes());
    assert_eq!(
        yaml.report().definition_digest,
        json.report().definition_digest
    );
    let normalized = validate_bytes(yaml.canonical_bytes(), Format::Json).unwrap();
    assert_eq!(yaml.canonical_bytes(), normalized.canonical_bytes());
}

#[test]
fn omitted_defaults_and_explicit_empty_lists_are_equivalent() {
    let mut absent = example();
    absent["spec"].as_object_mut().unwrap().remove("agents");
    let mut empty = absent.clone();
    empty["spec"]["agents"] = json!([]);
    empty["spec"]["sandboxes"][0]["mounts"] = json!([]);
    empty["spec"]["apps"][0]["statePaths"] = json!([]);
    assert_eq!(
        check(&absent).unwrap().report().definition_digest,
        check(&empty).unwrap().report().definition_digest
    );
}

#[test]
fn argv_order_and_revision_preconditions_are_part_of_the_intent() {
    let mut value = example();
    web_app(&mut value);
    let original = check(&value).unwrap().report().definition_digest.clone();
    value["spec"]["apps"][0]["argv"]
        .as_array_mut()
        .unwrap()
        .reverse();
    assert_ne!(original, check(&value).unwrap().report().definition_digest);
    value["spec"]["apps"][0]["argv"]
        .as_array_mut()
        .unwrap()
        .reverse();
    value["spec"]["apps"][0]["expectedRevision"] = json!(1);
    assert_ne!(original, check(&value).unwrap().report().definition_digest);
}

#[test]
fn exported_schema_matches_checked_in_schema_and_rejects_unknown_objects() {
    let expected: Value = serde_json::from_slice(include_bytes!(
        "../../../../schemas/computer-set-v1alpha1.json"
    ))
    .unwrap();
    let current = serde_json::to_value(schema()).unwrap();
    assert_eq!(
        expected, current,
        "regenerate with agent-computer schema computer-set --json"
    );
    assert_eq!(current["additionalProperties"], json!(false));
    for kind in [
        "Metadata",
        "SetSpec",
        "Sandbox",
        "Resources",
        "Mount",
        "App",
        "Health",
        "Agent",
        "Computer",
        "Workspace",
        "Volume",
    ] {
        assert_eq!(
            current["$defs"][kind]["additionalProperties"],
            json!(false),
            "{kind}"
        );
    }
}
