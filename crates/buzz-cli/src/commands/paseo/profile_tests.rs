use super::*;
use crate::commands::paseo::test_prompt::Script;
use axum::{http::HeaderMap, routing::get, Json, Router};

/// A fake GitHub `/user` that accepts one token.
async fn github() -> String {
    let app = Router::new().route(
        "/user",
        get(|headers: HeaderMap| async move {
            if headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer ghp_good")
            {
                Ok(Json(json!({"login": "ada-l", "name": "Ada Lovelace"})))
            } else {
                Err(axum::http::StatusCode::UNAUTHORIZED)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn base() -> Map<String, Value> {
    let mut base = Map::new();
    base.insert("version".into(), json!(1));
    base.insert("developer".into(), json!("ada"));
    base
}

fn local() -> Local {
    Local {
        git_email: Some("ada@example.com".into()),
        via_profile: SHARED_VIA_PROFILE.into(),
        via_explicit: false,
    }
}

fn keygen(login: &str) -> Result<SigningKey, CliError> {
    assert_eq!(login, "ada-l");
    Ok(SigningKey {
        private: "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n"
            .into(),
        public: "ssh-ed25519 AAAA buzz-agents ada-l".into(),
    })
}

const MODEL_SKIPS: [(&str, Option<&str>); 4] = [
    ("Anthropic API key", None),
    ("Codex access token", None),
    ("OpenAI API key", None),
    ("OpenRouter API key", None),
];

#[test]
fn shared_via_profile_matches_the_runbook() {
    let runbook = include_str!("../../../../../voltage/remote-agents.md");
    let section = &runbook[runbook.find("### Shared via profile").unwrap()..];
    let start = section.find("```toml\n").unwrap() + "```toml\n".len();
    let end = start + section[start..].find("```").unwrap();
    assert_eq!(SHARED_VIA_PROFILE, &section[start..end]);
}

#[tokio::test]
async fn new_profile_prefills_identity_explains_each_credential_and_checks_github() {
    let api = github().await;
    let mut steps = vec![
        ("GitHub token", Some("ghp_good")),
        ("Git name", None),
        ("Git email", None),
        ("1Password service account token", Some("ops_fixture")),
        ("Claude subscription token", Some("sk-ant-oat01-fixture")),
    ];
    steps.extend(MODEL_SKIPS);
    let mut script = Script::new(&steps);
    let collected = new_profile(&mut script, base(), &local(), &api, keygen)
        .await
        .unwrap();
    assert!(script.finished());
    assert_eq!(
        collected.profile,
        json!({
            "version": 1,
            "developer": "ada",
            "git": {
                "name": "Ada Lovelace",
                "email": "ada@example.com",
                "ssh_signing_key": keygen("ada-l").unwrap().private,
            },
            "github": {"login": "ada-l", "token": "ghp_good"},
            "via": {"profile": SHARED_VIA_PROFILE, "op_token": "ops_fixture"},
            "models": {"claude_oauth_token": "sk-ant-oat01-fixture"},
        })
    );
    assert_eq!(
        collected.signing_public.as_deref(),
        Some("ssh-ed25519 AAAA buzz-agents ada-l")
    );
    let guidance = script.notes.join("\n");
    for helper in [
        "github.com/settings/personal-access-tokens/new",
        "Service Account",
        "claude setup-token",
        "chatgpt.com/admin/access-tokens",
        "openrouter.ai/keys",
    ] {
        assert!(guidance.contains(helper), "missing helper: {helper}");
    }
    assert!(!guidance.contains("ghp_good") && !guidance.contains("ops_fixture"));

    // Rows: a token GitHub refuses, and an exclusive credential pair, both
    // fail before anything is enrolled.
    let mut refused = Script::new(&[("GitHub token", Some("ghp_bad"))]);
    let error = new_profile(&mut refused, base(), &local(), &api, keygen)
        .await
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("GitHub did not accept"),
        "{error}"
    );
    let mut both = Script::new(&[
        ("GitHub token", Some("ghp_good")),
        ("Git name", None),
        ("Git email", None),
        ("1Password service account token", Some("ops_fixture")),
        ("Claude subscription token", Some("sk-ant-oat01-fixture")),
        ("Anthropic API key", Some("sk-ant-api-fixture")),
        ("Codex access token", None),
        ("OpenAI API key", None),
        ("OpenRouter API key", None),
    ]);
    let error = new_profile(&mut both, base(), &local(), &api, keygen)
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("enter only one"), "{error}");
}

#[tokio::test]
async fn update_sends_only_the_credentials_typed() {
    let api = github().await;
    let mut steps = vec![
        ("New GitHub token", Some("ghp_good")),
        ("New 1Password service account token", None),
        ("Claude subscription token", None),
    ];
    steps.extend(MODEL_SKIPS[..3].iter().copied());
    steps.push(("OpenRouter API key", Some("sk-or-fixture")));
    let mut script = Script::new(&steps.clone());
    let collected = updated_profile(&mut script, base(), &api, &local(), None)
        .await
        .unwrap();
    assert!(script.finished());
    assert_eq!(
        collected.profile,
        json!({
            "version": 1,
            "developer": "ada",
            "github": {"login": "ada-l", "token": "ghp_good"},
            "models": {"openrouter_api_key": "sk-or-fixture"},
        })
    );
    assert!(collected.signing_public.is_none());

    // --via-profile replaces the stored via profile, and
    // --replace-signing-key sends a new key and returns its public half.
    let explicit = Local {
        via_explicit: true,
        via_profile: "version = 1\n".into(),
        ..local()
    };
    let mut script = Script::new(&steps);
    let collected = updated_profile(&mut script, base(), &api, &explicit, Some(keygen_any))
        .await
        .unwrap();
    assert_eq!(
        collected.profile["via"],
        json!({"profile": "version = 1\n"})
    );
    assert_eq!(
        collected.profile["git"],
        json!({"ssh_signing_key": keygen("ada-l").unwrap().private})
    );
    assert_eq!(
        collected.signing_public.as_deref(),
        Some("ssh-ed25519 AAAA buzz-agents ada-l")
    );
}

fn keygen_any(_: &str) -> Result<SigningKey, CliError> {
    keygen("ada-l")
}

#[test]
fn pairing_offer_renders_as_a_qr_code() {
    let code = super::super::qr("https://app.paseo.sh/#offer=fixture").unwrap();
    assert!(code.lines().count() > 10);
}
