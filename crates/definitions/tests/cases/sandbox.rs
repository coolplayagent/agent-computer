use super::*;

#[test]
fn floating_or_malformed_images_and_runtime_fallbacks_are_rejected() {
    for image in [
        &format!("registry/Uppercase@sha256:{}", "a".repeat(64)),
        &format!("registry///repo@sha256:{}", "a".repeat(64)),
        &format!("registry:bad:port/repo@sha256:{}", "a".repeat(64)),
        "registry/browser:latest",
        "registry/browser",
        "registry/browser@sha256:abc",
        &format!("https://registry/browser@sha256:{}", "a".repeat(64)),
        &format!("registry/browser@sha256:{}", "A".repeat(64)),
    ] {
        let mut value = example();
        value["spec"]["sandboxes"][0]["image"] = json!(image);
        invalid(&value, "unpinned_image");
    }
    let mut value = example();
    value["spec"]["sandboxes"][0]["runtimeClass"] = json!("runc");
    invalid(&value, "unsupported_runtime");
    value["spec"]["sandboxes"][0]["runtimeClass"] = json!("gvisor");
    value["spec"]["sandboxes"][0]["image"] = json!(format!(
        "registry:5000/repo/browser:v1@sha256:{}",
        "a".repeat(64)
    ));
    check(&value).unwrap();
}

#[test]
fn zero_resource_limits_and_out_of_range_quotas_fail() {
    for pointer in [
        "/spec/sandboxes/0/resources/cpuMillis",
        "/spec/sandboxes/0/resources/memoryMiB",
    ] {
        let mut value = example();
        *value.pointer_mut(pointer).unwrap() = json!(0);
        invalid(&value, "invalid_resources");
    }
    for quota in [0, u64::MAX] {
        let mut value = example();
        value["spec"]["volumes"][0]["quotaBytes"] = json!(quota);
        invalid(&value, "invalid_quota");
    }
}

#[test]
fn mount_paths_cannot_escape_or_overlap() {
    for path in [
        "/",
        "/etc",
        "/workspace/../etc",
        "/workspace//nested",
        "/workspace/./x",
        "/workspace\\x",
        "/workspace/x/",
        "relative",
        "/workspace/\0",
    ] {
        let mut value = example();
        value["spec"]["sandboxes"][0]["mounts"] =
            json!([{"workspaceRef":"research-work", "path":path,"readOnly":true}]);
        invalid(&value, "invalid_path");
    }
    let mut value = example();
    value["spec"]["sandboxes"][0]["mounts"] = json!([
        {"workspaceRef":"research-work","path":"/workspace","readOnly":true},
        {"workspaceRef":"research-work","path":"/workspace/nested","readOnly":false}
    ]);
    invalid(&value, "mount_overlap");
}

#[test]
fn writable_mounts_use_the_computers_primary_workspace() {
    let mut value = example();
    value["spec"]["workspaces"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"other","volumeRef":"work","conflictPolicy":"explicit"}));
    value["spec"]["sandboxes"][0]["mounts"] =
        json!([{"workspaceRef":"other","path":"/workspace","readOnly":false}]);
    invalid(&value, "foreign_write_mount");
    value["spec"]["sandboxes"][0]["mounts"][0]["workspaceRef"] = json!("research-work");
    check(&value).unwrap();
}

#[test]
fn browser_launch_and_profile_state_cannot_be_overridden() {
    let mut value = example();
    value["spec"]["apps"][0]["argv"] = json!(["chromium", "--no-sandbox"]);
    invalid(&value, "driver_field_mismatch");
    value["spec"]["apps"][0]
        .as_object_mut()
        .unwrap()
        .remove("argv");
    value["spec"]["apps"][0]
        .as_object_mut()
        .unwrap()
        .remove("profileRef");
    invalid(&value, "profile_required");
}

#[test]
fn web_application_has_explicit_launch_health_and_state_contracts() {
    let mut value = example();
    web_app(&mut value);
    check(&value).unwrap();
    for (pointer, replacement, code) in [
        ("/spec/apps/0/argv", json!([]), "invalid_argv"),
        ("/spec/apps/0/argv", json!([""]), "invalid_argv"),
        (
            "/spec/apps/0/argv",
            json!(["node", "bad\0arg"]),
            "invalid_argv",
        ),
        ("/spec/apps/0/cwd", json!("/app/../root"), "invalid_path"),
        ("/spec/apps/0/health/port", json!(0), "invalid_health"),
        (
            "/spec/apps/0/health/path",
            json!("//metadata/secret"),
            "invalid_health",
        ),
        (
            "/spec/apps/0/health/path",
            json!("/%2e%2e/secret"),
            "invalid_health",
        ),
        (
            "/spec/apps/0/health/startupTimeoutSeconds",
            json!(0),
            "invalid_health",
        ),
        ("/spec/apps/0/statePaths", json!(["/etc"]), "invalid_path"),
        (
            "/spec/apps/0/exportPaths",
            json!(["/data/a", "/data/a"]),
            "duplicate_path",
        ),
    ] {
        let mut bad = value.clone();
        *bad.pointer_mut(pointer).unwrap() = replacement;
        invalid(&bad, code);
    }
    value["spec"]["apps"][0]["profileRef"] = json!("private-profile");
    invalid(&value, "driver_field_mismatch");
}
