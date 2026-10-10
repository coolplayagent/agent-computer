use super::*;
use agent_computer_sandbox::streaming::{Chunk, Frame, Stream, Transcript};

fn plan(renewable: bool) -> StartupSandboxPlan {
    let mut bootstrap = startup_plan().bootstrap().clone();
    bootstrap.version = 3;
    bootstrap.hard_budget_ms = renewable.then_some(90_000);
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
fn chunks(p: &StartupSandboxPlan) -> (Vec<Chunk>, Value) {
    let grant = grant(p);
    let mut t = Transcript::new(grant.digest().unwrap(), 10).unwrap();
    let mut chunks = vec![];
    for (stream, bytes) in [
        (Stream::Stdout, vec![0, 255, 10]),
        (Stream::Stderr, b"error".to_vec()),
    ] {
        let c = Chunk {
            version: 1,
            startup_grant_digest: grant.digest().unwrap(),
            sequence: t.progress().sequence + 1,
            previous_digest: t.progress().last_digest.clone(),
            stream,
            offset: 0,
            observed_bytes: bytes.len() as u64,
            bytes,
            truncated: false,
            eof: true,
        };
        t.accept(&c).unwrap();
        chunks.push(c);
    }
    let mut value = report(p);
    value["stream"] = serde_json::to_value(t.progress()).unwrap();
    let r = value["report"].as_object_mut().unwrap();
    for (name,value) in json!({"outcome":"succeeded","main_exit_code":0,"main_signal":null,"reaped_processes":1,"children_reaped":true,"kill_sent":false,"elapsed_ms":2000,"stdout":{"bytes":[0,255,10],"observed_bytes":3,"truncated":false,"eof":true},"stderr":{"bytes":[101,114,114,111,114],"observed_bytes":5,"truncated":false,"eof":true}}).as_object().unwrap() { r.insert(name.clone(),value.clone()); }
    (chunks, value)
}
fn bytes(chunk: Chunk) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&Frame { output: chunk }).unwrap();
    bytes.push(b'\n');
    bytes
}

#[tokio::test]
async fn output_and_renewal_frames_share_transport_but_keep_independent_digest_chains() {
    use agent_computer_sandbox::renewal::{Challenge, ChallengeFrame, Grant};
    let p = plan(true);
    let challenge = Challenge {
        version: 1,
        startup_grant_digest: grant(&p).digest().unwrap(),
        sequence: 1,
        nonce: "f".repeat(64),
    };
    let response = Grant {
        version: 1,
        challenge_digest: challenge.digest().unwrap(),
        lease_budget_ms: 30000,
    };
    let expected = response.clone();
    let mut replies = attached_replies(&p, move |s, p| {
        expect_grant(s, p);
        let (chunks, mut report) = chunks(p);
        let mut first = bytes(chunks[0].clone());
        first.extend(serde_json::to_vec(&ChallengeFrame { renewal: challenge }).unwrap());
        first.push(b'\n');
        // The next chunk is already buffered when renewal requires a response.
        first.extend(bytes(chunks[1].clone()));
        send(s, 1, &first);
        let incoming = read_binary(s);
        assert_eq!(incoming[0], 0);
        assert_eq!(Grant::parse(&incoming[1..]).unwrap(), expected);
        report["renewal"] = json!({"sequence":1,"grant_digest":expected.digest().unwrap()});
        send_report(s, &report);
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
    assert!(matches!(
        channel.next_event().await.unwrap(),
        ExecutionEvent::Output(_)
    ));
    assert!(matches!(
        channel.next_event().await.unwrap(),
        ExecutionEvent::Renewal(_)
    ));
    channel.send_renewal(&response).await.unwrap();
    let ExecutionEvent::Output(second) = channel.next_event().await.unwrap() else {
        panic!("second chunk missing")
    };
    assert_eq!(second.chunk(), &chunks(&p).0[1]);
    let ExecutionEvent::Complete(done) = channel.next_event().await.unwrap() else {
        panic!("final report missing")
    };
    let value: Value = serde_json::from_slice(done.report_bytes()).unwrap();
    assert_eq!(value["stream"]["sequence"], 2);
    assert_eq!(value["renewal"]["sequence"], 1);
    server.join().unwrap();
}

#[tokio::test]
async fn fixed_and_renewable_streams_preserve_fragmented_reads_and_coalesced_frames() {
    for renewable in [false, true] {
        let p = plan(renewable);
        let (partial_tx, partial_rx) = tokio::sync::oneshot::channel();
        let mut replies = attached_replies(&p, move |s, p| {
            expect_grant(s, p);
            let (chunks, report) = chunks(p);
            let mut b = bytes(chunks[0].clone());
            b.extend(bytes(chunks[1].clone()));
            b.extend(serde_json::to_vec(&report).unwrap());
            b.push(b'\n');
            send(s, 1, &b[..15]);
            partial_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(60));
            send(s, 1, &b[15..]);
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
        for c in chunks(&p).0 {
            let ExecutionEvent::Output(observation) = channel.next_event().await.unwrap() else {
                panic!("missing output")
            };
            assert_eq!(observation.pod_uid(), "pod-uid");
            assert_eq!(observation.chunk(), &c);
        }
        let ExecutionEvent::Complete(observation) = channel.next_event().await.unwrap() else {
            panic!("missing final report")
        };
        assert_eq!(
            serde_json::from_slice::<Value>(observation.report_bytes()).unwrap(),
            chunks(&p).1
        );
        server.join().unwrap();
    }
}

#[tokio::test]
async fn invalid_chunks_are_rejected_before_they_become_observations() {
    for case in 0..5 {
        let p = plan(false);
        let mut replies = attached_replies(&p, move |s, p| {
            expect_grant(s, p);
            let mut c = chunks(p).0.remove(0);
            match case {
                0 => c.sequence += 1,
                1 => c.startup_grant_digest = format!("sha256:{}", "d".repeat(64)),
                2 => c.offset += 1,
                3 => c.bytes = vec![0; 11],
                _ => c.previous_digest = format!("sha256:{}", "d".repeat(64)),
            }
            send(s, 1, &bytes(c));
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
        assert_eq!(
            channel.next_event().await.unwrap_err(),
            Error::MutationUnconfirmed
        );
        drop(channel);
        server.join().unwrap();
    }
}

#[tokio::test]
async fn final_report_cannot_replace_stream_bytes_or_claim_missing_chunks() {
    for case in 0..4 {
        let p = plan(true);
        let mut replies = attached_replies(&p, move |s, p| {
            expect_grant(s, p);
            let (chunks, mut report) = chunks(p);
            let mut b = bytes(chunks[0].clone());
            b.extend(bytes(chunks[1].clone()));
            match case {
                0 => report["report"]["stdout"]["bytes"][0] = json!(1),
                1 => report["stream"]["sequence"] = json!(3),
                2 => {
                    report.as_object_mut().unwrap().remove("stream");
                }
                _ => {
                    b.extend(bytes(chunks[0].clone()));
                }
            }
            b.extend(serde_json::to_vec(&report).unwrap());
            b.push(b'\n');
            send(s, 1, &b);
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
        for _ in 0..2 {
            assert!(matches!(
                channel.next_event().await.unwrap(),
                ExecutionEvent::Output(_)
            ));
        }
        assert_eq!(
            channel.next_event().await.unwrap_err(),
            Error::MutationUnconfirmed
        );
        drop(channel);
        server.join().unwrap();
    }
}
