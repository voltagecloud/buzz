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

/// A filled form, with the Anthropic and OpenAI keys.
fn values() -> Values {
    let mut values = Values::default();
    for (field, text) in [
        (Field::Developer, "tee8z"),
        (Field::GitName, "Tee Eight"),
        (Field::GitEmail, "tee@example.com"),
        (
            Field::SigningKey,
            "0123456789ABCDEF0123456789ABCDEF01234567",
        ),
        (Field::GithubLogin, "tee8z"),
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
    let built = profile(&values(), OWNER).unwrap();
    assert_eq!(
        built,
        json!({
            "version": 1,
            "developer": "tee8z",
            "owner_pubkey": OWNER,
            "git": {
                "name": "Tee Eight",
                "email": "tee@example.com",
                "signing_key": "0123456789ABCDEF0123456789ABCDEF01234567",
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
    assert_eq!(profile(&no_models, OWNER).unwrap()["models"], json!({}));
    // Both subscriptions, for Codex and Claude Code in parallel.
    let mut subscriptions = no_models;
    subscriptions.set(Field::CodexAccessToken, "at-fixtureCodexToken".into());
    subscriptions.set(Field::ClaudeOauthToken, "sk-ant-oat01-fixture".into());
    assert_eq!(
        profile(&subscriptions, OWNER).unwrap()["models"],
        json!({
            "codex_access_token": "at-fixtureCodexToken",
            "claude_oauth_token": "sk-ant-oat01-fixture",
        })
    );
}

#[test]
fn the_profile_is_refused_as_profile_py_refuses_it() {
    let refused = |field: Field, text: &str| {
        let mut values = values();
        values.set(field, text.into());
        profile(&values, OWNER).unwrap_err()
    };
    for (field, text, message) in [
        (Field::Developer, "Tee8z", "Invalid developer ID"),
        (Field::Developer, "tee8z-", "Invalid developer ID"),
        (Field::GitName, "", "Invalid Git identity"),
        (Field::GitEmail, "tee\n@example.com", "Invalid Git identity"),
        (
            Field::SigningKey,
            "ABCDEF01",
            "Use a full OpenPGP signing fingerprint",
        ),
        (Field::GithubLogin, "-tee8z", "Invalid GitHub login"),
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
    assert_eq!(profile(&values(), "AA").unwrap_err(), "Invalid Buzz owner");
    // An OpenAI API key pasted as the Codex access token.
    let mut pasted = values();
    pasted.set(Field::OpenaiApiKey, String::new());
    pasted.set(Field::CodexAccessToken, "sk-proj-fixture".into());
    assert_eq!(
        profile(&pasted, OWNER).unwrap_err(),
        "Use a Codex access token (at-...)"
    );
    let mut extra = profile(&values(), OWNER).unwrap();
    extra["models"]["gemini_api_key"] = json!("x");
    assert_eq!(
        validate(&extra).unwrap_err(),
        "Unsupported model credential type"
    );
    extra = profile(&values(), OWNER).unwrap();
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
    let built = profile(&values(), &owner.pubkey_hex()).unwrap();
    let event = owner
        .enroll_request(&manager, "enroll-1", built, 1_700_000_000)
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
    };
    assert_eq!(
        outcome_of(json!({"ok": true, "version_id": "v-2"})).await,
        Ok(Outcome::Enrolled {
            version_id: "v-2".into()
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
    assert!(profile(&sent, OWNER).is_ok());
}
