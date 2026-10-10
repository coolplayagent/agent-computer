use crate::{
    ActionIntent, ActionReceipt, AuthorizationRequest, Binding, Callback, Decision, Error,
    INPUT_FRAME_BYTES, OUTPUT_FRAME_BYTES, Operation, Outcome, Permission, Result, contracts,
    identifier,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, future::Future, time::Duration};
use tokio::time::{Instant, timeout_at};

/// Private duplex transport. Implementations must not reconnect or retry writes.
pub trait Transport: Send {
    fn write<'a>(
        &'a mut self,
        bytes: &'a [u8],
        deadline: Instant,
    ) -> impl Future<Output = Result<()>> + Send + 'a;
    fn read(&mut self, deadline: Instant) -> impl Future<Output = Result<Vec<u8>>> + Send;
}
/// Mandatory trusted policy. Check live node guard, original admitted identity,
/// current authority and durable journal before answering. Never derive caller
/// authentication from the callback's strings. Returning an error closes usage.
pub trait Policy: Send {
    fn answer<'a>(
        &'a mut self,
        callback: &'a Callback,
    ) -> impl Future<Output = Result<Decision>> + Send + 'a;
}

/// Bounded, correlated worker output, not physical termination, a checkpoint,
/// database admission or permission to release any resource reservation.
pub struct ResponseObservation {
    ok: bool,
    value: Value,
}
impl ResponseObservation {
    pub fn ok(&self) -> bool {
        self.ok
    }
    pub fn value(&self) -> &Value {
        &self.value
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ActionResult {
    receipt: ActionReceipt,
    recorded: bool,
}
struct Cached {
    operation: Operation,
    result: ActionResult,
}
#[derive(Default)]
struct Flow {
    authorization_calls: usize,
    grants: usize,
    epoch: Option<u64>,
    epoch_changed: bool,
    begin_seen: bool,
    intent: Option<ActionIntent>,
    dispatch: bool,
    reauthorized: bool,
    replay: Option<ActionReceipt>,
    complete: Option<ActionReceipt>,
    recorded: bool,
}

pub struct Session<T> {
    transport: T,
    binding: Binding,
    session: String,
    request: u32,
    callback: u32,
    launched: bool,
    closed: bool,
    blocked: bool,
    until: Instant,
    buffer: Vec<u8>,
    read_bytes: usize,
    written_bytes: usize,
    frames: usize,
    actions: BTreeMap<String, Cached>,
}
impl<T: Transport> Session<T> {
    pub async fn connect(transport: T, binding: Binding) -> Result<Self> {
        binding.validate()?;
        let until = Instant::now() + Duration::from_millis(binding.runtime_ms.into());
        let mut session = Self {
            transport,
            binding,
            session: String::new(),
            request: 0,
            callback: 0,
            launched: false,
            closed: false,
            blocked: false,
            until,
            buffer: Vec::new(),
            read_bytes: 0,
            written_bytes: 0,
            frames: 0,
            actions: BTreeMap::new(),
        };
        let deadline = until.min(Instant::now() + Duration::from_secs(15));
        timeout_at(deadline, async move {
            let digest = session.binding.digest()?;
            session
                .send(
                    json!({"version":1,"kind":"hello","binding_digest":digest}),
                    deadline,
                )
                .await?;
            let value = session.receive(deadline).await?;
            exact(&value, &["version", "kind", "session_id", "binding_digest"])?;
            let id = value["session_id"].as_str().ok_or(Error::Protocol)?;
            if value["version"] != 1
                || value["kind"] != "challenge"
                || value["binding_digest"] != digest
                || id.len() != 32
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(Error::Protocol);
            }
            session.session = id.into();
            Ok(session)
        })
        .await
        .map_err(|_| Error::Deadline)?
    }

    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn session_id(&self) -> &str {
        &self.session
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub async fn call(
        &mut self,
        operation: Operation,
        policy: &mut impl Policy,
    ) -> Result<ResponseObservation> {
        if self.closed || Instant::now() >= self.until {
            return Err(Error::Closed);
        }
        let operation = operation.normalize()?;
        if self.request >= 1024 || (matches!(operation, Operation::Launch) == self.launched) {
            return Err(Error::Protocol);
        }
        if let Operation::Act { action_id, .. } = &operation
            && self.blocked
            && !self.actions.contains_key(action_id)
        {
            return Err(Error::Denied);
        }
        let duration = if matches!(operation, Operation::Launch) {
            20
        } else {
            12
        };
        let deadline = self
            .until
            .min(Instant::now() + Duration::from_secs(duration));
        let result = timeout_at(deadline, self.call_inner(operation, policy, deadline))
            .await
            .map_err(|_| Error::Deadline)
            .and_then(|v| v);
        if result.is_err() {
            self.closed = true;
        }
        result
    }
    async fn call_inner(
        &mut self,
        operation: Operation,
        policy: &mut impl Policy,
        deadline: Instant,
    ) -> Result<ResponseObservation> {
        self.request += 1;
        self.send(json!({"version":1,"kind":"request","session_id":self.session,"request_id":self.request,"operation":operation}),deadline).await?;
        let mut flow = Flow::default();
        loop {
            let frame = self.receive(deadline).await?;
            if frame["version"] != 1
                || frame["session_id"] != self.session
                || frame["request_id"] != self.request
            {
                return Err(Error::Protocol);
            }
            match frame["kind"].as_str() {
                Some("callback") => {
                    exact(
                        &frame,
                        &[
                            "version",
                            "kind",
                            "session_id",
                            "request_id",
                            "callback_id",
                            "method",
                            "payload",
                        ],
                    )?;
                    if frame["callback_id"].as_u64() != Some(u64::from(self.callback) + 1) {
                        return Err(Error::Protocol);
                    }
                    self.callback += 1;
                    let callback = self.callback_for(&operation, &flow, &frame)?;
                    let decision = policy.answer(&callback).await?;
                    let (ok, value) = decision_value(&callback, decision, &mut flow)?;
                    self.send(json!({"version":1,"kind":"reply","session_id":self.session,"request_id":self.request,
                        "callback_id":self.callback,"method":frame["method"],"ok":ok,"value":value}),deadline).await?;
                }
                Some("result") => {
                    exact(
                        &frame,
                        &["version", "kind", "session_id", "request_id", "ok", "value"],
                    )?;
                    let ok = frame["ok"].as_bool().ok_or(Error::Protocol)?;
                    let value = frame["value"].clone();
                    if ok {
                        self.accept_result(&operation, &flow, &value)?;
                    } else {
                        exact(&value, &["code"])?;
                        if !matches!(
                            value["code"].as_str(),
                            Some(
                                "invalid_request"
                                    | "authority_unavailable"
                                    | "authority_mismatch"
                                    | "busy"
                                    | "closed"
                                    | "control_blocked"
                                    | "stale_observation"
                                    | "page_unavailable"
                                    | "page_limit"
                                    | "observation_too_large"
                                    | "browser_unavailable"
                                    | "journal_unavailable"
                                    | "bridge_failure"
                            )
                        ) {
                            return Err(Error::Protocol);
                        }
                        if flow.begin_seen {
                            self.blocked = true;
                        }
                    }
                    match operation {
                        Operation::Launch => {
                            self.launched = ok;
                            self.closed = !ok;
                        }
                        Operation::Close => self.closed = true,
                        _ => {}
                    }
                    return Ok(ResponseObservation { ok, value });
                }
                _ => return Err(Error::Protocol),
            }
        }
    }

    fn callback_for(&self, operation: &Operation, flow: &Flow, frame: &Value) -> Result<Callback> {
        match frame["method"].as_str() {
            Some("authorize") => {
                let request: AuthorizationRequest =
                    serde_json::from_value(frame["payload"].clone())
                        .map_err(|_| Error::Protocol)?;
                let (actor, permission) = match operation {
                    Operation::Launch => (&self.binding.actor, Permission::Launch),
                    Operation::Pages { actor } | Operation::Observe { actor, .. } => {
                        (actor, Permission::Observe)
                    }
                    Operation::Act { actor, .. } => (actor, Permission::Control),
                    Operation::Close => return Err(Error::Protocol),
                };
                if request.scope != self.binding.scope
                    || &request.actor != actor
                    || request.permission != permission
                    || flow.authorization_calls >= 2
                    || (permission == Permission::Launch && flow.authorization_calls != 0)
                    || (permission == Permission::Control
                        && flow.authorization_calls == 1
                        && !flow.dispatch)
                {
                    return Err(Error::Protocol);
                }
                Ok(Callback::Authorize(request))
            }
            Some("begin") => {
                let Operation::Act {
                    actor,
                    action_id,
                    action,
                } = operation
                else {
                    return Err(Error::Protocol);
                };
                let intent: ActionIntent = serde_json::from_value(frame["payload"].clone())
                    .map_err(|_| Error::Protocol)?;
                if flow.begin_seen
                    || self.actions.contains_key(action_id)
                    || flow.grants != 1
                    || intent.scope != self.binding.scope
                    || &intent.actor != actor
                    || &intent.action_id != action_id
                    || intent.action.clone().normalize()? != *action
                    || !contracts::digest(&intent.input_digest)
                    || Some(intent.control_epoch) != flow.epoch
                {
                    return Err(Error::Protocol);
                }
                Ok(Callback::Begin(intent))
            }
            Some("complete") => {
                let receipt: ActionReceipt = serde_json::from_value(frame["payload"].clone())
                    .map_err(|_| Error::Protocol)?;
                let intent = flow.intent.as_ref().ok_or(Error::Protocol)?;
                if !flow.dispatch
                    || flow.complete.is_some()
                    || !receipt.matches(intent)
                    || (receipt.outcome == Outcome::Delivered
                        && (!flow.reauthorized || Some(receipt.control_epoch) != flow.epoch))
                {
                    return Err(Error::Protocol);
                }
                Ok(Callback::Complete(receipt))
            }
            _ => Err(Error::Protocol),
        }
    }

    fn accept_result(&mut self, operation: &Operation, flow: &Flow, value: &Value) -> Result<()> {
        match operation {
            Operation::Launch => {
                exact(value, &["driver_state", "browser_version"])?;
                if flow.grants != 1
                    || value["driver_state"] != "open"
                    || value["browser_version"]
                        .as_str()
                        .is_none_or(|s| s.is_empty() || s.len() > 64 || s == "unavailable")
                {
                    return Err(Error::Protocol);
                }
            }
            Operation::Close => {
                exact(value, &["driver_state", "physical_termination"])?;
                if value["physical_termination"] != false
                    || !matches!(
                        value["driver_state"].as_str(),
                        Some("closed" | "close_unknown")
                    )
                {
                    return Err(Error::Protocol);
                }
            }
            Operation::Pages { .. } => {
                let pages = value.as_array().ok_or(Error::Protocol)?;
                if flow.grants != 2 || flow.epoch_changed || pages.len() > 8 {
                    return Err(Error::Protocol);
                }
                for page in pages {
                    exact(page, &["page_id", "document_generation", "url", "title"])?;
                    if page["page_id"].as_str().is_none_or(|s| !identifier(s))
                        || !positive(&page["document_generation"])
                        || page["url"].as_str().is_none_or(|s| s.len() > 4096)
                        || page["title"].as_str().is_none_or(|s| s.len() > 512)
                    {
                        return Err(Error::Protocol);
                    }
                }
            }
            Operation::Observe { page_id, .. } => {
                crate::observation::validate(value, &self.binding.viewport)?;
                let scope: crate::Scope =
                    serde_json::from_value(value["scope"].clone()).map_err(|_| Error::Protocol)?;
                if flow.grants != 2
                    || flow.epoch_changed
                    || scope != self.binding.scope
                    || value["page_id"] != *page_id
                    || value["control_epoch"].as_u64() != flow.epoch
                    || value["observation_id"]
                        .as_str()
                        .is_none_or(|s| !identifier(s))
                    || !positive(&value["document_generation"])
                    || value["screenshot_base64"]
                        .as_str()
                        .is_none_or(|s| s.len() > 2_796_204)
                    || value["screenshot_sha256"]
                        .as_str()
                        .is_none_or(|s| !contracts::digest(s))
                {
                    return Err(Error::Protocol);
                }
            }
            Operation::Act { action_id, .. } => {
                let result: ActionResult =
                    serde_json::from_value(value.clone()).map_err(|_| Error::Protocol)?;
                if flow.grants == 0 {
                    return Err(Error::Protocol);
                }
                if let Some(cached) = self.actions.get(action_id) {
                    if cached.operation != *operation
                        || cached.result != result
                        || Some(result.receipt.control_epoch) != flow.epoch
                    {
                        return Err(Error::Protocol);
                    }
                    return Ok(());
                }
                let intent = flow.intent.as_ref().ok_or(Error::Protocol)?;
                if !result.receipt.matches(intent) {
                    return Err(Error::Protocol);
                }
                if result.recorded {
                    if flow.replay.as_ref() != Some(&result.receipt)
                        && !(flow.recorded && flow.complete.as_ref() == Some(&result.receipt))
                    {
                        return Err(Error::Protocol);
                    }
                } else if !flow.dispatch || result.receipt.outcome != Outcome::Unknown {
                    return Err(Error::Protocol);
                }
                if result.receipt.outcome == Outcome::Unknown || !result.recorded {
                    self.blocked = true;
                }
                self.actions.insert(
                    action_id.clone(),
                    Cached {
                        operation: operation.clone(),
                        result,
                    },
                );
            }
        }
        Ok(())
    }

    async fn send(&mut self, value: Value, deadline: Instant) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value).map_err(|_| Error::Protocol)?;
        if bytes.len() > INPUT_FRAME_BYTES {
            return Err(Error::Limit);
        }
        bytes.push(b'\n');
        self.written_bytes = self
            .written_bytes
            .checked_add(bytes.len())
            .ok_or(Error::Limit)?;
        if self.written_bytes > 16 * 1024 * 1024 {
            return Err(Error::Limit);
        }
        self.transport.write(&bytes, deadline).await
    }
    async fn receive(&mut self, deadline: Instant) -> Result<Value> {
        loop {
            if let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
                if end > OUTPUT_FRAME_BYTES || self.frames >= 16384 {
                    return Err(Error::Limit);
                }
                let value =
                    serde_json::from_slice(&self.buffer[..end]).map_err(|_| Error::Protocol)?;
                self.buffer.drain(..=end);
                self.frames += 1;
                return Ok(value);
            }
            let bytes = self.transport.read(deadline).await?;
            if bytes.is_empty() {
                return Err(Error::Closed);
            }
            self.read_bytes = self
                .read_bytes
                .checked_add(bytes.len())
                .ok_or(Error::Limit)?;
            if bytes.len() > 65536
                || self.read_bytes > 64 * 1024 * 1024
                || self.buffer.len() + bytes.len() > OUTPUT_FRAME_BYTES + 65536
            {
                return Err(Error::Limit);
            }
            self.buffer.extend_from_slice(&bytes);
        }
    }
}

pub(crate) fn exact(value: &Value, keys: &[&str]) -> Result<()> {
    if value
        .as_object()
        .is_none_or(|o| o.len() != keys.len() || keys.iter().any(|k| !o.contains_key(*k)))
    {
        Err(Error::Protocol)
    } else {
        Ok(())
    }
}
fn positive(value: &Value) -> bool {
    value
        .as_u64()
        .is_some_and(|n| (1..=(1 << 53) - 1).contains(&n))
}
fn decision_value(
    callback: &Callback,
    decision: Decision,
    flow: &mut Flow,
) -> Result<(bool, Value)> {
    // Remember the original completion even when persistence is denied. The
    // worker may then return Unknown with recorded=false, never delivered=true.
    match callback {
        Callback::Authorize(_) => flow.authorization_calls += 1,
        Callback::Begin(_) => flow.begin_seen = true,
        Callback::Complete(receipt) => flow.complete = Some(receipt.clone()),
    }
    match (callback, decision) {
        (_, Decision::Deny) => Ok((false, Value::Null)),
        (Callback::Authorize(request), Decision::Grant(grant)) if grant.matches(request) => {
            flow.grants += 1;
            flow.epoch_changed |= flow.epoch.is_some_and(|old| old != grant.control_epoch);
            flow.epoch = Some(grant.control_epoch);
            flow.reauthorized |= flow.intent.is_some();
            Ok((
                true,
                serde_json::to_value(grant).map_err(|_| Error::Protocol)?,
            ))
        }
        (Callback::Begin(intent), Decision::Dispatch) => {
            flow.intent = Some(intent.clone());
            flow.dispatch = true;
            Ok((true, json!({"kind":"dispatch"})))
        }
        (Callback::Begin(intent), Decision::Replay(receipt)) if receipt.matches(intent) => {
            flow.intent = Some(intent.clone());
            flow.replay = Some(receipt.clone());
            Ok((true, json!({"kind":"replay","receipt":receipt})))
        }
        (Callback::Complete(_), Decision::Recorded) => {
            flow.recorded = true;
            Ok((true, Value::Null))
        }
        _ => Err(Error::Protocol),
    }
}
