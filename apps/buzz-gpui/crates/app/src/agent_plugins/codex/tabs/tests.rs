//! buzz-app's `tabs.test.tsx`: the choices the settings offer, what a save
//! checks, and the catalog's own server.

use super::*;
use crate::agent_plugins::codex::config::Effort;
use crate::agent_plugins::codex::rpc::testing::{FakeCodex, gate, until};
use core::prelude::v1::test;

fn models() -> Vec<Model> {
    let effort = |name: &str| Effort {
        reasoning_effort: name.into(),
        description: String::new(),
    };
    vec![
        Model {
            model: "model-a".into(),
            display_name: "Model A".into(),
            description: "Fast".into(),
            is_default: true,
            default_reasoning_effort: "low".into(),
            supported_reasoning_efforts: vec![effort("low"), effort("high")],
        },
        Model {
            model: "model-b".into(),
            display_name: "Model B".into(),
            default_reasoning_effort: "medium".into(),
            supported_reasoning_efforts: vec![effort("medium")],
            ..Default::default()
        },
    ]
}

fn labels(options: &Options) -> Vec<&str> {
    options.iter().map(|(_, label)| label.as_str()).collect()
}

#[test]
fn offers_codex_models_and_the_selected_models_thinking() {
    let models = models();
    let draft = Config::default();
    let ids = |choices: Vec<ModelChoice>| -> Vec<String> {
        choices.into_iter().map(|choice| choice.label).collect()
    };
    assert_eq!(ids(model_choices(&models)), ["Model A", "Model B"]);
    assert_eq!(
        model_choices(&models)[0].description.as_deref(),
        Some("Fast")
    );
    assert_eq!(
        super::super::defaults::native_label(Some(&models)),
        "Codex default (Model A)"
    );
    // Before the catalog loads, the default has no name.
    assert_eq!(super::super::defaults::native_label(None), "Codex default");
    let selected = selected_model(&draft, &models);
    assert_eq!(selected.map(|model| model.model.as_str()), Some("model-a"));
    assert_eq!(
        labels(&effort_options(&draft, selected)),
        ["Codex default (Low)", "Low", "High"]
    );
    let b = Config {
        model: "model-b".into(),
        effort: "medium".into(),
        ..Config::default()
    };
    let selected = selected_model(&b, &models);
    assert_eq!(
        labels(&effort_options(&b, selected)),
        ["Codex default (Medium)", "Medium"]
    );
}

#[test]
fn keeps_saved_choices_codex_no_longer_offers() {
    let models = models();
    let retired = Config {
        model: "retired-model".into(),
        effort: "xhigh".into(),
        ..Config::default()
    };
    let selected = selected_model(&retired, &models);
    assert!(selected.is_none());
    // Without a model to offer efforts, the saved one still shows.
    assert_eq!(
        labels(&effort_options(&retired, selected)),
        ["Codex default", "Extra high (unavailable)"]
    );
}

#[test]
fn saves_only_a_valid_workspace_and_restores_the_default_when_cleared() {
    let relative = Config {
        workspace: "relative".into(),
        ..Config::default()
    };
    assert_eq!(
        validate(&relative),
        Err("Enter an absolute or ~/… workspace path.".into())
    );
    let custom = Config {
        workspace: " /tmp/codex ".into(),
        instructions: "Custom instructions".into(),
        ..Config::default()
    };
    assert_eq!(
        validate(&custom).map(|config| config.workspace),
        Ok("/tmp/codex".into())
    );
    let cleared = Config {
        workspace: "   ".into(),
        ..Config::default()
    };
    assert_eq!(validate(&cleared), Ok(Config::default()));
    // The process host expands a workspace under home.
    let home = Config {
        workspace: "~/code".into(),
        ..Config::default()
    };
    assert_eq!(
        validate(&home).map(|config| config.workspace),
        Ok("~/code".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_the_account_and_models_then_ends_its_server() {
    let fake = FakeCodex::default();
    fake.state().models = serde_json::json!([
        { "model": "model-a", "displayName": "Model A", "isDefault": true },
    ]);
    let catalog = catalog(fake.spawner(), Signal::default()).await.unwrap();
    assert_eq!(catalog.account, "test@example.com");
    assert_eq!(catalog.models[0].display_name, "Model A");
    assert_eq!(fake.state().ended, 1);
    // Its server starts where the host starts it, not in a workspace.
    assert_eq!(fake.spawned("app-server")[0].cwd, None);

    fake.state().account = serde_json::json!({ "account": { "type": "apiKey" } });
    let catalog = super::catalog(fake.spawner(), Signal::default())
        .await
        .unwrap();
    assert_eq!(catalog.account, "apiKey");
    fake.state().account = serde_json::json!({ "account": null });
    assert_eq!(
        super::catalog(fake.spawner(), Signal::default()).await,
        Err(SIGN_IN.to_string())
    );
    assert_eq!(fake.state().ended, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn closes_a_server_returned_after_the_tab_went_away() {
    let fake = FakeCodex::default();
    let (started, began) = tokio::sync::oneshot::channel();
    let (release, open) = gate();
    fake.state().hold_server =
        Some(crate::agent_plugins::codex::rpc::testing::Held { started, release });
    let signal = Signal::default();
    let checking = tokio::spawn(catalog(fake.spawner(), signal.clone()));
    began.await.unwrap();
    signal.abort("Codex check closed");
    open.send(()).unwrap();
    assert!(checking.await.unwrap().is_err());
    until(|| fake.state().ended == 1).await;
    assert!(fake.calls("account/read").is_empty());
}

#[test]
fn says_where_codex_stands() {
    let mut check = CatalogCheck {
        host: Host {
            spawn: None,
            setup: crate::agent_plugins::codex::setup::setup(None, false),
        },
        data: None,
        error: String::new(),
        checking: true,
        setup: Default::default(),
        signal: Signal::default(),
        _task: None,
        _follow: None,
    };
    assert_eq!(status_line(&check), "Checking Codex…");
    check.checking = false;
    assert_eq!(status_line(&check), "Codex needs attention.");
    // Not installed, or signed out: the tab offers the step that fixes it.
    check.error = NOT_INSTALLED.into();
    assert_eq!(check.action(), Some((Step::Install, "Install Codex")));
    check.error = SIGN_IN.into();
    assert_eq!(check.action(), Some((Step::Login, "Sign in")));
    check.error = "Codex did not start".into();
    assert_eq!(check.action(), None);
    check.data = Some(Catalog {
        models: vec![],
        account: "me@example.com".into(),
    });
    assert_eq!(status_line(&check), "Codex is ready (me@example.com).");
}
