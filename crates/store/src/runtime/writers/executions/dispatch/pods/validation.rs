use super::*;
use sha2::{Digest, Sha256};

/// Identity checks only. The Kubernetes adapter owns strict manifest/security
/// validation and live deployment/storage checks; this journal does not replace it.
pub(super) fn identity(
    dispatch: &ExecutionDispatchIntent,
    namespace_uid: &str,
    manifest: &Value,
) -> Result<(String, String)> {
    valid_id(namespace_uid)?;
    if serde_json::to_vec(manifest)
        .map_err(|_| Error::InvalidRuntimeRequest)?
        .len()
        > 262_144
    {
        return Err(Error::InvalidRuntimeRequest);
    }
    let namespace = manifest["metadata"]["namespace"]
        .as_str()
        .ok_or(Error::InvalidRuntimeRequest)?;
    let end = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if namespace.is_empty()
        || namespace.len() > 63
        || !end(namespace.as_bytes()[0])
        || !end(namespace.as_bytes()[namespace.len() - 1])
        || !namespace.bytes().all(|b| end(b) || b == b'-')
    {
        return Err(Error::InvalidRuntimeRequest);
    }
    let execution = &dispatch.execution;
    let key = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(&dispatch.organization, &execution.execution_id))
                .map_err(|_| Error::InvalidStoredData)?
        )
    );
    let name = format!("ac-{}", &key[..52]);
    let binding: Value = serde_json::from_str(
        manifest["metadata"]["annotations"]["agent-computer.io/binding"]
            .as_str()
            .ok_or(Error::InvalidRuntimeRequest)?,
    )
    .map_err(|_| Error::InvalidRuntimeRequest)?;
    let bootstrap =
        serde_json::to_value(dispatch.bootstrap()?).map_err(|_| Error::InvalidStoredData)?;
    let command = manifest["spec"]["containers"][0]["command"]
        .as_array()
        .ok_or(Error::InvalidRuntimeRequest)?;
    let parsed_bootstrap: Value = serde_json::from_str(
        command
            .get(2)
            .and_then(Value::as_str)
            .ok_or(Error::InvalidRuntimeRequest)?,
    )
    .map_err(|_| Error::InvalidRuntimeRequest)?;
    let workspace = &binding["workspace"];
    let target = &dispatch.binding["storage_target"];
    if manifest["kind"] != "Pod"
        || manifest["apiVersion"] != "v1"
        || manifest["metadata"]["name"] != name
        || manifest["metadata"].get("uid").is_some()
        || manifest["metadata"].get("resourceVersion").is_some()
        || binding["version"] != 2
        || binding["namespace"] != namespace
        || binding["identity"]
            != json!({"organization":dispatch.organization,"computer":execution.computer_id,
            "sandbox":execution.sandbox_id,"instance":execution.execution_id,"generation":execution.generation,"spec_revision":execution.sandbox_revision})
        || binding["bootstrap"] != bootstrap
        || parsed_bootstrap != bootstrap
        || command.len() != 3
        || command[0] != "/bin/agent-computer-sandbox"
        || command[1] != "--attach-startup-json"
        || workspace["kind"] != "prepared_candidate"
        || workspace["version"] != 1
        || workspace["organization"] != dispatch.organization
        || workspace["computer"] != execution.computer_id
        || workspace["candidate"] != execution.candidate_id
        || workspace["generation"] != execution.generation
        || workspace["namespace_uid"] != namespace_uid
        || target["namespace_uid"] != namespace_uid
        || workspace["pv_uid"] != target["pv_uid"]
        || workspace["volume_path"] != target["volume_path"]
        || workspace["prepared"] != dispatch.binding["prepared"]
    {
        return Err(Error::InvalidRuntimeRequest);
    }
    Ok((namespace.into(), name))
}
