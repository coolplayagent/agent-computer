//! Validate the bounded wire representation. Pixels and DOM remain untrusted
//! observations; matching digests do not authenticate the page or caller.
use crate::{Error, Result, Viewport, identifier, session::exact};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub(crate) fn validate(value: &Value, viewport: &Viewport) -> Result<()> {
    exact(
        value,
        &[
            "observation_id",
            "scope",
            "page_id",
            "document_generation",
            "control_epoch",
            "captured_at",
            "valid_for_ms",
            "viewport",
            "image_size",
            "device_scale",
            "coordinate_space",
            "image_scale",
            "screenshot_sha256",
            "screenshot_base64",
            "elements",
            "dom_truncated",
        ],
    )?;
    let expected = serde_json::to_value(viewport).map_err(|_| Error::Protocol)?;
    if value["viewport"] != expected
        || value["image_size"] != expected
        || value["device_scale"] != 1
        || value["image_scale"] != 1
        || value["coordinate_space"] != "css_viewport"
        || !value["dom_truncated"].is_boolean()
        || value["valid_for_ms"].as_u64().is_none_or(|v| v > 30000)
        || value["captured_at"]
            .as_str()
            .is_none_or(|s| s.len() != 24 || !s.is_ascii())
    {
        return Err(Error::Protocol);
    }
    let encoded = value["screenshot_base64"].as_str().ok_or(Error::Protocol)?;
    if encoded.len() > 2_796_204 {
        return Err(Error::Limit);
    }
    let png = STANDARD.decode(encoded).map_err(|_| Error::Protocol)?;
    // Same PNG header contract as the driver. No decompression of untrusted pixels.
    if png.len() < 24
        || png.len() > 2 * 1024 * 1024
        || png[..8] != [137, 80, 78, 71, 13, 10, 26, 10]
        || png[8..16] != [0, 0, 0, 13, 73, 72, 68, 82]
        || png[16..20] != viewport.width.to_be_bytes()
        || png[20..24] != viewport.height.to_be_bytes()
        || value["screenshot_sha256"] != format!("sha256:{:x}", Sha256::digest(&png))
    {
        return Err(Error::Protocol);
    }
    let elements = value["elements"].as_array().ok_or(Error::Protocol)?;
    if elements.len() > 100 {
        return Err(Error::Limit);
    }
    let mut ids = BTreeSet::new();
    let mut bytes = 0;
    for element in elements {
        exact(
            element,
            &["element_id", "tag", "role", "label", "text", "bounds"],
        )?;
        let id = element["element_id"].as_str().ok_or(Error::Protocol)?;
        let tag = element["tag"].as_str().ok_or(Error::Protocol)?;
        if !identifier(id)
            || !ids.insert(id)
            || tag.is_empty()
            || tag.len() > 64
            || !tag.as_bytes()[0].is_ascii_lowercase()
            || !tag
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(Error::Protocol);
        }
        for (key, limit) in [("role", 64), ("label", 512), ("text", 512)] {
            if element[key]
                .as_str()
                .is_none_or(|s| s.encode_utf16().count() > limit)
            {
                return Err(Error::Protocol);
            }
        }
        let bounds = &element["bounds"];
        exact(bounds, &["x", "y", "width", "height"])?;
        let number = |key: &str| {
            bounds[key]
                .as_f64()
                .filter(|v| v.is_finite())
                .ok_or(Error::Protocol)
        };
        let (x, y, w, h) = (
            number("x")?,
            number("y")?,
            number("width")?,
            number("height")?,
        );
        if w <= 0.0
            || h <= 0.0
            || x >= f64::from(viewport.width)
            || y >= f64::from(viewport.height)
            || x + w <= 0.0
            || y + h <= 0.0
            || !(x + w).is_finite()
            || !(y + h).is_finite()
        {
            return Err(Error::Protocol);
        }
        let mut entry = element.clone();
        entry.as_object_mut().unwrap().remove("element_id");
        bytes += serde_json::to_vec(&entry)
            .map_err(|_| Error::Protocol)?
            .len();
        if bytes > 32768 {
            return Err(Error::Limit);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> (Value, Viewport) {
        let viewport = Viewport {
            width: 800,
            height: 600,
        };
        let mut bytes = vec![137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82];
        bytes.extend_from_slice(&800u32.to_be_bytes());
        bytes.extend_from_slice(&600u32.to_be_bytes());
        let value = json!({"observation_id":"observation","scope":{},"page_id":"page","document_generation":1,"control_epoch":1,"captured_at":"2026-10-11T00:00:00.000Z","valid_for_ms":1000,"viewport":viewport,"image_size":viewport,"device_scale":1,"coordinate_space":"css_viewport","image_scale":1,"screenshot_sha256":format!("sha256:{:x}",Sha256::digest(&bytes)),"screenshot_base64":STANDARD.encode(bytes),"elements":[{"element_id":"element","tag":"button","role":"button","label":"test","text":"ok","bounds":{"x":1,"y":2,"width":3,"height":4}}],"dom_truncated":false});
        (value, viewport)
    }
    #[test]
    fn bounded_observation_rejects_corrupt_pixels_coordinates_and_dom() {
        let (value, viewport) = fixture();
        validate(&value, &viewport).unwrap();
        for (pointer, changed) in [
            (
                "/screenshot_sha256",
                json!(format!("sha256:{}", "0".repeat(64))),
            ),
            ("/screenshot_base64", json!("invalid")),
            ("/image_size/width", json!(801)),
            ("/device_scale", json!(2)),
            ("/valid_for_ms", json!(30001)),
            ("/elements/0/bounds/width", json!(0)),
            ("/elements/0/bounds/x", json!(800)),
            ("/elements/0/label", json!("x".repeat(513))),
            ("/elements/0/tag", json!("<script>")),
        ] {
            let mut input = value.clone();
            *input.pointer_mut(pointer).unwrap() = changed;
            assert!(validate(&input, &viewport).is_err(), "{pointer}");
        }
        let mut input = value.clone();
        input["elements"]
            .as_array_mut()
            .unwrap()
            .push(value["elements"][0].clone());
        assert!(validate(&input, &viewport).is_err());
        input = value;
        input["script"] = json!("x");
        assert!(validate(&input, &viewport).is_err());
    }
}
