use crate::model::*;
use crate::{Diagnostic, ExternalReference, ResourceKind};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_RESOURCES: usize = 1024;
pub const MAX_DIAGNOSTICS: usize = 100;

pub(crate) struct Validation {
    pub diagnostics: Vec<Diagnostic>,
    pub truncated: bool,
    pub external: BTreeSet<ExternalReference>,
}

impl Validation {
    fn error(&mut self, path: &str, code: &str, message: &str) {
        if self.diagnostics.len() == MAX_DIAGNOSTICS {
            self.truncated = true;
        } else {
            self.diagnostics.push(Diagnostic::new(path, code, message));
        }
    }

    fn check_name(&mut self, value: &str, path: &str) {
        if !valid_name(value) {
            self.error(path, "invalid_name", "Use 1–63 lowercase ASCII letters, digits or hyphens, starting and ending with a letter or digit.");
        }
    }

    fn revision(&mut self, value: Option<u64>, path: &str) {
        if value == Some(0) || value.is_some_and(|n| n > i64::MAX as u64) {
            self.error(
                path,
                "invalid_revision",
                "Expected revisions must be positive signed 64-bit integers; omit for creation.",
            );
        }
    }

    fn names<'a>(
        &mut self,
        resources: impl Iterator<Item = (&'a str, Option<u64>)>,
        path: &str,
    ) -> BTreeSet<&'a str> {
        let mut names = BTreeSet::new();
        for (i, (name, revision)) in resources.enumerate() {
            self.check_name(name, &format!("{path}/{i}/name"));
            self.revision(revision, &format!("{path}/{i}/expectedRevision"));
            if !names.insert(name) {
                self.error(
                    &format!("{path}/{i}/name"),
                    "duplicate_name",
                    "Names must be unique within a resource kind.",
                );
            }
        }
        names
    }

    fn reference(
        &mut self,
        value: &str,
        kind: ResourceKind,
        names: Option<&BTreeSet<&str>>,
        path: &str,
    ) {
        if let Some(id) = value.strip_prefix("id:") {
            if id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            {
                self.error(path, "invalid_reference", "Resource IDs use id: followed by 1–128 ASCII letters, digits, underscores or hyphens.");
                return;
            }
        } else {
            if !valid_name(value) {
                self.error(path, "invalid_reference", "Use a local name or an explicit id: reference; organization-qualified references are not accepted.");
                return;
            }
            if let Some(names) = names {
                if !names.contains(value) {
                    self.error(
                        path,
                        "missing_reference",
                        "The referenced local resource does not exist in this document.",
                    );
                }
                return;
            }
        }
        self.external.insert(ExternalReference {
            kind,
            reference: value.into(),
            path: path.into(),
        });
    }

    fn references(
        &mut self,
        values: &[String],
        kind: ResourceKind,
        names: &BTreeSet<&str>,
        path: &str,
    ) {
        let mut seen = BTreeSet::new();
        for (i, value) in values.iter().enumerate() {
            let at = format!("{path}/{i}");
            self.reference(value, kind, Some(names), &at);
            if !seen.insert(value) {
                self.error(
                    &at,
                    "duplicate_reference",
                    "A resource reference may only appear once in this list.",
                );
            }
        }
    }

    fn paths(&mut self, values: &[String], path: &str) {
        let mut seen = BTreeSet::new();
        for (i, value) in values.iter().enumerate() {
            let at = format!("{path}/{i}");
            if !valid_sandbox_path(value) {
                self.error(
                    &at,
                    "invalid_path",
                    "Use a normalized absolute path below /workspace, /inputs, /app or /data.",
                );
            }
            if !seen.insert(value) {
                self.error(&at, "duplicate_path", "Paths must be unique.");
            }
        }
    }
}

pub(crate) fn valid_name(value: &str) -> bool {
    let valid_end = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !value.is_empty()
        && value.len() <= 63
        && valid_end(value.as_bytes()[0])
        && valid_end(value.as_bytes()[value.len() - 1])
        && value.bytes().all(|b| valid_end(b) || b == b'-')
}

pub(crate) fn valid_sandbox_path(value: &str) -> bool {
    let root = value.split('/').nth(1).unwrap_or("");
    matches!(root, "workspace" | "inputs" | "app" | "data") && normalized_absolute_path(value)
}

fn normalized_absolute_path(value: &str) -> bool {
    value.starts_with('/')
        && value.len() <= 4096
        && value.bytes().all(|b| b >= 32 && b != 127 && b != b'\\')
        && value
            .split('/')
            .skip(1)
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

fn pinned_image(value: &str) -> bool {
    let Some((_, digest)) = value.split_once("@sha256:") else {
        return false;
    };
    value.len() <= 1024
        && digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && oci_spec::distribution::Reference::try_from(value).is_ok()
}

fn overlap(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).is_some_and(|s| s.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|s| s.starts_with('/'))
}

pub(crate) fn validate(set: &ComputerSet) -> Validation {
    let mut v = Validation {
        diagnostics: vec![],
        truncated: false,
        external: BTreeSet::new(),
    };
    v.check_name(&set.metadata.name, "/metadata/name");
    v.revision(set.metadata.expected_revision, "/metadata/expectedRevision");
    let s = &set.spec;
    let count = s.volumes.len()
        + s.workspaces.len()
        + s.sandboxes.len()
        + s.apps.len()
        + s.agents.len()
        + s.computers.len();
    if count == 0 || count > MAX_RESOURCES {
        v.error(
            "/spec",
            "resource_limit",
            "A document must contain between 1 and 1024 resource definitions.",
        );
        return v;
    }
    let volumes = v.names(
        s.volumes
            .iter()
            .map(|r| (r.name.as_str(), r.expected_revision)),
        "/spec/volumes",
    );
    let workspaces = v.names(
        s.workspaces
            .iter()
            .map(|r| (r.name.as_str(), r.expected_revision)),
        "/spec/workspaces",
    );
    let sandboxes = v.names(
        s.sandboxes
            .iter()
            .map(|r| (r.name.as_str(), r.expected_revision)),
        "/spec/sandboxes",
    );
    let apps = v.names(
        s.apps
            .iter()
            .map(|r| (r.name.as_str(), r.expected_revision)),
        "/spec/apps",
    );
    v.names(
        s.agents
            .iter()
            .map(|r| (r.name.as_str(), r.expected_revision)),
        "/spec/agents",
    );
    v.names(
        s.computers
            .iter()
            .map(|r| (r.name.as_str(), r.expected_revision)),
        "/spec/computers",
    );
    for (i, r) in s.volumes.iter().enumerate() {
        let p = format!("/spec/volumes/{i}");
        v.reference(
            &r.storage_class,
            ResourceKind::StorageClass,
            None,
            &format!("{p}/storageClass"),
        );
        if r.quota_bytes == 0 || r.quota_bytes > i64::MAX as u64 {
            v.error(
                &format!("{p}/quotaBytes"),
                "invalid_quota",
                "A positive signed 64-bit quota is required.",
            );
        }
    }
    for (i, r) in s.workspaces.iter().enumerate() {
        v.reference(
            &r.volume_ref,
            ResourceKind::Volume,
            Some(&volumes),
            &format!("/spec/workspaces/{i}/volumeRef"),
        );
    }
    for (i, r) in s.sandboxes.iter().enumerate() {
        let p = format!("/spec/sandboxes/{i}");
        if r.mounts.len() > 128 {
            v.error(
                &format!("{p}/mounts"),
                "mount_limit",
                "At most 128 mounts are allowed per Sandbox.",
            );
            continue;
        }
        if r.runtime_class != "gvisor" {
            v.error(&format!("{p}/runtimeClass"), "unsupported_runtime", "This schema recognizes gvisor; runtime compatibility still requires deployment certification.");
        }
        if !pinned_image(&r.image) {
            v.error(
                &format!("{p}/image"),
                "unpinned_image",
                "An OCI repository reference pinned to a lowercase sha256 digest is required.",
            );
        }
        if r.resources.cpu_millis == 0 || r.resources.memory_mi_b == 0 {
            v.error(
                &format!("{p}/resources"),
                "invalid_resources",
                "CPU and memory reservations must both be positive.",
            );
        }
        v.reference(
            &r.network_policy_ref,
            ResourceKind::NetworkPolicy,
            None,
            &format!("{p}/networkPolicyRef"),
        );
        for (j, m) in r.mounts.iter().enumerate() {
            let at = format!("{p}/mounts/{j}");
            v.reference(
                &m.workspace_ref,
                ResourceKind::Workspace,
                Some(&workspaces),
                &format!("{at}/workspaceRef"),
            );
            if !valid_sandbox_path(&m.path) {
                v.error(
                    &format!("{at}/path"),
                    "invalid_path",
                    "Use a normalized path below /workspace, /inputs, /app or /data.",
                );
            }
            if r.mounts[..j]
                .iter()
                .any(|other| overlap(&other.path, &m.path))
            {
                v.error(
                    &format!("{at}/path"),
                    "mount_overlap",
                    "Mount destinations must not overlap.",
                );
            }
        }
    }
    for (i, r) in s.apps.iter().enumerate() {
        validate_app(&mut v, r, i, &sandboxes);
    }
    for (i, r) in s.agents.iter().enumerate() {
        let p = format!("/spec/agents/{i}");
        match (r.mode, &r.sandbox_ref) {
            (AgentMode::Hosted, Some(target)) => v.reference(target, ResourceKind::Sandbox, Some(&sandboxes), &format!("{p}/sandboxRef")),
            (AgentMode::External, None) => (),
            _ => v.error(&format!("{p}/sandboxRef"), "agent_location", "Hosted agents require a Sandbox; external agents must not bind their model runtime to a Sandbox."),
        }
        let mut secrets = BTreeSet::new();
        for (j, secret) in r.secret_refs.iter().enumerate() {
            let at = format!("{p}/secretRefs/{j}");
            v.reference(secret, ResourceKind::Secret, None, &at);
            if !secrets.insert(secret) {
                v.error(
                    &at,
                    "duplicate_reference",
                    "Secret references must be unique.",
                );
            }
        }
        if r.capabilities.iter().collect::<BTreeSet<_>>().len() != r.capabilities.len() {
            v.error(
                &format!("{p}/capabilities"),
                "duplicate_capability",
                "Capabilities must be unique; they declare requirements and do not grant access.",
            );
        }
    }
    let app_by_name: BTreeMap<_, _> = s.apps.iter().map(|app| (app.name.as_str(), app)).collect();
    for (i, r) in s.computers.iter().enumerate() {
        let p = format!("/spec/computers/{i}");
        v.reference(
            &r.workspace_ref,
            ResourceKind::Workspace,
            Some(&workspaces),
            &format!("{p}/workspaceRef"),
        );
        v.references(
            &r.sandbox_refs,
            ResourceKind::Sandbox,
            &sandboxes,
            &format!("{p}/sandboxRefs"),
        );
        v.references(
            &r.app_refs,
            ResourceKind::App,
            &apps,
            &format!("{p}/appRefs"),
        );
        for (j, app) in r.app_refs.iter().enumerate() {
            if let Some(app) = app_by_name.get(app.as_str())
                && !r.sandbox_refs.contains(&app.sandbox_ref)
            {
                v.error(
                    &format!("{p}/appRefs/{j}"),
                    "app_sandbox_missing",
                    "The Computer must include each local App's Sandbox.",
                );
            }
        }
        for sandbox in s
            .sandboxes
            .iter()
            .filter(|sandbox| r.sandbox_refs.contains(&sandbox.name))
        {
            if sandbox
                .mounts
                .iter()
                .any(|m| !m.read_only && m.workspace_ref != r.workspace_ref)
            {
                v.error(
                    &format!("{p}/sandboxRefs"),
                    "foreign_write_mount",
                    "Writable mounts must use this Computer's primary Workspace.",
                );
            }
        }
    }
    v
}

fn validate_app(v: &mut Validation, app: &App, i: usize, sandboxes: &BTreeSet<&str>) {
    let p = format!("/spec/apps/{i}");
    v.reference(
        &app.sandbox_ref,
        ResourceKind::Sandbox,
        Some(sandboxes),
        &format!("{p}/sandboxRef"),
    );
    match app.driver {
        AppDriver::ChromiumPlaywright => {
            if let Some(profile) = &app.profile_ref {
                v.reference(
                    profile,
                    ResourceKind::BrowserProfile,
                    None,
                    &format!("{p}/profileRef"),
                );
            } else {
                v.error(
                    &format!("{p}/profileRef"),
                    "profile_required",
                    "Browser Apps require an explicitly authorized profile reference.",
                );
            }
            if !app.argv.is_empty()
                || app.cwd.is_some()
                || app.health.is_some()
                || !app.state_paths.is_empty()
                || !app.export_paths.is_empty()
            {
                v.error(&p, "driver_field_mismatch", "Browser launch, health and private profile state are controlled by the Browser Driver.");
            }
        }
        AppDriver::WebApplication => {
            if app.profile_ref.is_some() {
                v.error(
                    &format!("{p}/profileRef"),
                    "driver_field_mismatch",
                    "WebApplication does not accept a browser profile.",
                );
            }
            if app.argv.is_empty()
                || app.argv.len() > 128
                || app.argv[0].is_empty()
                || app
                    .argv
                    .iter()
                    .any(|arg| arg.len() > 4096 || arg.contains('\0'))
            {
                v.error(&format!("{p}/argv"), "invalid_argv", "Provide 1–128 arguments with a nonempty program; arguments are at most 4096 bytes and contain no NUL.");
            }
            if !app.cwd.as_deref().is_some_and(valid_sandbox_path) {
                v.error(&format!("{p}/cwd"), "invalid_path", "WebApplication requires a working directory below /workspace, /inputs, /app or /data.");
            }
            match &app.health {
                Some(h) if h.port > 0 && h.startup_timeout_seconds > 0 && h.startup_timeout_seconds <= 3600
                    && (h.path == "/" || normalized_absolute_path(&h.path)) && !h.path.contains(['?', '#', '%']) => (),
                _ => v.error(&format!("{p}/health"), "invalid_health", "Provide a nonzero private port, normalized HTTP path and 1–3600 second startup timeout."),
            }
        }
    }
    v.paths(&app.state_paths, &format!("{p}/statePaths"));
    v.paths(&app.export_paths, &format!("{p}/exportPaths"));
}
