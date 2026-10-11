//! Export a service-authorized observation without decoding page-controlled pixels.
use super::{Failure, Result, output};
use agent_computer_core::identity::ComputerId;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn save(path: &str, request_id: &str, mut value: Value) -> Result<Value> {
    let bad = || {
        Failure::local(
            "invalid_observation",
            "Expected a current, complete observation with matching request, scope, epoch and PNG metadata; no file was published.",
        )
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bad())?
        .as_millis();
    let frame = &value["result"];
    let id = |v: &Value| v.as_str().is_some_and(|s| ComputerId::new(s).is_ok());
    let positive = |v: &Value| v.as_u64().is_some_and(|n| n > 0 && n < (1 << 53));
    let dimension = |name: &str, min, max| {
        frame["image_size"][name]
            .as_u64()
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(bad)
    };
    let width = dimension("width", 320, 1920)? as u32;
    let height = dimension("height", 240, 1080)? as u32;
    if value["request_id"] != request_id
        || value["state"] != "Complete"
        || !id(&value["instance_id"])
        || frame["scope"]["app_instance_id"] != value["instance_id"]
        || !positive(&value["control_epoch"])
        || frame["control_epoch"] != value["control_epoch"]
        || !id(&frame["observation_id"])
        || !id(&frame["page_id"])
        || !positive(&frame["document_generation"])
        || value["result_valid_until_ms"]
            .as_u64()
            .is_none_or(|n| u128::from(n) <= now || u128::from(n) > now + 30_000)
        || frame["valid_for_ms"]
            .as_u64()
            .is_none_or(|n| n == 0 || n > 30_000)
        || frame["viewport"] != frame["image_size"]
        || frame["device_scale"] != 1
        || frame["image_scale"] != 1
        || frame["coordinate_space"] != "css_viewport"
    {
        return Err(bad());
    }
    let encoded = frame["screenshot_base64"]
        .as_str()
        .filter(|s| s.len() <= 2_796_204)
        .ok_or_else(bad)?;
    let png = STANDARD.decode(encoded).map_err(|_| bad())?;
    let digest = format!("sha256:{:x}", Sha256::digest(&png));
    if !(24..=2 * 1024 * 1024).contains(&png.len())
        || png[..16] != [137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82]
        || png[16..20] != width.to_be_bytes()
        || png[20..24] != height.to_be_bytes()
        || frame["screenshot_sha256"] != digest
    {
        return Err(bad());
    }
    output::publish(path, &png)?;
    // Keep the observation identity/geometry available for an explicit later action.
    value["result"]
        .as_object_mut()
        .ok_or_else(bad)?
        .remove("screenshot_base64");
    value["screenshot_file"] = json!({"path":path,"bytes":png.len(),"sha256":digest});
    Ok(value)
}
