use super::*;
use agent_computer_sandbox::{Bootstrap, Request, StartupChallenge, StartupGrant, StartupHello};
use std::net::TcpStream;
use tokio_tungstenite::tungstenite::{self, Message, WebSocket, protocol::Role};

type Socket = WebSocket<TcpStream>;

fn startup_plan() -> StartupSandboxPlan {
    StartupSandboxPlan::new(
        &definition(false),
        "sandbox",
        identity(),
        "ac-test",
        &format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
        Bootstrap {
            version: 1,
            intent_digest: format!("sha256:{}", "b".repeat(64)),
            request: Request {
                execution_id: identity().instance,
                generation: 1,
                argv: vec!["/bin/true".into()],
                cwd: String::new(),
                timeout_seconds: 10,
                lease_budget_ms: 30000,
                term_grace_ms: 100,
                output_limit_bytes: 10,
            },
        },
    )
    .unwrap()
}
fn challenge(plan: &StartupSandboxPlan) -> StartupChallenge {
    StartupChallenge {
        version: 1,
        execution_id: identity().instance,
        generation: 1,
        bootstrap_digest: plan.bootstrap().digest().unwrap(),
        nonce: "c".repeat(64),
    }
}
fn grant(plan: &StartupSandboxPlan) -> StartupGrant {
    StartupGrant {
        version: 1,
        challenge_digest: challenge(plan).digest().unwrap(),
        lease_budget_ms: 10000,
    }
}
fn running(plan: &StartupSandboxPlan) -> Value {
    let mut p = pod(plan.pod_plan());
    p["status"]["phase"] = json!("Running");
    p
}
fn observed(plan: &StartupSandboxPlan) -> PodObservation {
    verify::pod(plan.pod_plan(), &running(plan), None).unwrap()
}
fn observation(pod: Value) -> Vec<Reply> {
    let mut replies = probe_replies();
    replies.push(Reply::Json(200, pod));
    replies
}
fn handshake(
    mut stream: TcpStream,
    headers: String,
    protocol: &str,
    accept: Option<&str>,
) -> Socket {
    assert!(headers.starts_with("GET /api/v1/namespaces/ac-test/pods/ac-"));
    assert!(headers.contains(
        "/attach?container=sandbox&stdin=true&stdout=true&stderr=true&tty=false HTTP/1.1"
    ));
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("sec-websocket-protocol: v5.channel.k8s.io\r\n")
    );
    let key = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-key"))
                .map(|(_, v)| v.trim())
        })
        .unwrap();
    let expected = tungstenite::handshake::derive_accept_key(key.as_bytes());
    write!(stream, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\nSec-WebSocket-Protocol: {protocol}\r\n\r\n", accept.unwrap_or(&expected)).unwrap();
    WebSocket::from_raw_socket(stream, Role::Server, None)
}
fn read_binary(socket: &mut Socket) -> Vec<u8> {
    match socket.read().unwrap() {
        Message::Binary(b) => b.to_vec(),
        other => panic!("unexpected {other:?}"),
    }
}
fn expect_hello(socket: &mut Socket, plan: &StartupSandboxPlan) {
    let bytes = read_binary(socket);
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes.last(), Some(&b'\n'));
    let hello: StartupHello = serde_json::from_slice(&bytes[1..]).unwrap();
    assert_eq!(hello.version, 1);
    assert_eq!(hello.bootstrap_digest, plan.bootstrap().digest().unwrap());
}
fn send(socket: &mut Socket, channel: u8, bytes: &[u8]) {
    let mut frame = vec![channel];
    frame.extend_from_slice(bytes);
    socket.send(Message::Binary(frame.into())).unwrap();
}
fn send_challenge(socket: &mut Socket, plan: &StartupSandboxPlan) {
    let mut bytes = serde_json::to_vec(&challenge(plan)).unwrap();
    bytes.push(b'\n');
    // Splitting JSON across multiple channel messages must preserve the line.
    send(socket, 1, &bytes[..9]);
    send(socket, 1, &bytes[9..]);
}
fn expect_grant(socket: &mut Socket, plan: &StartupSandboxPlan) {
    let bytes = read_binary(socket);
    assert_eq!(bytes[0], 0);
    assert_eq!(
        StartupGrant::parse(&bytes[1..]).unwrap().digest().unwrap(),
        grant(plan).digest().unwrap()
    );
    assert_eq!(read_binary(socket), [255, 0]);
}
fn report(plan: &StartupSandboxPlan) -> Value {
    let grant = grant(plan);
    let mut request = plan.bootstrap().request.clone();
    request.lease_budget_ms = grant.lease_budget_ms;
    json!({"version":1,"challenge_digest":grant.challenge_digest,"grant_digest":grant.digest().unwrap(),
        "report":{"version":1,"execution_id":request.execution_id,"generation":request.generation,
            "request_digest":request.digest().unwrap(),"outcome":"component-observation-only"}})
}
fn send_report(socket: &mut Socket, value: &Value) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    send(socket, 1, &bytes);
}
fn attached_replies(
    plan: &StartupSandboxPlan,
    script: impl FnOnce(&mut Socket, &StartupSandboxPlan) + Send + 'static,
) -> Vec<Reply> {
    let mut replies = observation(running(plan));
    let copy = plan.clone();
    replies.push(Reply::Upgrade(Box::new(move |stream, headers| {
        let mut socket = handshake(stream, headers, "v5.channel.k8s.io", None);
        expect_hello(&mut socket, &copy);
        send_challenge(&mut socket, &copy);
        script(&mut socket, &copy);
    })));
    replies.extend(observation(running(plan)));
    replies
}
fn assert_no_grant(socket: &mut Socket) {
    // Closure/reset are both possible when the client rejects the observation.
    assert!(!matches!(socket.read(), Ok(Message::Binary(_))));
}

#[test]
fn startup_plan_requires_approved_supervisor_and_preserves_strict_admission() {
    let plan = startup_plan();
    let construct = |image: &str, id| {
        StartupSandboxPlan::new(
            &definition(false),
            "sandbox",
            id,
            "ac-test",
            image,
            plan.bootstrap().clone(),
        )
    };
    assert_eq!(
        construct("foreign", identity()).unwrap_err(),
        Error::UnsupportedSandbox
    );
    let mut id = identity();
    id.instance = "other".into();
    assert_eq!(
        construct("foreign", id).unwrap_err(),
        Error::InvalidIdentity
    );
    for path in ["/spec/containers/0/stdin", "/spec/containers/0/stdinOnce"] {
        let mut p = running(&plan);
        *p.pointer_mut(path).unwrap() = json!(false);
        assert_eq!(
            verify::pod(plan.pod_plan(), &p, None).unwrap_err(),
            Error::IdentityMismatch
        );
    }
    let mut p = running(&plan);
    p["spec"]["containers"][0]
        .as_object_mut()
        .unwrap()
        .remove("tty");
    verify::pod(plan.pod_plan(), &p, None).unwrap();
    p["spec"]["containers"][0]["tty"] = json!(true);
    assert_eq!(
        verify::pod(plan.pod_plan(), &p, None).unwrap_err(),
        Error::IdentityMismatch
    );
}

#[tokio::test]
async fn attach_fragmented_challenge_single_grant_and_correlated_raw_report() {
    let plan = startup_plan();
    let mut replies = attached_replies(&plan, |socket, p| {
        expect_grant(socket, p);
        socket.send(Message::Ping(vec![1, 2].into())).unwrap();
        assert!(matches!(socket.read().unwrap(), Message::Pong(_)));
        send(socket, 2, b"supervisor diagnostics");
        send(socket, 3, br#"{"status":"Success"}"#);
        send_report(socket, &report(p));
    });
    replies.extend(observation(running(&plan)));
    let (client, captured, server) = fixture(replies);
    let channel = client
        .attach_startup(&plan, &observed(&plan))
        .await
        .unwrap();
    assert_eq!(channel.pod_uid(), "pod-uid");
    assert_eq!(
        channel.challenge().digest().unwrap(),
        challenge(&plan).digest().unwrap()
    );
    let result = channel.run(&grant(&plan)).await.unwrap();
    assert_eq!(result.pod_uid(), "pod-uid");
    assert_eq!(
        serde_json::from_slice::<Value>(result.report_bytes()).unwrap(),
        report(&plan)
    );
    assert_eq!(result.supervisor_stderr(), b"supervisor diagnostics");
    server.join().unwrap();
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 13);
    assert_eq!(
        requests
            .iter()
            .filter(|(p, _)| p.contains("/attach?"))
            .count(),
        1
    );
    assert!(
        requests
            .iter()
            .all(|(p, _)| p.starts_with("GET ") && !p.contains("/exec"))
    );
}

#[tokio::test]
async fn replacement_uid_before_or_after_challenge_or_before_grant_never_receives_grant() {
    for stage in 0..3 {
        let plan = startup_plan();
        let mut replaced = running(&plan);
        replaced["metadata"]["uid"] = json!("replacement");
        let mut replies = if stage == 0 {
            observation(replaced.clone())
        } else {
            attached_replies(&plan, |socket, _| assert_no_grant(socket))
        };
        if stage == 1 {
            *replies.last_mut().unwrap() = Reply::Json(200, replaced.clone());
        }
        if stage == 2 {
            replies.extend(observation(replaced));
        }
        let (client, _, server) = fixture(replies);
        let result = client.attach_startup(&plan, &observed(&plan)).await;
        let error = if stage == 2 {
            result.unwrap().run(&grant(&plan)).await.unwrap_err()
        } else {
            result.unwrap_err()
        };
        assert_eq!(error, Error::IdentityMismatch);
        server.join().unwrap();
    }
}

#[tokio::test]
async fn attach_rejects_downgraded_or_forged_handshake_without_hello() {
    for (protocol, accept) in [
        ("v4.channel.k8s.io", None),
        ("v5.channel.k8s.io", Some("incorrect")),
    ] {
        let plan = startup_plan();
        let mut replies = observation(running(&plan));
        replies.push(Reply::Upgrade(Box::new(move |stream, headers| {
            let mut socket = handshake(stream, headers, protocol, accept);
            assert_no_grant(&mut socket);
        })));
        let (client, _, server) = fixture(replies);
        assert_eq!(
            client
                .attach_startup(&plan, &observed(&plan))
                .await
                .unwrap_err(),
            Error::InvalidResponse
        );
        server.join().unwrap();
    }
}

#[tokio::test]
async fn attach_rejects_malformed_oversized_or_foreign_challenge() {
    for case in 0..5 {
        let plan = startup_plan();
        let copy = plan.clone();
        let mut replies = observation(running(&plan));
        replies.push(Reply::Upgrade(Box::new(move |stream, headers| {
            let mut socket = handshake(stream, headers, "v5.channel.k8s.io", None);
            expect_hello(&mut socket, &copy);
            match case {
                0 => socket.send(Message::Text("invalid".into())).unwrap(),
                1 => send(&mut socket, 1, &vec![b'x'; 4097]),
                2 => send(&mut socket, 2, b"unexpected"),
                3 => {
                    let mut c = challenge(&copy);
                    c.generation += 1;
                    send(&mut socket, 1, &serde_json::to_vec(&c).unwrap());
                    send(&mut socket, 1, b"\n");
                }
                _ => send(&mut socket, 1, b"{}\n{}\n"),
            }
            assert_no_grant(&mut socket);
        })));
        let (client, _, server) = fixture(replies);
        let error = client
            .attach_startup(&plan, &observed(&plan))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            match case {
                1 => Error::ResponseLimit,
                3 => Error::IdentityMismatch,
                _ => Error::InvalidResponse,
            }
        );
        server.join().unwrap();
    }
}

#[tokio::test]
async fn attach_never_follows_redirect_or_retries_failed_upgrade() {
    for reply in [
        Reply::Raw(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ),
        Reply::Json(403, json!({})),
        Reply::Drop,
    ] {
        let plan = startup_plan();
        let mut replies = observation(running(&plan));
        replies.push(reply);
        let (client, captured, server) = fixture(replies);
        assert!(
            client
                .attach_startup(&plan, &observed(&plan))
                .await
                .is_err()
        );
        server.join().unwrap();
        assert_eq!(captured.lock().unwrap().len(), 5);
    }
}

#[tokio::test]
async fn invalid_grant_is_rejected_before_any_grant_write() {
    for case in 0..3 {
        let plan = startup_plan();
        let replies = attached_replies(&plan, |socket, _| assert_no_grant(socket));
        let (client, captured, server) = fixture(replies);
        let channel = client
            .attach_startup(&plan, &observed(&plan))
            .await
            .unwrap();
        let mut grant = grant(&plan);
        match case {
            0 => grant.version = 2,
            1 => grant.challenge_digest = format!("sha256:{}", "d".repeat(64)),
            _ => grant.lease_budget_ms = 30001,
        }
        assert!(channel.run(&grant).await.is_err());
        server.join().unwrap();
        assert_eq!(captured.lock().unwrap().len(), 9);
    }
}

#[tokio::test]
async fn post_grant_disconnect_corruption_failure_and_overflow_remain_uncertain() {
    for case in 0..7 {
        let plan = startup_plan();
        let mut replies = attached_replies(&plan, move |socket, p| {
            expect_grant(socket, p);
            match case {
                0 => {}
                1 => {
                    send(socket, 1, b"{\"version\":");
                }
                2 => {
                    let mut r = report(p);
                    r["report"]["generation"] = json!(2);
                    send_report(socket, &r);
                    send(socket, 3, br#"{"status":"Success"}"#);
                }
                3 => {
                    send_report(socket, &report(p));
                    send(socket, 3, br#"{"status":"Failure"}"#);
                }
                4 => {
                    send(socket, 1, &vec![b' '; 17000]);
                }
                5 => {
                    send(socket, 2, &vec![b' '; 65536]);
                }
                _ => {
                    send(socket, 4, b"unexpected");
                }
            }
        });
        replies.extend(observation(running(&plan)));
        let (client, captured, server) = fixture(replies);
        let channel = client
            .attach_startup(&plan, &observed(&plan))
            .await
            .unwrap();
        assert_eq!(
            channel.run(&grant(&plan)).await.unwrap_err(),
            Error::MutationUnconfirmed,
            "case {case}"
        );
        server.join().unwrap();
        assert_eq!(captured.lock().unwrap().len(), 13);
    }
}
