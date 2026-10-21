use super::*;

fn coded(code: &str, message: &str) -> SessionError {
    SessionError {
        message: message.to_string(),
        code: Some(code.to_string()),
    }
}

fn uncoded(message: &str) -> SessionError {
    SessionError::from(message)
}

#[test]
fn every_provider_code_maps_to_its_action() {
    for (code, expected) in [
        ("session_active", Some(RecoveryKind::EndFirst)),
        ("config_differs", Some(RecoveryKind::EndFirst)),
        ("session_ending", Some(RecoveryKind::Ending)),
        ("session_ended", Some(RecoveryKind::Recover)),
        ("pod_ended", Some(RecoveryKind::Recover)),
        ("pod_replaced", Some(RecoveryKind::Recover)),
        ("session_not_running", Some(RecoveryKind::Recover)),
        ("binding_expired", Some(RecoveryKind::Recover)),
        ("recovery_interrupted", Some(RecoveryKind::Recover)),
        ("no_checkpoint", Some(RecoveryKind::Recover)),
        ("no_ended_session", Some(RecoveryKind::Recover)),
        ("identity_mismatch", None),
        ("session_deleting", None),
        ("recovery_mode_conflict", None),
        ("unknown_future_code", None),
    ] {
        // The message names a different action: the code must decide.
        let error = coded(code, "end it first; explicit recovery is required");
        assert_eq!(recovery_kind(&error), expected, "{code}");
    }
}

#[test]
fn phrases_decide_only_when_there_is_no_code() {
    for (message, expected) in [
        (
            "A live session exists. End it first.",
            Some(RecoveryKind::EndFirst),
        ),
        (
            "end the session before recovering",
            Some(RecoveryKind::EndFirst),
        ),
        ("The session is ending", Some(RecoveryKind::Ending)),
        ("the session is still ending", Some(RecoveryKind::Ending)),
        ("Explicit recovery is required", Some(RecoveryKind::Recover)),
        (
            "recover it explicitly once it has ended",
            Some(RecoveryKind::Recover),
        ),
        ("The Session Ended", Some(RecoveryKind::Recover)),
        ("or start fresh instead", Some(RecoveryKind::Recover)),
        ("provider unavailable", None),
        ("", None),
    ] {
        assert_eq!(recovery_kind(&uncoded(message)), expected, "{message:?}");
    }
}

#[test]
fn checkpoint_is_hidden_when_there_is_nothing_to_restore() {
    for (error, expected) in [
        (coded("no_checkpoint", "anything"), false),
        (coded("no_ended_session", "anything"), false),
        (coded("session_ended", "no verified checkpoint"), true),
        (coded("pod_ended", "pod ended"), true),
        (uncoded("The session has no verified checkpoint"), false),
        (uncoded("No ended session exists for this thread"), false),
        (uncoded("session ended"), true),
    ] {
        assert_eq!(offers_checkpoint(&error), expected, "{error:?}");
    }
}

fn session(root: char) -> SessionScope {
    SessionScope {
        channel_id: uuid::Uuid::from_u128(7),
        thread_root: root.to_string().repeat(64),
    }
}

fn prompt(agent: &str, root: char, message: &str) -> RecoveryPrompt {
    RecoveryPrompt {
        agent_pubkey: agent.to_string(),
        agent_name: "Agent".to_string(),
        session: session(root),
        expected_relay_url: "wss://tenant-a.example".to_string(),
        expected_signer_pubkey: "aa11".to_string(),
        error: uncoded(message),
    }
}

#[test]
fn a_newer_failure_replaces_the_prompt_for_the_same_key() {
    let mut prompts = RecoveryPrompts::default();
    prompts.record(prompt("agent-1", 'a', "first"));
    prompts.record(prompt("agent-2", 'a', "other agent"));
    let mut newer = prompt(" AGENT-1 ", 'a', "second");
    newer.expected_relay_url = " wss://tenant-a.example ".to_string();
    newer.expected_signer_pubkey = "AA11".to_string();
    prompts.record(newer);

    let messages: Vec<&str> = prompts.iter().map(|p| p.error.message.as_str()).collect();
    assert_eq!(messages, ["other agent", "second"], "newest last");
}

#[test]
fn the_key_separates_tenants_agents_and_threads() {
    let base = prompt("agent-1", 'a', "base");
    let mut other_relay = base.clone();
    other_relay.expected_relay_url = "wss://tenant-b.example".to_string();
    let mut other_signer = base.clone();
    other_signer.expected_signer_pubkey = "bb22".to_string();
    let mut other_channel = base.clone();
    other_channel.session.channel_id = uuid::Uuid::from_u128(8);
    let distinct = [
        other_relay,
        other_signer,
        prompt("agent-2", 'a', "agent"),
        prompt("agent-1", 'b', "thread"),
        other_channel,
    ];
    let mut prompts = RecoveryPrompts::default();
    prompts.record(base.clone());
    for prompt in distinct {
        assert_ne!(prompt.key(), base.key());
        prompts.record(prompt);
    }
    assert_eq!(prompts.iter().count(), 6);
}

#[test]
fn the_store_keeps_only_the_newest_prompts() {
    let mut prompts = RecoveryPrompts::default();
    for index in 0..=MAX_RECOVERY_PROMPTS {
        prompts.record(prompt(&format!("agent-{index}"), 'a', &index.to_string()));
    }
    assert_eq!(prompts.iter().count(), MAX_RECOVERY_PROMPTS);
    assert_eq!(prompts.iter().next().unwrap().agent_pubkey, "agent-1");
    assert_eq!(
        prompts.iter().last().unwrap().agent_pubkey,
        format!("agent-{MAX_RECOVERY_PROMPTS}")
    );

    // Replacing an existing key at the bound drops nothing else.
    prompts.record(prompt("agent-5", 'a', "replaced"));
    assert_eq!(prompts.iter().count(), MAX_RECOVERY_PROMPTS);
    assert_eq!(prompts.iter().next().unwrap().agent_pubkey, "agent-1");
    assert_eq!(prompts.iter().last().unwrap().error.message, "replaced");
}

#[test]
fn clear_and_thread_selection_use_the_key() {
    let mut prompts = RecoveryPrompts::default();
    let first = prompt("agent-1", 'a', "first");
    prompts.record(first.clone());
    prompts.record(prompt("agent-2", 'a', "second"));
    prompts.record(prompt("agent-1", 'b', "elsewhere"));

    let thread_a = session('a');
    let thread: Vec<&str> = prompts
        .for_thread(&thread_a)
        .map(|p| p.error.message.as_str())
        .collect();
    assert_eq!(thread, ["first", "second"]);

    let key = RecoveryPromptKey::new(
        "wss://tenant-a.example ",
        " AA11",
        "Agent-1",
        &first.session,
    );
    assert!(prompts.clear(&key));
    assert!(!prompts.clear(&key), "already cleared");
    let remaining: Vec<&str> = prompts.iter().map(|p| p.error.message.as_str()).collect();
    assert_eq!(remaining, ["second", "elsewhere"]);
}
