use super::*;
use agent_computer_sandbox::renewal::{Challenge, ChallengeFrame, Grant};

fn plan() -> StartupSandboxPlan {
    let mut bootstrap = startup_plan().bootstrap().clone();
    bootstrap.version = 2;
    bootstrap.hard_budget_ms = Some(90_000);
    bootstrap.request.timeout_seconds = 60;
    StartupSandboxPlan::new(
        &definition(false),
        "sandbox",
        identity(),
        "ac-test",
        &format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
        bootstrap,
    )
    .unwrap()
}
fn renewal(p: &StartupSandboxPlan) -> Challenge {
    Challenge {
        version: 1,
        startup_grant_digest: grant(p).digest().unwrap(),
        sequence: 1,
        nonce: "d".repeat(64),
    }
}
fn response(c: &Challenge) -> Grant {
    Grant {
        version: 1,
        challenge_digest: c.digest().unwrap(),
        lease_budget_ms: 30_000,
    }
}
fn bytes(c: Challenge) -> Vec<u8> {
    let mut b = serde_json::to_vec(&ChallengeFrame { renewal: c }).unwrap();
    b.push(b'\n');
    b
}

#[tokio::test]
async fn renewal_keeps_stdin_open_and_survives_cancelled_fragment_reads() {
    let p = plan();
    let (partial_tx, partial_rx) = tokio::sync::oneshot::channel();
    let mut replies = attached_replies(&p, move |s, p| {
        expect_grant(s, p);
        let c = renewal(p);
        let b = bytes(c.clone());
        send(s, 1, &b[..10]);
        partial_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(60));
        send(s, 1, &b[10..]);
        let b = read_binary(s);
        assert_eq!(b[0], 0);
        let r = Grant::parse(&b[1..]).unwrap();
        assert_eq!(r, response(&c));
        let mut value = report(p);
        value["renewal"] = json!({"sequence":1,"grant_digest":r.digest().unwrap()});
        send_report(s, &value);
        send(s, 3, br#"{"status":"Success"}"#);
    });
    replies.extend(observation(running(&p)));
    let (client, _, server) = fixture(replies);
    let mut channel = client
        .attach_startup(&p, &observed(&p))
        .await
        .unwrap()
        .start(&grant(&p))
        .await
        .unwrap();
    partial_rx.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), channel.next_event())
            .await
            .is_err()
    );
    let ExecutionEvent::Renewal(c) = channel.next_event().await.unwrap() else {
        panic!("missing renewal")
    };
    assert_eq!(c, renewal(&p));
    assert_eq!(
        channel.next_event().await.unwrap_err(),
        Error::MutationUnconfirmed
    );
    channel.send_renewal(&response(&c)).await.unwrap();
    let ExecutionEvent::Complete(result) = channel.next_event().await.unwrap() else {
        panic!("missing completion")
    };
    assert_eq!(
        serde_json::from_slice::<Value>(result.report_bytes()).unwrap()["renewal"]["sequence"],
        1
    );
    assert_eq!(
        channel.next_event().await.unwrap_err(),
        Error::MutationUnconfirmed
    );
    server.join().unwrap();
}

#[tokio::test]
async fn foreign_skipped_replayed_and_unbounded_renewal_frames_are_uncertain() {
    for case in 0..5 {
        let p = plan();
        let mut replies = attached_replies(&p, move |s, p| {
            expect_grant(s, p);
            let mut c = renewal(p);
            match case {
                0 => c.startup_grant_digest = format!("sha256:{}", "e".repeat(64)),
                1 => c.sequence = 2,
                2 => c.version = 3,
                3 => c.nonce = "bad".into(),
                _ => {}
            }
            let b = bytes(c.clone());
            send(s, 1, &b);
            if case == 4 {
                let r = read_binary(s);
                assert_eq!(Grant::parse(&r[1..]).unwrap(), response(&c));
                send(s, 1, &b);
            }
            assert_no_grant(s);
        });
        replies.extend(observation(running(&p)));
        let (client, _, server) = fixture(replies);
        let mut channel = client
            .attach_startup(&p, &observed(&p))
            .await
            .unwrap()
            .start(&grant(&p))
            .await
            .unwrap();
        if case == 4 {
            let ExecutionEvent::Renewal(c) = channel.next_event().await.unwrap() else {
                panic!("missing renewal")
            };
            channel.send_renewal(&response(&c)).await.unwrap();
        }
        assert_eq!(
            channel.next_event().await.unwrap_err(),
            Error::MutationUnconfirmed
        );
        drop(channel);
        server.join().unwrap();
    }
}

#[tokio::test]
async fn wrong_response_never_writes_and_report_must_bind_accepted_renewal() {
    for case in 0..3 {
        let p = plan();
        let mut replies = attached_replies(&p, move |s, p| {
            expect_grant(s, p);
            send(s, 1, &bytes(renewal(p)));
            if case == 0 {
                assert_no_grant(s);
                return;
            }
            let _ = read_binary(s);
            if case == 1 {
                return;
            }
            // This stale completion omits the renewal just acknowledged above.
            send_report(s, &report(p));
            send(s, 3, br#"{"status":"Success"}"#);
        });
        replies.extend(observation(running(&p)));
        let (client, _, server) = fixture(replies);
        let mut channel = client
            .attach_startup(&p, &observed(&p))
            .await
            .unwrap()
            .start(&grant(&p))
            .await
            .unwrap();
        let ExecutionEvent::Renewal(c) = channel.next_event().await.unwrap() else {
            panic!("missing renewal")
        };
        let mut r = response(&c);
        if case == 0 {
            r.challenge_digest = format!("sha256:{}", "e".repeat(64));
            assert_eq!(
                channel.send_renewal(&r).await.unwrap_err(),
                Error::PreconditionFailed
            );
        } else {
            channel.send_renewal(&r).await.unwrap();
            assert_eq!(
                channel.next_event().await.unwrap_err(),
                Error::MutationUnconfirmed
            );
        }
        drop(channel);
        server.join().unwrap();
    }
}
