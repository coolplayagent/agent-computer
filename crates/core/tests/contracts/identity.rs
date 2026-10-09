use super::*;

#[test]
fn untrusted_identifiers_and_non_sha256_digests_are_rejected() {
    for id in [
        "",
        "../file",
        "a/b",
        "a b",
        "a\nb",
        "用户",
        &"a".repeat(129),
    ] {
        assert_eq!(ComputerId::new(id), Err(Error::InvalidIdentifier));
    }
    assert!(ComputerId::new("cmp_01-A").is_ok());
    for text in [
        "",
        "sha256:ab",
        &format!("sha256:{}", "z".repeat(64)),
        &format!("sha256:{}", "é".repeat(32)),
    ] {
        assert_eq!(InputDigest::parse(text), Err(Error::InvalidDigest));
    }
    assert_eq!(digest('a'), digest('A'));
}

#[test]
fn generations_revisions_and_epochs_never_wrap() {
    assert_eq!(
        Generation::from_u64(u64::MAX).next(),
        Err(Error::CounterExhausted)
    );
    assert_eq!(
        Revision::from_u64(u64::MAX).next(),
        Err(Error::CounterExhausted)
    );
    assert_eq!(
        LeaseEpoch::from_u64(u64::MAX).next(),
        Err(Error::CounterExhausted)
    );
    assert_eq!(Generation::INITIAL.next(), Ok(generation(1)));
}

#[test]
fn never_started_generation_cannot_own_a_lease_or_execution() {
    assert_eq!(
        Lease::new(
            OrganizationId::new("org_1").unwrap(),
            LeaseId::new("lease_a").unwrap(),
            ComputerId::new("cmp_1").unwrap(),
            Generation::INITIAL,
            LeaseScope::Modify(CandidateId::new("candidate_1").unwrap()),
        ),
        Err(Error::StaleGeneration)
    );
    assert_eq!(
        Execution::new(
            OrganizationId::new("org_1").unwrap(),
            ExecutionId::new("exec_1").unwrap(),
            ComputerId::new("cmp_1").unwrap(),
            Generation::INITIAL,
            digest('a'),
        ),
        Err(Error::StaleGeneration)
    );
}
