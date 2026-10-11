use super::*;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::time::Instant;

pub(super) fn binding() -> Binding {
    Binding::parse(br#"{"scope":{"organization_id":"org","computer_id":"computer","computer_generation":1,"app_instance_id":"app","app_generation":1,"profile_owner_id":"owner"},"actor":{"principal_id":"owner","connection_session_id":"connection"},"viewport":{"width":800,"height":600},"runtime_ms":60000}"#).unwrap()
}
const SESSION: &str = "0123456789abcdef0123456789abcdef";
struct Wire {
    input: VecDeque<Vec<u8>>,
    output: Arc<Mutex<Vec<Value>>>,
}
impl Transport for Wire {
    async fn write(&mut self, bytes: &[u8], _: Instant) -> Result<()> {
        self.output
            .lock()
            .unwrap()
            .push(serde_json::from_slice(bytes).unwrap());
        Ok(())
    }
    async fn read(&mut self, _: Instant) -> Result<Vec<u8>> {
        self.input.pop_front().ok_or(Error::Closed)
    }
}
fn scripted_wire(frames: Vec<Value>) -> (Wire, Arc<Mutex<Vec<Value>>>) {
    let mut bytes = Vec::new();
    for frame in frames {
        bytes.extend(serde_json::to_vec(&frame).unwrap());
        bytes.push(b'\n');
    }
    let output = Arc::new(Mutex::new(Vec::new()));
    (
        Wire {
            input: bytes.chunks(37).map(Vec::from).collect(),
            output: output.clone(),
        },
        output,
    )
}
fn hello() -> Value {
    json!({"version":1,"kind":"challenge","session_id":SESSION,"binding_digest":binding().digest().unwrap()})
}
fn callback(request: u32, id: u32, method: &str, payload: Value) -> Value {
    json!({"version":1,"kind":"callback","session_id":SESSION,"request_id":request,"callback_id":id,"method":method,"payload":payload})
}
fn authorize(request: u32, id: u32, permission: Permission) -> Value {
    let b = binding();
    callback(
        request,
        id,
        "authorize",
        json!({"scope":b.scope,"actor":b.actor,"permission":permission}),
    )
}
fn result(request: u32, value: Value) -> Value {
    json!({"version":1,"kind":"result","session_id":SESSION,"request_id":request,"ok":true,"value":value})
}
fn launched() -> Vec<Value> {
    vec![
        hello(),
        authorize(1, 1, Permission::Launch),
        result(
            1,
            json!({"driver_state":"open","browser_version":"156.fixture"}),
        ),
    ]
}
#[derive(Default)]
struct Controller {
    calls: usize,
    deny: bool,
    deny_complete: bool,
    epoch: u64,
}
impl Policy for Controller {
    async fn answer(&mut self, callback: &Callback) -> Result<Decision> {
        self.calls += 1;
        if self.deny {
            return Ok(Decision::Deny);
        }
        Ok(match callback {
            Callback::Authorize(r) => Decision::Grant(Grant {
                scope: r.scope.clone(),
                actor: r.actor.clone(),
                permission: r.permission,
                control_epoch: self.epoch.max(1),
                valid_for_ms: 10000,
            }),
            Callback::Begin(_) => Decision::Dispatch,
            Callback::Complete(_) if self.deny_complete => Decision::Deny,
            Callback::Complete(_) => Decision::Recorded,
        })
    }
}

#[test]
fn binding_digest_matches_the_actual_typescript_worker_and_rejects_unsafe_shapes() {
    let b = binding();
    assert_eq!(
        b.digest().unwrap(),
        "sha256:54421cd3de5eb0038910e4f89e61aa41a206e43493a2b745f9cdb146b937a174"
    );
    for (pointer, value) in [
        ("/scope/computer_generation", json!(9007199254740992_u64)),
        ("/scope/app_generation", json!(0)),
        ("/scope/profile_owner_id", json!("../other")),
        ("/runtime_ms", json!(300001)),
        ("/viewport/width", json!(319)),
    ] {
        let mut changed = serde_json::to_value(&b).unwrap();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert!(
            Binding::parse(&serde_json::to_vec(&changed).unwrap()).is_err(),
            "{pointer}"
        );
    }
    let mut changed = serde_json::to_value(b).unwrap();
    changed["token"] = json!("secret");
    assert!(Binding::parse(&serde_json::to_vec(&changed).unwrap()).is_err());
}

#[test]
fn actions_match_finite_driver_contract_without_scripts_or_unbounded_values() {
    let base = json!({"kind":"navigate","page_id":"page","observation_id":"obs","url":"https://example.test"});
    let action: Action = serde_json::from_value(base.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(action.normalize().unwrap()).unwrap()["url"],
        "https://example.test/"
    );
    for url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "data:text/html,a",
        "https://u:p@example.test",
    ] {
        let mut v = base.clone();
        v["url"] = json!(url);
        let action: Action = serde_json::from_value(v).unwrap();
        assert!(action.normalize().is_err());
    }
    assert!(serde_json::from_value::<Action>(json!({"kind":"create_page","script":"x"})).is_err());
    let action = Action::ClickAt {
        page_id: "p".into(),
        observation_id: "o".into(),
        x: f64::NAN,
        y: 0.0,
    };
    assert!(action.normalize().is_err());
}

#[tokio::test]
async fn fragmented_private_protocol_requires_launch_authorization_and_explicit_close() {
    let mut frames = launched();
    frames.push(result(
        2,
        json!({"driver_state":"closed","physical_termination":false}),
    ));
    let (wire, output) = scripted_wire(frames);
    let mut s = Session::connect(wire, binding()).await.unwrap();
    let mut policy = Controller::default();
    assert!(s.call(Operation::Launch, &mut policy).await.unwrap().ok());
    assert_eq!(policy.calls, 1);
    assert!(s.call(Operation::Close, &mut policy).await.unwrap().ok());
    assert!(s.is_closed());
    assert_eq!(
        output.lock().unwrap()[2]["value"]["scope"],
        serde_json::to_value(binding().scope).unwrap()
    );
    assert!(matches!(
        s.call(Operation::Launch, &mut policy).await,
        Err(Error::Closed)
    ));
}

#[tokio::test]
async fn hostile_callback_identity_is_rejected_before_any_policy_call() {
    for (pointer, value) in [
        ("/payload/scope/app_generation", json!(2)),
        ("/payload/actor/principal_id", json!("other")),
        ("/payload/permission", json!("control")),
        ("/request_id", json!(2)),
        ("/callback_id", json!(2)),
        ("/session_id", json!("foreign")),
    ] {
        let mut frame = authorize(1, 1, Permission::Launch);
        *frame.pointer_mut(pointer).unwrap() = value;
        let (wire, _) = scripted_wire(vec![hello(), frame]);
        let mut s = Session::connect(wire, binding()).await.unwrap();
        let mut policy = Controller::default();
        assert!(
            matches!(
                s.call(Operation::Launch, &mut policy).await,
                Err(Error::Protocol)
            ),
            "{pointer}"
        );
        assert_eq!(policy.calls, 0);
        assert!(s.is_closed());
    }
}

#[tokio::test]
async fn worker_cannot_claim_success_without_grant_or_retry_a_denied_callback() {
    let (wire, _) = scripted_wire(vec![
        hello(),
        result(1, json!({"driver_state":"open","browser_version":"156"})),
    ]);
    let mut s = Session::connect(wire, binding()).await.unwrap();
    let mut policy = Controller::default();
    assert!(matches!(
        s.call(Operation::Launch, &mut policy).await,
        Err(Error::Protocol)
    ));
    assert_eq!(policy.calls, 0);
    let (wire, _) = scripted_wire(vec![
        hello(),
        authorize(1, 1, Permission::Launch),
        authorize(1, 2, Permission::Launch),
    ]);
    let mut s = Session::connect(wire, binding()).await.unwrap();
    let mut policy = Controller {
        deny: true,
        ..Default::default()
    };
    assert!(matches!(
        s.call(Operation::Launch, &mut policy).await,
        Err(Error::Protocol)
    ));
    assert_eq!(policy.calls, 1);
}

fn action() -> Operation {
    Operation::Act {
        actor: binding().actor,
        action_id: "action".into(),
        action: Action::CreatePage {},
    }
}
fn intent() -> Value {
    json!({"scope":binding().scope,"actor":binding().actor,"control_epoch":1,"action_id":"action","input_digest":format!("sha256:{}","a".repeat(64)),"action":{"kind":"create_page"}})
}
fn receipt(outcome: &str) -> Value {
    json!({"action_id":"action","input_digest":format!("sha256:{}","a".repeat(64)),"app_instance_id":"app","app_generation":1,"control_epoch":1,"outcome":outcome,"page_id":"new_page"})
}

#[tokio::test]
async fn action_callbacks_bind_the_full_original_input_and_order() {
    for (pointer, value) in [
        ("/action_id", json!("other")),
        ("/control_epoch", json!(2)),
        ("/input_digest", json!("not-a-digest")),
        ("/actor/connection_session_id", json!("new_connection")),
        (
            "/action",
            json!({"kind":"close_page","page_id":"p","observation_id":"o"}),
        ),
    ] {
        let mut changed = intent();
        *changed.pointer_mut(pointer).unwrap() = value;
        let mut frames = launched();
        frames.extend([
            authorize(2, 2, Permission::Control),
            callback(2, 3, "begin", changed),
        ]);
        let (wire, _) = scripted_wire(frames);
        let mut s = Session::connect(wire, binding()).await.unwrap();
        let mut policy = Controller::default();
        s.call(Operation::Launch, &mut policy).await.unwrap();
        assert!(
            matches!(s.call(action(), &mut policy).await, Err(Error::Protocol)),
            "{pointer}"
        );
        assert_eq!(policy.calls, 2);
    }
}

#[tokio::test]
async fn lost_completion_ack_stays_unknown_and_cannot_authorize_another_effect() {
    let unknown = json!({"receipt":receipt("unknown"),"recorded":false});
    let mut frames = launched();
    frames.extend([
        authorize(2, 2, Permission::Control),
        callback(2, 3, "begin", intent()),
        authorize(2, 4, Permission::Control),
        callback(2, 5, "complete", receipt("delivered")),
        result(2, unknown.clone()),
        authorize(3, 6, Permission::Control),
        result(3, unknown),
    ]);
    let (wire, output) = scripted_wire(frames);
    let mut s = Session::connect(wire, binding()).await.unwrap();
    let mut policy = Controller {
        deny_complete: true,
        ..Default::default()
    };
    s.call(Operation::Launch, &mut policy).await.unwrap();
    let response = s.call(action(), &mut policy).await.unwrap();
    assert_eq!(response.value()["recorded"], false);
    assert_eq!(
        s.call(action(), &mut policy).await.unwrap().value(),
        response.value()
    );
    assert!(matches!(
        s.call(
            Operation::Act {
                actor: binding().actor,
                action_id: "different".into(),
                action: Action::CreatePage {}
            },
            &mut policy
        )
        .await,
        Err(Error::Denied)
    ));
    assert_eq!(
        output
            .lock()
            .unwrap()
            .iter()
            .filter(|f| f["value"]["kind"] == "dispatch")
            .count(),
        1
    );
}

#[tokio::test]
async fn delivered_receipt_requires_durable_completion_and_reads_cannot_cross_epochs() {
    let mut frames = launched();
    frames.extend([
        authorize(2, 2, Permission::Control),
        callback(2, 3, "begin", intent()),
        authorize(2, 4, Permission::Control),
        result(2, json!({"receipt":receipt("delivered"),"recorded":true})),
    ]);
    let (wire, _) = scripted_wire(frames);
    let mut s = Session::connect(wire, binding()).await.unwrap();
    let mut policy = Controller::default();
    s.call(Operation::Launch, &mut policy).await.unwrap();
    assert!(matches!(
        s.call(action(), &mut policy).await,
        Err(Error::Protocol)
    ));
    struct Changing(u64);
    impl Policy for Changing {
        async fn answer(&mut self, c: &Callback) -> Result<Decision> {
            let Callback::Authorize(r) = c else { panic!() };
            self.0 += 1;
            Ok(Decision::Grant(Grant {
                scope: r.scope.clone(),
                actor: r.actor.clone(),
                permission: r.permission,
                control_epoch: self.0,
                valid_for_ms: 1000,
            }))
        }
    }
    let mut frames = launched();
    frames.extend([
        authorize(2, 2, Permission::Observe),
        authorize(2, 3, Permission::Observe),
        result(2, json!([])),
    ]);
    let (wire, _) = scripted_wire(frames);
    let mut s = Session::connect(wire, binding()).await.unwrap();
    let mut policy = Changing(0);
    s.call(Operation::Launch, &mut policy).await.unwrap();
    assert!(matches!(
        s.call(
            Operation::Pages {
                actor: binding().actor
            },
            &mut policy
        )
        .await,
        Err(Error::Protocol)
    ));
}

#[tokio::test]
async fn caller_policy_timeout_closes_the_session_without_sending_a_grant() {
    struct Never;
    impl Policy for Never {
        async fn answer(&mut self, _: &Callback) -> Result<Decision> {
            std::future::pending().await
        }
    }
    let mut b = binding();
    b.runtime_ms = 1000;
    let mut h = hello();
    h["binding_digest"] = json!(b.digest().unwrap());
    let (wire, output) = scripted_wire(vec![h, authorize(1, 1, Permission::Launch)]);
    let mut s = Session::connect(wire, b).await.unwrap();
    assert!(matches!(
        s.call(Operation::Launch, &mut Never).await,
        Err(Error::Deadline)
    ));
    assert!(s.is_closed());
    assert_eq!(output.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn drain_barrier_requires_exact_epoch_and_correlated_response() {
    for wrong in [false, true] {
        let mut frames = launched();
        frames.push(result(
            2,
            json!({"control_epoch":if wrong{2}else{1},"drained":true,"requires_observation":true}),
        ));
        let (wire, _) = scripted_wire(frames);
        let mut session = Session::connect(wire, binding()).await.unwrap();
        let mut policy = Controller::default();
        session.call(Operation::Launch, &mut policy).await.unwrap();
        let response = session
            .call(Operation::Drain { control_epoch: 1 }, &mut policy)
            .await;
        if wrong {
            assert!(matches!(response, Err(Error::Protocol)));
            assert!(session.is_closed());
        } else {
            let response = response.unwrap();
            assert!(response.ok());
            assert_eq!(response.scope(), &binding().scope);
            assert_eq!(response.operation(), &Operation::Drain { control_epoch: 1 });
        }
        assert_eq!(
            policy.calls, 1,
            "barrier must never grant application permissions"
        );
    }
}
