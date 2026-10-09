//! Side-effect-free ComputerSet parsing, validation, canonicalization and schema.
#![forbid(unsafe_code)]

mod bounded;
mod canonical;
pub mod model;
mod validation;

use agent_computer_core::API_VERSION;
use serde::Serialize;
use sha2::{Digest, Sha256};

pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
pub const CANONICALIZATION: &str = "agent-computer/definition-v1";

#[derive(Clone, Debug, Serialize)]
pub struct Diagnostic {
    pub path: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
}

impl Diagnostic {
    pub(crate) fn new(path: &str, code: &str, message: &str) -> Self {
        Self {
            path: path.into(),
            code: code.into(),
            message: message.into(),
            line: None,
            column: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Volume,
    Workspace,
    Sandbox,
    App,
    StorageClass,
    NetworkPolicy,
    BrowserProfile,
    Secret,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct ExternalReference {
    pub kind: ResourceKind,
    pub reference: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidationReport {
    pub api_version: &'static str,
    pub scope: &'static str,
    pub valid: bool,
    pub definition_digest: Option<String>,
    pub canonicalization: &'static str,
    pub resource_count: usize,
    pub diagnostics: Vec<Diagnostic>,
    pub diagnostics_truncated: bool,
    /// Each reference needs existence, organization, authorization and compatibility checks in plan/apply.
    pub external_references: Vec<ExternalReference>,
}

impl ValidationReport {
    fn empty() -> Self {
        Self {
            api_version: API_VERSION,
            scope: "static",
            valid: false,
            definition_digest: None,
            canonicalization: CANONICALIZATION,
            resource_count: 0,
            diagnostics: vec![],
            diagnostics_truncated: false,
            external_references: vec![],
        }
    }
    pub fn failure(code: &str, message: &str) -> Self {
        let mut report = Self::empty();
        report.diagnostics.push(Diagnostic::new("/", code, message));
        report
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Format {
    Json,
    Yaml,
}

/// Only the validator can create this type. It conveys static validity, never authority.
#[derive(Clone, Debug)]
pub struct ValidatedDefinition {
    document: model::ComputerSet,
    canonical_bytes: Vec<u8>,
    report: ValidationReport,
}

impl ValidatedDefinition {
    pub fn document(&self) -> &model::ComputerSet {
        &self.document
    }
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }
    pub fn report(&self) -> &ValidationReport {
        &self.report
    }
}

pub fn validate_bytes(
    input: &[u8],
    format: Format,
) -> Result<ValidatedDefinition, Box<ValidationReport>> {
    if input.len() > MAX_DOCUMENT_BYTES {
        return Err(Box::new(ValidationReport::failure(
            "document_too_large",
            "Declarations are limited to 1 MiB.",
        )));
    }
    let decoded = match format {
        Format::Json => serde_json::from_slice::<bounded::Document>(input).map_err(|error| {
            let mut report = ValidationReport::failure(
                "invalid_document",
                "Invalid, duplicate-key or over-complex JSON document.",
            );
            report.diagnostics[0].line = Some(error.line());
            report.diagnostics[0].column = Some(error.column());
            Box::new(report)
        }),
        Format::Yaml => serde_yaml_ng::from_slice::<bounded::Document>(input).map_err(|error| {
            let mut report = ValidationReport::failure(
                "invalid_document",
                "Invalid, duplicate-key, tagged or over-complex YAML document.",
            );
            if let Some(at) = error.location() {
                report.diagnostics[0].line = Some(at.line());
                report.diagnostics[0].column = Some(at.column());
            }
            Box::new(report)
        }),
    }?;
    let mut document: model::ComputerSet = serde_json::from_value(decoded.0).map_err(|_| {
        // Serde diagnostics may include supplied secrets or arbitrary unknown field names.
        Box::new(ValidationReport::failure("schema_violation", "Document fields or types do not match ComputerSet v1alpha1. Unknown fields, server-owned fields and unknown capabilities are rejected; inspect the exported schema."))
    })?;
    let checked = validation::validate(&document);
    let mut report = ValidationReport::empty();
    let s = &document.spec;
    report.resource_count = s.volumes.len()
        + s.workspaces.len()
        + s.sandboxes.len()
        + s.apps.len()
        + s.agents.len()
        + s.computers.len();
    report.diagnostics = checked.diagnostics;
    report.diagnostics_truncated = checked.truncated;
    report.external_references = checked.external.into_iter().collect();
    if !report.diagnostics.is_empty() {
        return Err(Box::new(report));
    }
    canonical::normalize(&mut document);
    // serde_json's default map is a BTreeMap; all keys and unordered resource lists are sorted.
    let value = serde_json::to_value(&document).map_err(|_| {
        Box::new(ValidationReport::failure(
            "encoding_failed",
            "Unable to encode the validated declaration.",
        ))
    })?;
    let canonical_bytes = serde_json::to_vec(&value).map_err(|_| {
        Box::new(ValidationReport::failure(
            "encoding_failed",
            "Unable to encode the validated declaration.",
        ))
    })?;
    let mut hash = Sha256::new();
    hash.update(CANONICALIZATION.as_bytes());
    hash.update([0]);
    hash.update(&canonical_bytes);
    report.valid = true;
    report.definition_digest = Some(format!("sha256:{:x}", hash.finalize()));
    Ok(ValidatedDefinition {
        document,
        canonical_bytes,
        report,
    })
}

pub fn schema() -> schemars::Schema {
    schemars::schema_for!(model::ComputerSet)
}
