use super::*;
use serde_json::json;
use std::cell::{Cell, RefCell};

fn inventory() -> Snapshot {
    let scope = Scope {
        owner: "ab".repeat(32),
        relay: "wss://relay.example".into(),
    };
    let agents = (1..=3).map(|id| Agent {
        pubkey: format!("{id:064x}"), name: format!("Agent {id}"),
        agent_type: if id == 1 { "buzz.codex/codex" } else { "buzz.claude-code/claude-code" }.into(),
        owner: scope.owner.clone(), relay: scope.relay.clone(), picture: Some("https://example.com/avatar.png".into()),
        attention: Default::default(), skipped: Default::default(), timers: Default::default(), attention_enabled: true,
        config: json!({"instructions": format!("Role {id}"), "workspace": "/local/project", "model": format!("model-{id}"), "effort": "high", "fast": true}),
    }).collect::<Vec<_>>();
    let teams = vec![Team {
        id: "engineering".into(),
        name: "Engineering".into(),
        instructions: "Work together".into(),
        members: agents.iter().map(|agent| agent.pubkey.clone()).collect(),
    }];
    Snapshot {
        scope,
        ready: true,
        agents,
        teams,
    }
}

fn review(
    snapshot: &Snapshot,
    keys: BTreeSet<String>,
    team: Option<&str>,
    reference: Option<&str>,
) -> Plan {
    Plan::review(
        snapshot,
        &keys,
        team,
        reference,
        &defaults(&snapshot.scope),
        |key| {
            Ok(RetypePreview {
                config: json!({"instructions": snapshot.agent(key)?.config["instructions"]}),
                moved: vec![CarryKey::Instructions],
                dropped: vec![CarryKey::Workspace],
                settings: vec![],
            })
        },
    )
    .unwrap()
}

fn selected(snapshot: &Snapshot) -> BTreeSet<String> {
    snapshot
        .agents
        .iter()
        .map(|agent| agent.pubkey.clone())
        .collect()
}

#[derive(Default)]
struct MemoryJournal {
    fail: Cell<bool>,
    saved: RefCell<Option<Plan>>,
}
impl Journal for MemoryJournal {
    fn save(&self, plan: &Plan) -> Result<(), String> {
        if self.fail.get() {
            return Err("disk unavailable".into());
        }
        *self.saved.borrow_mut() = Some(plan.clone());
        Ok(())
    }
}

#[test]
fn whole_team_and_arbitrary_subset_keep_each_agents_own_options() {
    let snapshot = inventory();
    let plan = review(&snapshot, selected(&snapshot), Some("engineering"), None);
    assert_eq!(plan.items.len(), 3);
    assert!(plan.roster.is_some());
    for (index, item) in plan.items.iter().enumerate() {
        let after = Config::read(&item.after);
        assert_eq!(after.instructions, format!("Role {}", index + 1));
        assert_eq!(after.model, format!("model-{}", index + 1));
        assert_eq!(after.runtime, if index == 0 { "codex" } else { "claude" });
        assert_eq!(after.effort, "high");
        assert!(after.fast);
        assert!(after.env.is_empty());
        assert!(
            item.warnings
                .iter()
                .any(|warning| warning.contains("local"))
        );
    }
    let subset = [
        snapshot.agents[0].pubkey.clone(),
        snapshot.agents[2].pubkey.clone(),
    ]
    .into();
    let plan = review(&snapshot, subset, None, None);
    assert_eq!(plan.items.len(), 2);
    assert_eq!(plan.items[1].before.pubkey, snapshot.agents[2].pubkey);
}

#[test]
fn reference_shares_destination_without_copying_secrets_or_instructions() {
    let mut snapshot = inventory();
    let mut destination = defaults(&snapshot.scope);
    destination.env.insert("SECRET".into(), "private".into());
    destination.instructions = "Reference role".into();
    snapshot.agents[2].agent_type = TARGET.into();
    snapshot.agents[2].config = destination.value();
    let plan = Plan::review(
        &snapshot,
        &selected(&snapshot),
        Some("engineering"),
        Some(&snapshot.agents[2].pubkey),
        &destination,
        |key| {
            Ok(RetypePreview {
                config: json!({"instructions": snapshot.agent(key)?.config["instructions"]}),
                moved: vec![CarryKey::Instructions],
                dropped: vec![],
                settings: vec![],
            })
        },
    )
    .unwrap();
    assert_eq!(plan.items.len(), 2);
    assert_eq!(plan.already_remote, vec!["Agent 3"]);
    assert_eq!(
        plan.reference.as_ref().unwrap().pubkey,
        snapshot.agents[2].pubkey
    );
    assert!(Config::read(&plan.items[0].after).env.is_empty());
    assert_eq!(Config::read(&plan.items[0].after).instructions, "Role 1");
}

#[test]
fn journal_failure_and_stale_state_prevent_the_production_mutation_closure() {
    let snapshot = inventory();
    let original = review(&snapshot, selected(&snapshot), Some("engineering"), None);
    let journal = MemoryJournal::default();
    let called = Cell::new(false);
    journal.fail.set(true);
    assert!(
        checked_step(&mut original.clone(), &snapshot, &journal, |_, _| called
            .set(true))
        .is_err()
    );
    assert!(!called.get());
    journal.fail.set(false);
    for variant in 0..7 {
        let mut changed = inventory();
        match variant {
            0 => changed.scope.owner = "cd".repeat(32),
            1 => changed.scope.relay = "wss://other.example".into(),
            2 => changed.agents[1].config["instructions"] = "Edited after review".into(),
            3 => {
                changed.teams[0].members.pop();
            }
            4 => changed.ready = false,
            5 => {
                changed.agents.remove(1);
            }
            _ => changed.agents[1].owner = "cd".repeat(32),
        }
        assert!(
            checked_step(&mut original.clone(), &changed, &journal, |_, _| called
                .set(true))
            .is_err(),
            "variant {variant}"
        );
        assert!(!called.get());
    }
    assert!(matches!(
        checked_step(&mut original.clone(), &snapshot, &journal, |_, _| {
            assert!(journal.saved.borrow().is_some());
            called.set(true)
        })
        .unwrap(),
        Step::Started(0, ())
    ));
    assert!(called.get());
}

#[test]
fn resume_reconciles_a_lost_ack_and_moves_only_remaining_agents() {
    let mut snapshot = inventory();
    let mut plan = review(&snapshot, selected(&snapshot), None, None);
    let journal = MemoryJournal::default();
    snapshot.agents[0].agent_type = TARGET.into();
    snapshot.agents[0].config = plan.items[0].after.clone();
    assert!(matches!(
        checked_step(&mut plan, &snapshot, &journal, |_, _| panic!(
            "must not repeat applied change"
        ))
        .unwrap(),
        Step::Reconciled
    ));
    assert_eq!(
        journal.saved.borrow().as_ref().unwrap().completed,
        [0].into()
    );
    // Live attention and profile metadata are not overwritten by the batch.
    snapshot.agents[1].attention_enabled = false;
    snapshot.agents[1].picture = None;
    assert!(matches!(
        checked_step(&mut plan, &snapshot, &journal, |item, _| {
            assert_eq!(item.before.pubkey, snapshot.agents[1].pubkey);
        })
        .unwrap(),
        Step::Started(1, ())
    ));
    snapshot.agents[0].config["instructions"] = "Changed after moving".into();
    assert!(
        checked_step(&mut plan, &snapshot, &journal, |_, _| panic!(
            "stale completed source"
        ))
        .is_err()
    );
}

#[test]
fn each_success_is_resumable_and_completion_is_durable() {
    let mut snapshot = inventory();
    let mut plan = review(&snapshot, selected(&snapshot), None, None);
    let journal = MemoryJournal::default();
    for index in 0..3 {
        let step = checked_step(&mut plan, &snapshot, &journal, |item, operation| {
            assert_eq!(operation, Change::Retype);
            (item.before.pubkey.clone(), item.after.clone())
        })
        .unwrap();
        let Step::Started(actual, (key, config)) = step else {
            panic!("expected next source");
        };
        assert_eq!(actual, index);
        let agent = snapshot
            .agents
            .iter_mut()
            .find(|agent| agent.pubkey == key)
            .unwrap();
        agent.agent_type = TARGET.into();
        agent.config = config;
        // Recover from the saved pre-request record, as on app restart.
        plan = journal.saved.borrow().clone().unwrap();
        assert!(matches!(
            checked_step(&mut plan, &snapshot, &journal, |_, _| panic!(
                "already applied"
            ))
            .unwrap(),
            Step::Reconciled
        ));
    }
    journal.fail.set(true);
    assert!(checked_step(&mut plan, &snapshot, &journal, |_, _| panic!("all applied")).is_err());
    assert_eq!(plan.state, State::Active);
    journal.fail.set(false);
    assert!(matches!(
        checked_step(&mut plan, &snapshot, &journal, |_, _| panic!("all applied")).unwrap(),
        Step::Complete
    ));
    assert_eq!(
        journal.saved.borrow().as_ref().unwrap().state,
        State::Complete
    );
}

#[test]
fn interrupted_type_write_finishes_only_the_exact_reviewed_config() {
    let mut snapshot = inventory();
    let mut plan = review(&snapshot, selected(&snapshot), None, None);
    snapshot.agents[0].agent_type = TARGET.into();
    let journal = MemoryJournal::default();
    assert!(matches!(
        checked_step(&mut plan, &snapshot, &journal, |item, operation| {
            assert_eq!(operation, Change::FinishConfig);
            assert_eq!(item.before.pubkey, snapshot.agents[0].pubkey);
            assert!(journal.saved.borrow().is_some());
        })
        .unwrap(),
        Step::Started(0, ())
    ));
    snapshot.agents[0].config["instructions"] = "Newer edit".into();
    assert!(
        checked_step(&mut plan, &snapshot, &journal, |_, _| panic!(
            "must preserve newer edit"
        ))
        .is_err()
    );
}

#[test]
fn reference_changes_cancelled_batches_and_instruction_loss_are_rejected() {
    let mut snapshot = inventory();
    snapshot.agents[2].agent_type = TARGET.into();
    snapshot.agents[2].config = defaults(&snapshot.scope).value();
    let mut plan = review(
        &snapshot,
        selected(&snapshot),
        None,
        Some(&snapshot.agents[2].pubkey),
    );
    snapshot.agents[2].config["model"] = "new-model".into();
    assert!(
        checked_step(
            &mut plan,
            &snapshot,
            &MemoryJournal::default(),
            |_, _| panic!("reference changed")
        )
        .is_err()
    );
    plan.state = State::Cancelled;
    assert!(matches!(
        checked_step(
            &mut plan,
            &snapshot,
            &MemoryJournal::default(),
            |_, _| panic!("cancelled")
        )
        .unwrap(),
        Step::Complete
    ));
    let snapshot = inventory();
    let result = Plan::review(
        &snapshot,
        &selected(&snapshot),
        None,
        None,
        &defaults(&snapshot.scope),
        |_| {
            Ok(RetypePreview {
                config: json!({}),
                moved: vec![],
                dropped: vec![],
                settings: vec![],
            })
        },
    );
    assert!(
        result
            .unwrap_err()
            .contains("instructions cannot be preserved")
    );
}

#[test]
fn bounded_atomic_private_journal_restores_pending_work_and_refuses_corruption() {
    let snapshot = inventory();
    let mut plan = review(&snapshot, selected(&snapshot), None, None);
    let directory = tempfile::tempdir().unwrap();
    let journal = FileJournal::new(directory.path().into(), &snapshot.scope);
    assert!(journal.load(&snapshot.scope).unwrap().is_none());
    journal.save(&plan).unwrap();
    assert_eq!(
        journal.load(&snapshot.scope).unwrap().unwrap().items.len(),
        3
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&journal.0).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    plan.completed.insert(0);
    journal.save(&plan).unwrap();
    assert_eq!(
        journal.load(&snapshot.scope).unwrap().unwrap().completed,
        [0].into()
    );
    let other = Scope {
        owner: "cd".repeat(32),
        ..snapshot.scope.clone()
    };
    assert!(journal.load(&other).is_err());
    plan.completed.insert(999);
    journal.save(&plan).unwrap();
    assert!(journal.load(&snapshot.scope).is_err());
    std::fs::write(&journal.0, b"{").unwrap();
    assert!(journal.load(&snapshot.scope).is_err());
    journal.archive().unwrap();
    assert!(!journal.0.exists());
    assert!(std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("remote-agent-migration-archived-")
    }));
    std::fs::write(&journal.0, vec![b' '; JOURNAL_LIMIT as usize + 1]).unwrap();
    assert!(journal.load(&snapshot.scope).is_err());
    plan.completed.clear();
    plan.state = State::Complete;
    journal.save(&plan).unwrap();
    assert!(journal.load(&snapshot.scope).unwrap().is_none());
    let oversized = (0..=LIMIT).map(|id| format!("{id:064x}")).collect();
    assert!(
        Plan::review(
            &snapshot,
            &oversized,
            None,
            None,
            &defaults(&snapshot.scope),
            |_| panic!("too many agents")
        )
        .is_err()
    );
}
