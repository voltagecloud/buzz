use super::*;

#[tokio::test(start_paused = true)]
async fn retry_reuses_the_replacement_after_a_lost_create_response() {
    assert_recovery_retry(true).await;
}

#[tokio::test(start_paused = true)]
async fn retry_finishes_binding_the_replacement_after_a_secret_write_failure() {
    assert_recovery_retry(false).await;
}

async fn assert_recovery_retry(lost_create_response: bool) {
    for mode in [RecoveryMode::Checkpoint, RecoveryMode::Fresh] {
        let ended = Ended::new().await;
        let (state, identity, cfg, name) = (&ended.state, &ended.identity, &ended.cfg, &ended.name);
        if lost_create_response {
            state.lock().unwrap().fail_sandbox_create_responses = 1;
        } else {
            state.lock().unwrap().fail_binding_secret_creates = 1;
        }
        let recovery = fake_scope_with(Some(mode));
        let error = ended.deploy(Some(mode)).await.unwrap_err();
        assert!(
            error.message.contains(if lost_create_response {
                "create recovered Sandbox"
            } else {
                "retry deploy"
            }),
            "{error}"
        );
        let replacement = ended.sandbox();
        let pod_uid = state.lock().unwrap().pods[name].clone();
        let journal = ended.journal().unwrap();

        // A journal alone must not authorize reuse of an unrelated generation.
        state.lock().unwrap().sandboxes.get_mut(name).unwrap()["metadata"]["annotations"]
            [RECOVERED_FROM] = "unrelated-generation".into();
        let unrelated = ended.deploy(Some(mode)).await.unwrap_err();
        assert_eq!(unrelated.code, Some(ErrorCode::SessionActive));
        state
            .lock()
            .unwrap()
            .sandboxes
            .insert(name.clone(), replacement.clone());
        let other_owner = deploy(
            fake::client(state),
            identity,
            cfg,
            &recovery,
            "another-owner",
            plain_env(),
        )
        .await;
        assert!(other_owner.is_err());
        let mut changed_cfg = cfg.clone();
        changed_cfg.pod_options.active_deadline_seconds = Some(3601);
        let changed = deploy_to(state, identity, &changed_cfg, &recovery).await;
        assert_eq!(changed.unwrap_err().code, Some(ErrorCode::ConfigDiffers));

        assert_eq!(ended.deploy(Some(mode)).await.unwrap(), *name);
        let resumed = ended.sandbox();
        for field in ["uid", "name"] {
            assert_eq!(resumed["metadata"][field], replacement["metadata"][field]);
        }
        let annotations = &resumed["metadata"]["annotations"];
        assert_eq!(
            annotations[GENERATION],
            replacement["metadata"]["annotations"][GENERATION]
        );
        assert_eq!(annotations[RECOVERED_FROM], ended.previous);
        assert_eq!(state.lock().unwrap().pods[name], pod_uid);
        assert!(ended.journal().is_none());
        let restored = match mode {
            RecoveryMode::Checkpoint => ended.key.clone(),
            RecoveryMode::Fresh => String::new(),
        };
        assert_eq!(secret_env(state, RESTORE_KEY_ENV), [restored]);
        assert_eq!(
            ended.deploy(Some(mode)).await.unwrap_err().code,
            Some(ErrorCode::SessionActive),
            "a completed recovery must not authorize another replacement"
        );

        // A stale journal must not strand a replacement that has since ended.
        let journal_name = format!("{name}-recovery");
        state.lock().unwrap().secrets.insert(journal_name, journal);
        tombstone(state, name, "missing");
        ended.deploy(Some(RecoveryMode::Fresh)).await.unwrap();
        assert_ne!(
            ended.sandbox()["metadata"]["uid"],
            resumed["metadata"]["uid"]
        );
        assert!(ended.journal().is_none());
    }
}

/// Binding a half-finished replacement for the other mode would clear the
/// journal: a checkpoint request would get an empty workspace and lose the
/// last record of its key.
#[tokio::test(start_paused = true)]
async fn a_retry_in_the_other_mode_is_refused_and_keeps_the_journal() {
    use RecoveryMode::{Checkpoint, Fresh};
    for (first, retry) in [(Checkpoint, Fresh), (Fresh, Checkpoint)] {
        let ended = Ended::new().await;
        ended.state.lock().unwrap().fail_binding_secret_creates = 1;
        ended.deploy(Some(first)).await.unwrap_err();
        let (replacement, journal) = (ended.sandbox(), ended.journal());
        assert!(journal.is_some());

        let error = ended.deploy(Some(retry)).await.unwrap_err();
        assert_eq!(
            error.code,
            Some(ErrorCode::RecoveryModeConflict),
            "{first:?} then {retry:?}: {error}"
        );
        assert_eq!(ended.sandbox(), replacement);
        assert_eq!(ended.journal(), journal);
        assert_eq!(
            ended.state.lock().unwrap().secrets.len(),
            1,
            "nothing but the journal"
        );

        // The recovery already underway still finishes.
        ended.deploy(Some(first)).await.unwrap();
        assert!(ended.journal().is_none());
    }
}

/// A Sandbox that appears under the name while the tombstone is deleted is
/// joined only when it is the same recovery of the same tombstone.
#[tokio::test(start_paused = true)]
async fn a_concurrent_replacement_is_joined_only_for_the_same_recovery() {
    for (recovered_from, restores, expected) in [
        (true, true, None),
        (false, true, Some(ErrorCode::SessionReplaced)),
        (true, false, Some(ErrorCode::RecoveryModeConflict)),
    ] {
        let ended = Ended::new().await;
        let mut peer = ended.sandbox();
        peer["spec"]["operatingMode"] = "Running".into();
        let generation = "peer-generation";
        peer["spec"]["podTemplate"]["spec"]["containers"][0]["envFrom"][0]["secretRef"]["name"] =
            ended.identity.secret_name(generation).into();
        let annotations = peer["metadata"]["annotations"].as_object_mut().unwrap();
        for key in [LIFECYCLE, ENDED_REASON, CHECKPOINT, INITIAL_POD] {
            annotations.remove(key);
        }
        annotations.insert(GENERATION.into(), generation.into());
        let previous = if recovered_from {
            ended.previous.as_str()
        } else {
            "another-generation"
        };
        annotations.insert(RECOVERED_FROM.into(), previous.into());
        if restores {
            annotations.insert(RESTORE_CHECKPOINT.into(), ended.key.clone().into());
        }
        ended.state.lock().unwrap().peer_sandbox = Some(peer);

        let result = ended.deploy(Some(RecoveryMode::Checkpoint)).await;
        assert_eq!(result.as_ref().err().and_then(|e| e.code), expected);
        assert_eq!(
            ended.sandbox()["metadata"]["annotations"][GENERATION],
            generation
        );
        assert_eq!(ended.journal().is_none(), expected.is_none());
        if expected.is_none() {
            assert_eq!(secret_env(&ended.state, RESTORE_KEY_ENV), [ended.key]);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_foreign_pod_under_the_name_stops_recovery_and_keeps_the_journal() {
    let ended = Ended::new().await;
    ended.state.lock().unwrap().lingering_pods.insert(
        ended.name.clone(),
        json!({"apiVersion": "v1", "kind": "Pod", "metadata": {
            "name": ended.name, "uid": "foreign-pod",
            "ownerReferences": [{"apiVersion": "agents.x-k8s.io/v1beta1",
                "kind": "Sandbox", "name": ended.name, "uid": "another-sandbox",
                "controller": true}]}}),
    );
    let error = ended
        .deploy(Some(RecoveryMode::Checkpoint))
        .await
        .unwrap_err();
    assert_eq!(error.code, Some(ErrorCode::ForeignPod), "{error}");
    assert!(!ended
        .state
        .lock()
        .unwrap()
        .sandboxes
        .contains_key(&ended.name));
    assert!(ended.journal().is_some());
}

/// The tombstone is deleted only in the version that was journaled.
#[tokio::test(start_paused = true)]
async fn a_tombstone_changed_before_its_delete_is_kept() {
    let ended = Ended::new().await;
    let uid = ended.sandbox()["metadata"]["uid"].clone();
    ended.state.lock().unwrap().touch_before_delete = 1;
    let error = ended
        .deploy(Some(RecoveryMode::Checkpoint))
        .await
        .unwrap_err();
    assert!(error.message.contains("changed during recovery"), "{error}");
    assert_eq!(ended.sandbox()["metadata"]["uid"], uid);
    assert_eq!(
        ended.sandbox()["metadata"]["annotations"][LIFECYCLE],
        LIFECYCLE_ENDED
    );
    assert!(ended.journal().is_some());
    ended.deploy(Some(RecoveryMode::Checkpoint)).await.unwrap();
}

/// A journal that records a different checkpoint for the same tombstone is
/// never overwritten, and the tombstone is kept.
#[tokio::test(start_paused = true)]
async fn a_journal_that_disagrees_with_its_tombstone_stops_recovery() {
    let ended = Ended::new().await;
    let tombstone = ended.sandbox();
    ended.state.lock().unwrap().fail_sandbox_creates = 1;
    ended
        .deploy(Some(RecoveryMode::Checkpoint))
        .await
        .unwrap_err();
    let journal = ended.journal();
    let mut changed = tombstone;
    changed["metadata"]["annotations"][CHECKPOINT] = lifecycle::CHECKPOINT_MISSING.into();
    ended
        .state
        .lock()
        .unwrap()
        .sandboxes
        .insert(ended.name.clone(), changed.clone());
    let error = ended.deploy(Some(RecoveryMode::Fresh)).await.unwrap_err();
    assert!(error.message.contains("disagrees"), "{error}");
    assert_eq!(ended.sandbox(), changed);
    assert_eq!(ended.journal(), journal);
}
