//! v1alpha1 declaration DTOs. Unknown fields fail closed at every object boundary.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ComputerSet {
    pub api_version: ApiVersion,
    pub kind: DocumentKind,
    pub metadata: Metadata,
    pub spec: SetSpec,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
pub enum ApiVersion {
    #[serde(rename = "agent-computer/v1alpha1")]
    V1Alpha1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
pub enum DocumentKind {
    ComputerSet,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Metadata {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SetSpec {
    #[serde(default)]
    pub volumes: Vec<Volume>,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub sandboxes: Vec<Sandbox>,
    #[serde(default)]
    pub apps: Vec<App>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub computers: Vec<Computer>,
}

macro_rules! resource {
    ($name:ident { $($(#[$attr:meta])* $field:ident: $typ:ty),* $(,)? }) => {
        #[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        pub struct $name {
            pub name: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub expected_revision: Option<u64>,
            $($(#[$attr])* pub $field: $typ,)*
        }
    };
}

resource!(Volume {
    storage_class: String,
    quota_bytes: u64,
    reclaim_policy: ReclaimPolicy,
});

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
pub enum ReclaimPolicy {
    Retain,
}

resource!(Workspace {
    volume_ref: String,
    conflict_policy: ConflictPolicy,
});

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
pub enum ConflictPolicy {
    #[serde(rename = "explicit")]
    Explicit,
}

resource!(Sandbox {
    runtime_class: String,
    image: String,
    resources: Resources,
    network_policy_ref: String,
    #[serde(default)]
    mounts: Vec<Mount>,
});

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Resources {
    pub cpu_millis: u32,
    pub memory_mi_b: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Mount {
    pub workspace_ref: String,
    pub path: String,
    pub read_only: bool,
}

resource!(App {
    driver: AppDriver,
    sandbox_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile_ref: Option<String>,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    health: Option<Health>,
    #[serde(default)]
    state_paths: Vec<String>,
    #[serde(default)]
    export_paths: Vec<String>,
});

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
pub enum AppDriver {
    #[serde(rename = "chromium-playwright")]
    ChromiumPlaywright,
    #[serde(rename = "web-application")]
    WebApplication,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Health {
    pub port: u16,
    pub path: String,
    pub startup_timeout_seconds: u32,
}

resource!(Agent {
    mode: AgentMode,
    adapter: AgentAdapter,
    #[serde(default)]
    capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox_ref: Option<String>,
    #[serde(default)]
    secret_refs: Vec<String>,
});

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    External,
    Hosted,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
pub enum AgentAdapter {
    #[serde(rename = "tools-api")]
    ToolsApi,
}

#[derive(
    Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Deserialize, Serialize, JsonSchema,
)]
pub enum Capability {
    #[serde(rename = "browser.observe")]
    BrowserObserve,
    #[serde(rename = "browser.act")]
    BrowserAct,
    #[serde(rename = "files.read")]
    FilesRead,
    #[serde(rename = "files.write")]
    FilesWrite,
    #[serde(rename = "execution.submit")]
    ExecutionSubmit,
    #[serde(rename = "artifacts.read")]
    ArtifactsRead,
    #[serde(rename = "artifacts.commit")]
    ArtifactsCommit,
}

resource!(Computer {
    workspace_ref: String,
    #[serde(default)]
    sandbox_refs: Vec<String>,
    #[serde(default)]
    app_refs: Vec<String>,
    desired_state: DesiredState,
});

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
pub enum DesiredState {
    Running,
    Stopped,
}
