use super::*;
use core::prelude::v1::test;

fn effort(id: &str, label: &str) -> Effort {
    Effort {
        id: id.into(),
        label: label.into(),
    }
}

fn model(id: &str, label: &str, efforts: &[(&str, &str)], resting: Option<&str>) -> Model {
    Model {
        id: id.into(),
        label: label.into(),
        efforts: efforts
            .iter()
            .map(|(id, label)| effort(id, label))
            .collect(),
        effort: resting.map(str::to_owned),
    }
}

/// Honey's models in the reference's composer tests.
fn fast_and_deep() -> Models {
    Models {
        current: "fast".into(),
        models: vec![
            model("fast", "Fast", &[("low", "Low"), ("high", "High")], None),
            model("deep", "Deep", &[("high", "High"), ("max", "Max")], None),
        ],
    }
}

fn choice(pubkey: &str, name: &str, models: Models) -> Choice {
    Choice {
        pubkey: pubkey.into(),
        name: name.into(),
        models,
        loading: false,
    }
}

fn pick(model: Option<&str>, effort: Option<&str>) -> RunPick {
    RunPick {
        model: model.map(str::to_owned),
        effort: effort.map(str::to_owned),
    }
}

#[test]
fn offers_nothing_for_a_single_model_without_effort_levels() {
    let only = Models {
        current: "only".into(),
        models: vec![model("only", "Only", &[], None)],
    };
    assert!(!offers_choice(&only));
    assert!(!offers_choice(&Models::default()));
    let tunable = Models {
        current: "Tunable".into(),
        models: vec![model("Tunable", "Tunable", &[("high", "High")], None)],
    };
    assert!(offers_choice(&tunable));
    assert!(offers_choice(&fast_and_deep()));
    // Settled lookups keep the ones that offer a choice, in mention order;
    // a type that cannot list its models offers none.
    let settled = settled(vec![
        ("fixed".into(), "fixed".into(), "t/fixed".into(), Ok(only)),
        (
            "broken".into(),
            "broken".into(),
            "t/broken".into(),
            Err("offline".into()),
        ),
        (
            "tunable".into(),
            "tunable".into(),
            "t/tunable".into(),
            Ok(tunable.clone()),
        ),
    ]);
    assert_eq!(settled, vec![choice("tunable", "tunable", tunable)]);
}

#[test]
fn discards_pending_metadata_after_the_mentioned_agent_changes() {
    let key = |pubkey: &str| -> Key {
        vec![(
            pubkey.into(),
            Some(("type".into(), pubkey.into(), "{}".into(), 0)),
        )]
    };
    let loading = |pubkey: &str| Choice {
        pubkey: pubkey.into(),
        name: pubkey.into(),
        models: Models::default(),
        loading: true,
    };
    let mut state = RunModels::default();
    state.begin(key("old"), vec![loading("old")], Vec::new(), None);
    assert!(state.looked_up(&key("old")));
    // A new set shows its own loading rows, never the old choices.
    state.begin(key("new"), vec![loading("new")], Vec::new(), None);
    assert_eq!(state.choices(), [loading("new")]);
    let new = choice("new", "new", fast_and_deep());
    assert!(state.settle(&key("new"), vec![new.clone()]));
    assert!(!state.settle(&key("old"), vec![choice("old", "old", fast_and_deep())]));
    assert_eq!(state.choices(), [new]);
}

#[test]
fn asks_for_a_model_and_effort_on_the_next_run_only() {
    let models = fast_and_deep();
    let fast = run_model(&models, None);
    assert_eq!(fast.label, "Fast");
    // No pick and no known level: the first stop, muted, "Default".
    assert_eq!(
        effort_stop(&fast.efforts, None, None),
        (0, false, "Default".into())
    );
    // ArrowRight picks High.
    let next = choose_effort(&fast.efforts, None, &RunPick::default(), 1);
    assert_eq!(next, pick(None, Some("high")));
    assert_eq!(
        effort_stop(&fast.efforts, None, Some("high")),
        (1, true, "High".into())
    );
    let picks = with_pick(&Picks::new(), "honey", next);
    assert_eq!(
        picks,
        Picks::from([("honey".into(), pick(None, Some("high")))])
    );

    // A new model keeps an effort it also offers.
    let deep = choose_model(&models, &pick(None, Some("high")), "deep");
    assert_eq!(deep, pick(Some("deep"), Some("high")));
    let runs = run_model(&models, deep.model.as_deref());
    assert_eq!(runs.label, "Deep");
    assert_eq!(
        effort_stop(&runs.efforts, None, deep.effort.as_deref()).2,
        "High"
    );
    // An effort the new model lacks falls back to its default.
    assert_eq!(
        choose_model(&models, &pick(None, Some("low")), "deep"),
        pick(Some("deep"), None)
    );
    // Back to the agent's own model is no pick at all.
    let own = choose_model(&models, &pick(Some("deep"), None), "fast");
    assert_eq!(own, RunPick::default());
    assert!(with_pick(&picks, "honey", own).is_empty());
}

#[test]
fn opens_the_effort_picker_at_the_effort_the_next_run_uses() {
    let efforts = vec![
        effort("low", "Low"),
        effort("medium", "Medium"),
        effort("high", "High"),
    ];
    // A known level is drawn like a picked one, not muted.
    assert_eq!(
        effort_stop(&efforts, Some("medium"), None),
        (1, true, "Medium".into())
    );
    let high = choose_effort(&efforts, Some("medium"), &RunPick::default(), 2);
    assert_eq!(high, pick(None, Some("high")));
    // Back to the agent's own effort is no pick at all.
    assert_eq!(
        choose_effort(&efforts, Some("medium"), &high, 1),
        RunPick::default()
    );
    // A model pick stays when the effort goes back.
    assert_eq!(
        choose_effort(
            &efforts,
            Some("medium"),
            &pick(Some("deep"), Some("high")),
            1
        ),
        pick(Some("deep"), None)
    );
}

#[test]
fn names_the_picks_in_the_gauge_tooltip() {
    let honey = choice("honey", "Honey", fast_and_deep());
    let bee = choice("bee", "Bee", fast_and_deep());
    assert_eq!(
        tooltip(&picked(std::slice::from_ref(&honey), &Picks::new())),
        TITLE
    );
    let picks = Picks::from([("honey".into(), pick(None, Some("high")))]);
    assert_eq!(
        tooltip(&picked(std::slice::from_ref(&honey), &picks)),
        "Next run: High"
    );
    let picks = Picks::from([
        ("honey".into(), pick(Some("deep"), Some("max"))),
        ("bee".into(), pick(Some("deep"), None)),
    ]);
    assert_eq!(
        tooltip(&picked(&[honey.clone(), bee], &picks)),
        "Next run: Honey: Deep · Max, Bee: Deep"
    );
    // A loading agent, or a pick for an agent not offered, says nothing.
    let loading = Choice {
        loading: true,
        models: Models::default(),
        ..honey
    };
    assert!(picked(&[loading], &picks).is_empty());
}

#[test]
fn asks_only_agents_still_notified() {
    let picks = Picks::from([
        ("honey".into(), pick(None, Some("high"))),
        ("gone".into(), pick(Some("deep"), None)),
    ]);
    assert_eq!(
        sent_picks(&picks, &["bob".into(), "honey".into()]),
        Picks::from([("honey".into(), pick(None, Some("high")))])
    );
    assert!(sent_picks(&picks, &[]).is_empty());
}
