use super::*;

#[test]
fn lease_expiry_never_hands_control_to_a_second_owner() {
    let mut l = lease("lease_gui");
    let a = l
        .acquire(owner("alice"), generation(1), 100, 30_000)
        .unwrap();
    assert_eq!(l.authorize(&a, generation(1), 30_099), Ok(()));
    assert_eq!(
        l.authorize(&a, generation(1), 30_100),
        Err(Error::LeaseExpired)
    );
    assert_eq!(
        l.renew(&a, generation(1), 30_100, 30_000),
        Err(Error::LeaseExpired)
    );
    assert_eq!(
        l.acquire(owner("bob"), generation(1), 40_000, 30_000),
        Err(Error::LeaseBusy)
    );
    l.expire(40_000).unwrap();
    assert_eq!(l.state(), LeaseState::Draining);
    assert_eq!(
        l.acquire(owner("bob"), generation(1), 40_000, 30_000),
        Err(Error::LeaseBusy)
    );
    l.confirm_drained(DrainEvidence {
        token: a.clone(),
        evidence_id: evidence("drained"),
    })
    .unwrap();
    let b = l
        .acquire(owner("bob"), generation(1), 40_000, 30_000)
        .unwrap();
    assert_eq!(b.epoch().value(), 2);
    assert_eq!(b.owner().as_str(), "bob");
    assert_eq!(
        l.authorize(&a, generation(1), 40_001),
        Err(Error::LeaseMismatch)
    );
    assert_eq!(l.authorize(&b, generation(1), 40_001), Ok(()));
    assert_eq!(l.last_drain().unwrap().token, a);
}

#[test]
fn voluntary_handoff_rejects_inputs_until_drained_and_rejects_stale_proof() {
    let mut l = lease("lease_gui");
    let a = l.acquire(owner("alice"), generation(1), 0, 30_000).unwrap();
    let proof = DrainEvidence {
        token: a.clone(),
        evidence_id: evidence("drained_a"),
    };
    assert_eq!(
        l.confirm_drained(proof.clone()),
        Err(Error::InvalidTransition)
    );
    l.begin_release(&a).unwrap();
    assert_eq!(l.authorize(&a, generation(1), 1), Err(Error::LeaseBusy));
    assert_eq!(l.renew(&a, generation(1), 1, 30_000), Err(Error::LeaseBusy));
    l.confirm_drained(proof.clone()).unwrap();
    let b = l.acquire(owner("bob"), generation(1), 1, 30_000).unwrap();
    l.begin_release(&b).unwrap();
    let snapshot = l.clone();
    assert_eq!(l.confirm_drained(proof), Err(Error::LeaseMismatch));
    assert_eq!(l, snapshot);
}

#[test]
fn lease_tokens_cannot_cross_scope_or_runtime_generation() {
    let mut a = lease("lease_a");
    let mut b = lease("lease_b");
    let ta = a.acquire(owner("alice"), generation(1), 0, 100).unwrap();
    b.acquire(owner("alice"), generation(1), 0, 100).unwrap();
    assert_eq!(
        b.authorize(&ta, generation(1), 0),
        Err(Error::LeaseMismatch)
    );
    assert_eq!(
        a.authorize(&ta, generation(2), 0),
        Err(Error::StaleGeneration)
    );
    assert_eq!(
        a.renew(&ta, generation(2), 0, 100),
        Err(Error::StaleGeneration)
    );
    assert_eq!(a.expires_at_ms(), 100);
}

#[test]
fn invalid_or_overflowing_lease_deadlines_do_not_acquire_or_mutate() {
    for (now, duration, error) in [
        (0, 0, Error::InvalidDuration),
        (0, 30_001, Error::InvalidDuration),
        (u64::MAX, 1, Error::CounterExhausted),
    ] {
        let mut l = lease("lease_a");
        let before = l.clone();
        assert_eq!(
            l.acquire(owner("alice"), generation(1), now, duration),
            Err(error)
        );
        assert_eq!(l, before);
    }
    let mut l = lease("lease_a");
    let a = l.acquire(owner("alice"), generation(1), 0, 100).unwrap();
    assert_eq!(l.expire(99), Err(Error::InvalidTransition));
    l.renew(&a, generation(1), 99, 30_000).unwrap();
    assert_eq!(l.expires_at_ms(), 30_099);
    let before = l.clone();
    assert_eq!(
        l.renew(&a, generation(1), 100, 0),
        Err(Error::InvalidDuration)
    );
    assert_eq!(l, before);
}

#[test]
fn tokens_are_bound_to_organization_even_if_storage_is_misconfigured() {
    let mut a = lease("lease_a");
    let mut b = Lease::new(
        OrganizationId::new("org_other").unwrap(),
        LeaseId::new("lease_a").unwrap(),
        ComputerId::new("cmp_1").unwrap(),
        generation(1),
        LeaseScope::Gui(AppId::new("browser_1").unwrap()),
    )
    .unwrap();
    let ta = a.acquire(owner("alice"), generation(1), 0, 100).unwrap();
    b.acquire(owner("alice"), generation(1), 0, 100).unwrap();
    assert_eq!(
        b.authorize(&ta, generation(1), 0),
        Err(Error::LeaseMismatch)
    );
}
