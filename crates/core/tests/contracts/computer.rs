use super::*;

#[test]
fn computer_restart_retains_identity_and_workspace_but_changes_generation() {
    let mut c = ready_computer();
    let original = c.clone();
    c.stop(c.revision(), StopMode::Normal, Activity::default())
        .unwrap();
    assert_eq!(
        (c.desired(), c.observed()),
        (DesiredState::Stopped, ObservedState::Draining)
    );
    c.complete_stop(c.revision(), fence(&c), checkpoint())
        .unwrap();
    assert_eq!(c.observed(), ObservedState::Stopped);
    assert_eq!(c.generation(), original.generation());
    assert_eq!(c.checkpoint().unwrap().as_str(), "cp_1");
    let next = c.start(c.revision()).unwrap();
    assert_eq!(next.value(), 2);
    assert_eq!(c.id(), original.id());
    assert_eq!(c.organization_id(), original.organization_id());
    assert_eq!(c.workspace_id(), original.workspace_id());
    let before = c.clone();
    assert_eq!(
        c.ready(c.revision(), original.generation()),
        Err(Error::StaleGeneration)
    );
    assert_eq!(c, before);
}

#[test]
fn start_is_idempotent_without_allocating_another_generation() {
    let mut c = computer();
    assert_eq!(c.start(c.revision()), Ok(generation(1)));
    let snapshot = c.clone();
    assert_eq!(c.start(c.revision()), Ok(generation(1)));
    assert_eq!(c, snapshot);
    c.ready(c.revision(), generation(1)).unwrap();
    let snapshot = c.clone();
    assert_eq!(c.start(c.revision()), Ok(generation(1)));
    assert_eq!(c, snapshot);
}

#[test]
fn revision_precondition_applies_even_to_idempotent_start() {
    let mut c = ready_computer();
    let snapshot = c.clone();
    assert_eq!(c.start(Revision::INITIAL), Err(Error::RevisionConflict));
    assert_eq!(c, snapshot);
    assert!(c.revision().value() > c.generation().value());
}

#[test]
fn ordinary_stop_protects_people_and_presentations() {
    let mut c = ready_computer();
    let snapshot = c.clone();
    let active = Activity {
        human_or_presentation: true,
        ..Activity::default()
    };
    assert_eq!(
        c.stop(c.revision(), StopMode::Normal, active),
        Err(Error::ActiveUse)
    );
    assert_eq!(c, snapshot);
    c.stop(c.revision(), StopMode::Force, active).unwrap();
    assert_eq!(c.observed(), ObservedState::Draining);
}

#[test]
fn idle_stop_requires_every_activity_class_to_be_clear() {
    for mask in 1..16 {
        let mut c = ready_computer();
        let snapshot = c.clone();
        let active = Activity {
            human_or_presentation: mask & 1 != 0,
            execution: mask & 2 != 0,
            lease: mask & 4 != 0,
            keep_running: mask & 8 != 0,
        };
        assert_eq!(
            c.stop(c.revision(), StopMode::Idle, active),
            Err(Error::ActiveUse),
            "mask {mask}"
        );
        assert_eq!(c, snapshot);
    }
    let mut c = ready_computer();
    c.stop(c.revision(), StopMode::Idle, Activity::default())
        .unwrap();
    assert_eq!(c.observed(), ObservedState::Draining);
}

#[test]
fn normal_stop_drains_executions_and_needs_a_committed_checkpoint() {
    let mut c = ready_computer();
    c.stop(
        c.revision(),
        StopMode::Normal,
        Activity {
            execution: true,
            ..Activity::default()
        },
    )
    .unwrap();
    let snapshot = c.clone();
    assert_eq!(
        c.complete_stop(
            c.revision(),
            fence(&c),
            CheckpointOutcome::Discarded(evidence("loss"))
        ),
        Err(Error::CheckpointRequired)
    );
    assert_eq!(c, snapshot);
    assert_eq!(c.start(c.revision()), Err(Error::InvalidTransition));
    c.complete_stop(c.revision(), fence(&c), checkpoint())
        .unwrap();
}

#[test]
fn force_stop_records_lost_state_and_preserves_last_valid_checkpoint() {
    let mut c = ready_computer();
    c.stop(c.revision(), StopMode::Normal, Activity::default())
        .unwrap();
    c.complete_stop(c.revision(), fence(&c), checkpoint())
        .unwrap();
    c.start(c.revision()).unwrap();
    c.stop(c.revision(), StopMode::Force, Activity::default())
        .unwrap();
    let loss = CheckpointOutcome::Discarded(evidence("unsaved_state"));
    c.complete_stop(c.revision(), fence(&c), loss.clone())
        .unwrap();
    assert_eq!(c.last_stop(), Some(&loss));
    assert_eq!(c.checkpoint().unwrap().as_str(), "cp_1");
    assert_eq!(c.last_fence().unwrap().generation, generation(2));
}

#[test]
fn stop_rejects_fencing_for_another_computer_or_generation() {
    for wrong_generation in [false, true] {
        let mut c = ready_computer();
        c.stop(c.revision(), StopMode::Force, Activity::default())
            .unwrap();
        let before = c.clone();
        let mut proof = fence(&c);
        let error = if wrong_generation {
            proof.generation = generation(0);
            Error::StaleGeneration
        } else {
            proof.computer_id = ComputerId::new("cmp_other").unwrap();
            Error::EvidenceMismatch
        };
        assert_eq!(
            c.complete_stop(c.revision(), proof, checkpoint()),
            Err(error)
        );
        assert_eq!(c, before);
    }
}

#[test]
fn partition_recovery_requires_fencing_before_a_new_generation() {
    let mut c = ready_computer();
    c.runtime_failed(c.revision(), c.generation(), true)
        .unwrap();
    assert_eq!(c.observed(), ObservedState::RecoveryBlocked);
    let blocked = c.clone();
    assert_eq!(c.start(c.revision()), Err(Error::InvalidTransition));
    assert_eq!(
        c.ready(c.revision(), c.generation()),
        Err(Error::InvalidTransition)
    );
    assert_eq!(c, blocked);
    let mut stale = fence(&c);
    stale.generation = generation(0);
    assert_eq!(c.recover(c.revision(), stale), Err(Error::StaleGeneration));
    assert_eq!(c, blocked);
    assert_eq!(c.recover(c.revision(), fence(&c)), Ok(generation(2)));
    assert_eq!(c.observed(), ObservedState::Starting);
}

#[test]
fn delete_is_a_stopped_computer_tombstone_that_retains_storage() {
    let mut c = ready_computer();
    assert_eq!(
        c.delete_stopped(c.revision()),
        Err(Error::InvalidTransition)
    );
    c.stop(c.revision(), StopMode::Normal, Activity::default())
        .unwrap();
    c.complete_stop(c.revision(), fence(&c), checkpoint())
        .unwrap();
    let workspace = c.workspace_id().clone();
    c.delete_stopped(c.revision()).unwrap();
    assert_eq!(c.desired(), DesiredState::Deleted);
    assert_eq!(c.workspace_id(), &workspace);
    let deleted = c.clone();
    assert_eq!(c.start(c.revision()), Err(Error::InvalidTransition));
    assert_eq!(
        c.stop(c.revision(), StopMode::Force, Activity::default()),
        Err(Error::InvalidTransition)
    );
    assert_eq!(c, deleted);
}
