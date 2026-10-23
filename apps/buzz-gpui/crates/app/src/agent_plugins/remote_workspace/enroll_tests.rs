use super::*;
use buzz_session_control::Reply;
use core::prelude::v1::test;

const OWNER: &str = "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11";
const TOKENS: [&str; 4] = [
    "ghp_fixtureGithubToken",
    "ops_fixtureOpToken",
    "sk-ant-fixtureKey",
    "sk-fixtureOpenaiKey",
];

/// An agent signing key in the shape `ssh-keygen -t ed25519` writes.
const KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQ==\n-----END OPENSSH PRIVATE KEY-----\n";

/// The GitHub token's account.
fn github() -> GitHubIdentity {
    GitHubIdentity {
        login: "tee8z".into(),
        name: "Tee Eight".into(),
    }
}

/// `profile` for the fixture account and key.
fn built(values: &Values, owner: &str) -> Result<Value, String> {
    profile(values, owner, &github(), KEY)
}

/// A filled form, with the Anthropic and OpenAI keys.
fn values() -> Values {
    let mut values = Values::default();
    for (field, text) in [
        (Field::GitEmail, "tee@example.com"),
        (Field::GithubToken, " ghp_fixtureGithubToken "),
        (
            Field::ViaProfile,
            "[providers.op]\nkind = \"1password\"\n\n[services.github]\nprovider = \"op\"\n",
        ),
        (Field::OpToken, "ops_fixtureOpToken"),
        (Field::AnthropicApiKey, "sk-ant-fixtureKey"),
        (Field::OpenaiApiKey, "sk-fixtureOpenaiKey"),
    ] {
        values.set(field, text.into());
    }
    values
}

#[test]
fn the_form_builds_a_profile_matching_profile_py() {
    let profile = built(&values(), OWNER).unwrap();
    assert_eq!(
        profile,
        json!({
            "version": 1,
            // The manager fills in the signing owner's developer ID.
            "developer": "",
            "owner_pubkey": OWNER,
            "git": {
                "name": "Tee Eight",
                "email": "tee@example.com",
                "ssh_signing_key": KEY,
            },
            "github": {"login": "tee8z", "token": "ghp_fixtureGithubToken"},
            "via": {
                "profile": values().get(Field::ViaProfile),
                "op_token": "ops_fixtureOpToken",
            },
            "models": {
                "anthropic_api_key": "sk-ant-fixtureKey",
                "openai_api_key": "sk-fixtureOpenaiKey",
            },
        })
    );
    let mut no_models = values();
    no_models.set(Field::AnthropicApiKey, String::new());
    no_models.set(Field::OpenaiApiKey, "  ".into());
    assert_eq!(built(&no_models, OWNER).unwrap()["models"], json!({}));
    // Both subscriptions, for Codex and Claude Code in parallel.
    let mut subscriptions = no_models;
    subscriptions.set(Field::CodexAccessToken, "at-fixtureCodexToken".into());
    subscriptions.set(Field::ClaudeOauthToken, "sk-ant-oat01-fixture".into());
    subscriptions.set(Field::OpenrouterApiKey, "sk-or-fixture".into());
    assert_eq!(
        built(&subscriptions, OWNER).unwrap()["models"],
        json!({
            "codex_access_token": "at-fixtureCodexToken",
            "claude_oauth_token": "sk-ant-oat01-fixture",
            "openrouter_api_key": "sk-or-fixture",
        })
    );
}

#[test]
fn the_profile_is_refused_as_profile_py_refuses_it() {
    let refused = |field: Field, text: &str| {
        let mut values = values();
        values.set(field, text.into());
        built(&values, OWNER).unwrap_err()
    };
    for (field, text, message) in [
        (Field::GitEmail, "", "Invalid Git identity"),
        (Field::GitEmail, "tee\n@example.com", "Invalid Git identity"),
        (Field::GithubToken, "", "Invalid credential"),
        (Field::GithubToken, "ghp_a b", "Invalid credential"),
        (Field::ViaProfile, "[services", "Invalid via profile"),
        (
            Field::ViaProfile,
            "[providers.op]\nkind = \"x\"\n",
            "via profile requires providers and services",
        ),
        (
            Field::CodexAccessToken,
            "at-fixtureCodexToken",
            "Choose either an OpenAI API key or a Codex access token",
        ),
        (
            Field::OpToken,
            "x".repeat(8193).as_str(),
            "Invalid credential",
        ),
        (
            Field::ClaudeOauthToken,
            "sk-ant-oat-fixture",
            "Choose either a Claude API key or a Claude subscription token",
        ),
    ] {
        assert_eq!(refused(field, text), message, "{field:?}");
    }
    assert_eq!(built(&values(), "AA").unwrap_err(), "Invalid Buzz owner");
    // What the form does not ask for is checked as the manager will check it.
    let with = |pointer: &str, value: Value| {
        let mut profile = built(&values(), OWNER).unwrap();
        *profile.pointer_mut(pointer).unwrap() = value;
        validate(&profile)
    };
    assert!(with("/developer", json!("tee8z")).is_ok());
    for (pointer, value, message) in [
        ("/developer", json!("Tee8z"), "Invalid developer ID"),
        ("/developer", json!("tee8z-"), "Invalid developer ID"),
        ("/git/name", json!(""), "Invalid Git identity"),
        ("/github/login", json!("-tee8z"), "Invalid GitHub login"),
        (
            "/git/ssh_signing_key",
            json!("ABCDEF0123456789ABCDEF0123456789ABCDEF01"),
            "Invalid SSH signing key",
        ),
        (
            "/git/ssh_signing_key",
            json!(
                "-----BEGIN OPENSSH PRIVATE KEY-----\nab cd\n-----END OPENSSH PRIVATE KEY-----\n"
            ),
            "Invalid SSH signing key",
        ),
    ] {
        assert_eq!(with(pointer, value).unwrap_err(), message, "{pointer}");
    }
    // An OpenAI API key pasted as the Codex access token.
    let mut pasted = values();
    pasted.set(Field::OpenaiApiKey, String::new());
    pasted.set(Field::CodexAccessToken, "sk-proj-fixture".into());
    assert_eq!(
        built(&pasted, OWNER).unwrap_err(),
        "Use a Codex access token (at-...)"
    );
    let mut extra = built(&values(), OWNER).unwrap();
    extra["models"]["gemini_api_key"] = json!("x");
    assert_eq!(
        validate(&extra).unwrap_err(),
        "Unsupported model credential type"
    );
    extra = built(&values(), OWNER).unwrap();
    extra["extra"] = json!(true);
    assert_eq!(
        validate(&extra).unwrap_err(),
        "Profile fields are incomplete or unsupported"
    );
}

#[test]
fn the_signed_enroll_event_holds_no_token_or_profile_field() {
    let owner = Identity::ephemeral();
    let manager = Identity::ephemeral().public_key();
    let profile = built(&values(), &owner.pubkey_hex()).unwrap();
    let event = owner
        .enroll_request(&manager, "enroll-1", profile, 1_700_000_000)
        .unwrap();
    assert_eq!(event.kind.as_u16(), KIND);
    let text = event.as_json();
    for secret in TOKENS {
        assert!(!text.contains(secret), "{secret}");
    }
    for name in [
        "github",
        "token",
        "op_token",
        "developer",
        "tee8z",
        "models",
        "enroll",
    ] {
        assert!(!text.contains(name), "{name}");
    }
    let tags: Vec<_> = event
        .tags
        .iter()
        .map(|tag| tag.as_slice()[0].clone())
        .collect();
    assert_eq!(tags, ["p", "expiration"]);
}

#[tokio::test]
async fn the_done_reply_maps_to_enrolled_or_its_refusal() {
    let replies = |done: Value| {
        futures::stream::iter([
            None,
            Some(Reply::accepted("enroll-1")),
            Some(Reply::progress("enroll-1")),
            Some(Reply::done(
                "enroll-0",
                json!({"ok": true, "version_id": "old"}),
            )),
            Some(Reply::done("enroll-1", done)),
        ])
    };
    let outcome_of = |done: Value| async move {
        super::done(replies(done), "enroll-1", |reply: &Option<Reply>| {
            reply.clone()
        })
        .await
        .and_then(|response| outcome(&response))
    };
    assert_eq!(
        outcome_of(json!({"ok": true, "version_id": "v-2"})).await,
        Ok(Outcome::Enrolled {
            version_id: "v-2".into(),
            summary: None,
        })
    );
    // The manager's summary of the merged profile comes with it.
    assert_eq!(
        outcome_of(json!({"ok": true, "version_id": "v-3", "profile": summary_json()})).await,
        Ok(Outcome::Enrolled {
            version_id: "v-3".into(),
            summary: Some(summary()),
        })
    );
    for (code, _) in REFUSALS {
        let refusal = json!({"ok": false, "code": code, "message": "No."});
        let outcome = outcome_of(refusal).await.unwrap();
        assert_eq!(
            outcome,
            Outcome::Refused {
                code: code.to_string()
            }
        );
        assert!(
            outcome.text().starts_with(&format!("Refused ({code}). ")),
            "{code}"
        );
    }
    assert!(outcome_of(json!({"ok": true})).await.is_err());
    assert!(outcome_of(json!("done")).await.is_err());
    let unanswered = futures::stream::iter([Some(Reply::accepted("enroll-1"))]);
    assert!(
        super::done(unanswered, "enroll-1", |reply: &Option<Reply>| reply
            .clone())
        .await
        .is_err()
    );
}

#[test]
fn sending_clears_the_tokens_and_keeps_the_rest() {
    let mut form = values();
    let sent = form.submit();
    for field in Field::ALL {
        if field.secret() {
            assert_eq!(form.get(field), "", "{field:?}");
        } else {
            assert_eq!(form.get(field), values().get(field), "{field:?}");
        }
        assert_eq!(sent.get(field), values().get(field), "{field:?}");
    }
    assert!(built(&sent, OWNER).is_ok());
}

#[test]
fn the_git_author_comes_from_the_github_account() {
    let named = signing::parse_identity(br#"{"login": "tee8z", "name": " Tee Eight "}"#).unwrap();
    assert_eq!(named, github());
    // An account without a name commits under its login.
    for body in [
        &br#"{"login": "tee8z", "name": null}"#[..],
        br#"{"login": "tee8z", "name": ""}"#,
        br#"{"login": "tee8z", "name": "a\nb"}"#,
    ] {
        assert_eq!(signing::parse_identity(body).unwrap().name, "tee8z");
    }
    assert!(signing::parse_identity(br#"{"name": "Tee"}"#).is_err());
    assert!(signing::parse_identity(b"not json").is_err());
}

#[test]
fn a_gh_failure_says_what_to_do() {
    for (stderr, hint) in [
        (
            "HTTP 404: This API operation needs the \"admin:ssh_signing_key\" scope.",
            "gh auth refresh -h github.com -s admin:ssh_signing_key",
        ),
        (
            "To get started with GitHub CLI, please run:  gh auth login",
            "gh auth login",
        ),
        (
            "HTTP 422: key is already in use",
            "already on a GitHub account",
        ),
        ("HTTP 500", "could not add"),
    ] {
        assert!(signing::gh_failure(stderr).contains(hint), "{stderr}");
    }
}

/// The manager's summary of a stored profile, as it sends it.
fn summary_json() -> Value {
    json!({
        "version_id": "v-1",
        "updated_at": "2026-10-23T12:00:00Z",
        "git": {"name": "Tee Eight", "email": "tee@example.com", "signing_key": true},
        "github": {"login": "tee8z", "token": true},
        "via": {"profile": true, "op_token": true},
        "models": ["claude_oauth_token"],
    })
}

fn summary() -> Summary {
    serde_json::from_value(summary_json()).unwrap()
}

/// With a profile on file, only filled fields are sent, a blank GitHub token
/// and the signing key are kept, and a removed credential is `null`.
#[test]
fn a_change_over_a_stored_profile_sends_only_what_changes() {
    let mut values = Values::default();
    values.set(Field::CodexAccessToken, " at-fixtureCodexToken ".into());
    let sent = partial(
        &values,
        OWNER,
        Changes {
            removed: &["claude_oauth_token"],
            ..Changes::default()
        },
    )
    .unwrap();
    assert_eq!(
        sent,
        json!({
            "version": 1,
            "developer": "",
            "owner_pubkey": OWNER,
            "models": {"codex_access_token": "at-fixtureCodexToken", "claude_oauth_token": null},
        })
    );
    // A new token sends its account and the Git name; a new key is sent too.
    let mut values = Values::default();
    values.set(Field::GithubToken, "ghp_fixtureGithubToken".into());
    let account = github();
    let sent = partial(
        &values,
        OWNER,
        Changes {
            github: Some(&account),
            signing_key: Some(KEY),
            removed: &[],
        },
    )
    .unwrap();
    assert_eq!(
        sent["github"],
        json!({"login": "tee8z", "token": "ghp_fixtureGithubToken"})
    );
    assert_eq!(
        sent["git"],
        json!({"name": "Tee Eight", "ssh_signing_key": KEY})
    );
    assert!(sent.get("via").is_none() && sent.get("models").is_none());
    // Nothing filled and nothing removed is not sent.
    assert_eq!(
        partial(&Values::default(), OWNER, Changes::default()),
        Err("Nothing to change: fill in a field, or remove a credential.".into())
    );
}

/// Each field a change sends passes `profile.py`'s rule for it.
#[test]
fn a_change_is_refused_as_profile_py_refuses_its_fields() {
    let refused = |field, text: &str| {
        let mut values = Values::default();
        values.set(field, text.into());
        partial(&values, OWNER, Changes::default()).unwrap_err()
    };
    assert_eq!(
        refused(Field::CodexAccessToken, "sk-notAnAccessToken"),
        "Use a Codex access token (at-...)"
    );
    assert_eq!(
        refused(Field::OpToken, "ops with space"),
        "Invalid credential"
    );
    assert_eq!(
        refused(Field::ViaProfile, "not = [toml"),
        "Invalid via profile"
    );
    let mut both = Values::default();
    both.set(Field::OpenaiApiKey, "sk-fixtureOpenaiKey".into());
    both.set(Field::CodexAccessToken, "at-fixtureCodexToken".into());
    assert_eq!(
        partial(&both, OWNER, Changes::default()).unwrap_err(),
        "Choose either an OpenAI API key or a Codex access token"
    );
    assert_eq!(
        validate_partial(&json!({"version": 1, "owner_pubkey": OWNER,
            "models": {"gemini_api_key": null}})),
        Err("Unsupported model credential type".into())
    );
}

/// The `profile` reply: the summary, nothing on file, or the refusal.
#[test]
fn the_profile_reply_names_what_is_on_file() {
    assert_eq!(
        on_file_reply(&json!({"ok": true, "profile": summary_json()})),
        Ok(Some(summary()))
    );
    assert_eq!(
        on_file_reply(&json!({"ok": true, "profile": null})),
        Ok(None)
    );
    let refused =
        on_file_reply(&json!({"ok": false, "code": "unauthorized_owner", "error": "No."}));
    assert!(
        refused
            .unwrap_err()
            .starts_with("Refused (unauthorized_owner). ")
    );
    assert!(on_file_reply(&json!("done")).is_err());
    let summary = summary();
    assert!(summary.holds(Field::GithubToken) && summary.holds(Field::ClaudeOauthToken));
    assert!(!summary.holds(Field::CodexAccessToken));
    assert_eq!(summary.model_fields(), [Field::ClaudeOauthToken]);
    assert_eq!(
        OnFile::Stored(summary).text(),
        "On file: GitHub @tee8z · Git email tee@example.com · signing key · via profile · \
         1Password token · Claude subscription token. Updated 2026-10-23 12:00 UTC."
    );
    assert_eq!(OnFile::Nothing.text(), "Nothing enrolled yet.");
}
