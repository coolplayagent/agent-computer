use super::{Failure, Result};
use agent_computer_core::identity::{ComputerId, IdempotencyKey};
use reqwest::Method;
use std::collections::BTreeMap;

pub(super) struct Options {
    pub method: Method,
    pub path: String,
    pub endpoint: String,
    pub token_file: String,
    pub input: Option<String>,
    pub key: Option<String>,
    pub output: Option<String>,
}

pub(super) fn parse(args: &[&str]) -> Result<Options> {
    let usage = || {
        Failure::local(
            "usage",
            "Invalid remote command or options; run agent-computer --help.",
        )
    };
    let mut words = Vec::new();
    let mut flags = BTreeMap::new();
    let mut args = args.iter();
    while let Some(&arg) = args.next() {
        if arg == "--json" {
            if flags.insert(arg, "").is_some() {
                return Err(usage());
            }
        } else if matches!(
            arg,
            "--endpoint"
                | "--token-file"
                | "--request"
                | "--idempotency-key"
                | "--stream"
                | "--output"
        ) {
            let value = *args.next().ok_or_else(usage)?;
            if value.is_empty() || flags.insert(arg, value).is_some() {
                return Err(usage());
            }
        } else if arg.starts_with('-') {
            return Err(usage());
        } else {
            words.push(arg);
        }
    }
    let (method, path) = match words.as_slice() {
        ["doctor"] => (Method::GET, "/v1alpha1/capabilities".to_owned()),
        ["computer", action, id] => {
            let (method, suffix) = match *action {
                "show" => (Method::GET, "runtime"),
                "start" => (Method::POST, "start"),
                "cancel-start" => (Method::POST, "start/cancel"),
                "stop" => (Method::POST, "stop"),
                "checkpoint-stop" => (Method::POST, "checkpoint-stop"),
                _ => return Err(usage()),
            };
            (method, route("computers", id, suffix)?)
        }
        ["connect", id] => (Method::POST, route("computers", id, "connection-sessions")?),
        ["disconnect", id] => (Method::DELETE, route("connection-sessions", id, "")?),
        ["connection", "show", id] => (Method::GET, route("connection-sessions", id, "")?),
        ["connection", "heartbeat", id] => {
            (Method::POST, route("connection-sessions", id, "heartbeat")?)
        }
        ["lease", action, id] => match *action {
            "acquire" => (Method::POST, route("computers", id, "leases")?),
            "show" => (Method::GET, route("leases", id, "")?),
            "renew" | "release" => (Method::POST, route("leases", id, action)?),
            _ => return Err(usage()),
        },
        ["exec", id] => (Method::POST, route("computers", id, "executions")?),
        ["status", id] => (Method::GET, route("executions", id, "")?),
        ["cancel", id] => (Method::POST, route("executions", id, "cancel")?),
        ["logs", id] => {
            let suffix = match flags.get("--stream") {
                Some(&"stdout") => "output/stdout",
                Some(&"stderr") => "output/stderr",
                None => "output",
                _ => return Err(usage()),
            };
            if flags.contains_key("--stream") != flags.contains_key("--output") {
                return Err(usage());
            }
            (Method::GET, route("executions", id, suffix)?)
        }
        _ => return Err(usage()),
    };
    if words[0] != "logs" && (flags.contains_key("--stream") || flags.contains_key("--output")) {
        return Err(usage());
    }
    if flags.get("--output") == Some(&"-") {
        return Err(usage());
    }
    let input = flags.get("--request").map(|v| v.to_string());
    let key = flags.get("--idempotency-key").map(|v| v.to_string());
    if method == Method::POST {
        if input.is_none()
            || key
                .as_deref()
                .is_none_or(|k| IdempotencyKey::new(k).is_err())
        {
            return Err(usage());
        }
    } else if input.is_some() || key.is_some() {
        return Err(usage());
    }
    let setting = |flag, variable| {
        flags.get(flag).map(|s| s.to_string())
        .or_else(|| std::env::var(variable).ok()).filter(|s| !s.is_empty())
        .ok_or_else(|| Failure::local("configuration_required", "Supply an endpoint and token file using options or AGENT_COMPUTER_ENDPOINT and AGENT_COMPUTER_TOKEN_FILE."))
    };
    Ok(Options {
        method,
        path,
        input,
        key,
        endpoint: setting("--endpoint", "AGENT_COMPUTER_ENDPOINT")?,
        token_file: setting("--token-file", "AGENT_COMPUTER_TOKEN_FILE")?,
        output: flags.get("--output").map(|v| v.to_string()),
    })
}

fn route(collection: &str, id: &str, suffix: &str) -> Result<String> {
    ComputerId::new(id).map_err(|_| {
        Failure::local(
            "invalid_identifier",
            "Use an opaque resource identifier, not a URL or path.",
        )
    })?;
    Ok(format!(
        "/v1alpha1/{collection}/{id}{}",
        if suffix.is_empty() {
            String::new()
        } else {
            format!("/{suffix}")
        }
    ))
}
