//! `buzz onboard` journeys through the production wizard, with only the
//! terminal, relay, manager transport and local tools faked.

#[path = "tests_fakes.rs"]
mod fakes;

use std::path::Path;

use buzz_session_control::Op;
use clap::Parser;
use nostr::{Keys, ToBech32};
use serde_json::{json, Value};
use zeroize::Zeroizing;

use super::gateway::{self, GatewayGrants};
use super::profile::{self, GitHubIdentity};
use super::state::{Secret, Store};
use super::steps_credentials::SHARED_VIA_PROFILE;
use super::{agents, canonical_relay, paseo, steps_identity, EnvSecrets, OnboardArgs, Wizard};
use crate::error::CliError;
use fakes::{strings, Answer, FakeGrants, FakeManager, FakeRelay, FakeTools, ScriptPrompter};

const GITHUB: &str = "ghp_SECRETGITHUBTOKEN";
const GH_CLI: &str = "gho_SECRETGHCLITOKEN";
const OP: &str = "ops_SECRETOPTOKEN";
const CLAUDE: &str = "sk-ant-oat01-SECRETCLAUDE";
const CODEX: &str = "at-SECRETCODEX";
const PASSWORD: &str = "PASEO-SECRET-PASSWORD";
const INVITE: &str = "v2.invitecode";

#[derive(Parser)]
struct Harness {
    #[command(flatten)]
    args: OnboardArgs,
}

fn args(dir: &Path, manager: &FakeManager, extra: &[&str]) -> OnboardArgs {
    let mut argv = vec![
        "onboard".to_owned(),
        "--state-dir".into(),
        dir.display().to_string(),
        "--manager-pubkey".into(),
        manager.keys.public_key().to_hex(),
    ];
    argv.extend(extra.iter().map(|arg| arg.to_string()));
    Harness::parse_from(argv).args
}

fn secret(text: &str) -> Option<Zeroizing<String>> {
    Some(Zeroizing::new(text.to_owned()))
}

fn env(owner: &Keys) -> EnvSecrets {
    EnvSecrets {
        private_key: secret(&owner.secret_key().to_bech32().unwrap()),
        github_token: secret(GITHUB),
        op_token: secret(OP),
        claude_token: secret(CLAUDE),
        codex_token: secret(CODEX),
        ..EnvSecrets::default()
    }
}

fn tools() -> FakeTools {
    let octo = GitHubIdentity {
        login: "octo".into(),
        name: "Octo Cat".into(),
    };
    FakeTools {
        identities: [(GITHUB.to_owned(), octo.clone()), (GH_CLI.to_owned(), octo)]
            .into_iter()
            .collect(),
        git_email: "octo@example.com".into(),
        ..FakeTools::default()
    }
}

/// A pairing as the manager returns it; `gateway` empty leaves the key out.
fn paseo(gateway: &str) -> Value {
    let mut pairing = json!({"password": PASSWORD, "server_id": "srv-1",
        "offer_url": "https://app.paseo.sh/#offer=eyJ2IjoyfQ"});
    if !gateway.is_empty() {
        pairing["gateway_pubkey"] = json!(gateway);
    }
    pairing
}

async fn onboard(
    args: OnboardArgs,
    env: EnvSecrets,
    prompter: &mut ScriptPrompter,
    relay: &FakeRelay,
    manager: &FakeManager,
    tools: &FakeTools,
) -> Result<Value, CliError> {
    onboard_with(None, args, env, prompter, relay, manager, tools).await
}

/// [`onboard`] with `grants` in place of this build's [`gateway::Unreleased`].
async fn onboard_with(
    grants: Option<Box<dyn GatewayGrants + Send + Sync>>,
    args: OnboardArgs,
    env: EnvSecrets,
    prompter: &mut ScriptPrompter,
    relay: &FakeRelay,
    manager: &FakeManager,
    tools: &FakeTools,
) -> Result<Value, CliError> {
    let store = Store::new(args.state_dir.clone().unwrap());
    let mut wizard = Wizard::new(args, env, store, prompter, relay, manager, tools)?;
    if let Some(grants) = grants {
        wizard.grants = grants;
    }
    wizard.run().await
}

fn now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap()
}

fn tag<'a>(event: &'a Value, name: &str) -> Vec<&'a str> {
    event["tags"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tag| tag[0] == name)
        .map(|tag| tag[1].as_str().unwrap())
        .collect()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The secrets a run must never show, summarise, or save.
fn assert_no_secrets(haystack: &str, extra: &[String]) {
    let mut secrets: Vec<String> = [GITHUB, GH_CLI, OP, CLAUDE, CODEX, "PRIVATEKEYMATERIAL"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    secrets.extend(extra.iter().cloned());
    for secret in secrets {
        assert!(
            !haystack.contains(&secret),
            "a secret leaked: {}…",
            &secret[..6]
        );
    }
}

/// A non-interactive first run sets up everything the Buzz app would, and a
/// re-run changes nothing.
#[tokio::test]
async fn first_run_sets_up_like_the_app_and_a_rerun_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("buzz");
    let owner = Keys::generate();
    let owner_hex = owner.public_key().to_hex();
    let relay = FakeRelay::with_invite(INVITE);
    relay.state().policy = Some(super::relay::JoinPolicy {
        version: "2026-01".into(),
        age_attestation_required: false,
    });
    let manager = FakeManager::for_developer(&owner_hex);
    let gateway = Keys::generate().public_key().to_hex();
    manager.state().paseo = Some(paseo(&gateway));
    let tools = tools();
    let extra = [
        "--yes",
        "--import-key",
        "--invite",
        "https://buzz.voltage.cloud/invite/v2.invitecode",
        "--accept-join-policy",
        "--name",
        "Octo",
        "--agents",
        "brokkr,bifrost",
    ];
    let mut prompter = ScriptPrompter::quiet();
    let summary = onboard_with(
        Some(Box::new(FakeGrants)),
        args(&state_dir, &manager, &extra),
        env(&owner),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap();

    // Identity: joined through the policy, profile published.
    assert!(relay.state().accepted_policy);
    assert!(relay.state().members.contains(&owner_hex));
    let profiles = relay.of_kind(0);
    assert!(profiles.iter().any(|p| p["pubkey"] == owner_hex));

    // Agents: the exact app shapes.
    assert_eq!(summary["agents"].as_array().unwrap().len(), 2);
    let channel = relay.of_kind(9007);
    assert_eq!(channel.len(), 1);
    assert_eq!(tag(&channel[0], "name"), ["octo-agents"]);
    assert_eq!(tag(&channel[0], "visibility"), ["private"]);
    let channel_id = tag(&channel[0], "h")[0].to_owned();
    for agent in summary["agents"].as_array().unwrap() {
        let pubkey = agent["pubkey"].as_str().unwrap();
        assert_eq!(agent["registered"], true);
        let profile = profiles.iter().find(|p| p["pubkey"] == pubkey).unwrap();
        let content: Value = serde_json::from_str(profile["content"].as_str().unwrap()).unwrap();
        assert_eq!(content, json!({"name": agent["name"], "bot": true}));
        let auth = &profile["tags"][0];
        assert_eq!(profile["tags"].as_array().unwrap().len(), 1);
        assert_eq!(auth[0], "auth");
        assert_eq!(auth[1], owner_hex);
        assert_eq!(auth[2], "");
        let listing = relay
            .of_kind(30177)
            .into_iter()
            .find(|l| tag(l, "d") == [pubkey])
            .unwrap();
        assert_eq!(listing["pubkey"], owner_hex);
        assert_eq!(tag(&listing, "runtime"), [agents::RUNTIME_TYPE]);
        let content: Value = serde_json::from_str(listing["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            content,
            json!({"parallelism": 1, "respond_to": "owner-only", "name": agent["name"]})
        );
        let member = relay
            .of_kind(9000)
            .into_iter()
            .find(|m| tag(m, "p") == [pubkey])
            .unwrap();
        assert_eq!(tag(&member, "h"), [channel_id.as_str()]);
        assert_eq!(tag(&member, "role"), ["bot"]);
        // First start: the kickoff mention, then the registered launch.
        let kickoff = relay
            .of_kind(9)
            .into_iter()
            .find(|m| tag(m, "p") == [pubkey])
            .unwrap();
        let launch = manager.state().registrations[pubkey].clone();
        assert_eq!(launch["relay_url"], "wss://buzz.voltage.cloud");
        assert_eq!(launch["launch"]["owner_pubkey"], owner_hex);
        let policy = &launch["launch"]["policy_env"];
        assert_eq!(policy["BUZZ_ACP_SESSION_POLICY"], "thread");
        assert_eq!(
            policy["BUZZ_ACP_REPLAY_FLOOR"],
            kickoff["created_at"].to_string()
        );
        assert_eq!(policy["BUZZ_ACP_DISPLAY_NAME"], agent["name"]);
    }
    let first = summary["agents"][0]["pubkey"].as_str().unwrap().to_owned();
    let brokkr = manager.state().registrations[&first].clone();
    assert_eq!(brokkr["launch"]["command"], "claude-agent-acp");
    assert_eq!(
        brokkr["launch"]["policy_env"]["BUZZ_ACP_EFFORT_LEVEL"],
        "xhigh"
    );

    // Enrollment: one complete profile, then the key on GitHub.
    let stored = manager.state().profile.clone().unwrap();
    assert_eq!(stored["github"], json!({"login": "octo", "token": GITHUB}));
    assert_eq!(stored["git"]["name"], "Octo Cat");
    assert_eq!(stored["git"]["email"], "octo@example.com");
    assert_eq!(stored["via"]["profile"], SHARED_VIA_PROFILE);
    assert_eq!(
        stored["models"],
        json!({"claude_oauth_token": CLAUDE, "codex_access_token": CODEX})
    );
    assert_eq!(
        manager.state().ops,
        [Op::Profile, Op::Enroll, Op::Deploy, Op::Deploy]
    );
    let added = tools.gh_added.lock().unwrap().clone();
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].0, "octo");
    assert!(added[0].1.starts_with("ssh-ed25519 "));

    // Paseo: the password shown once, the offer as a QR code.
    let transcript = prompter.transcript.join("\n");
    assert_eq!(transcript.matches(PASSWORD).count(), 1);
    assert!(transcript.contains("https://app.paseo.sh/#offer="));
    assert!(transcript.contains('█') || transcript.contains('▀'));
    assert!(transcript.contains("app.paseo.sh"));
    assert_eq!(summary["paseo"]["provisioned"], true);
    assert_eq!(summary["paseo"]["gateway_pubkey"], gateway);

    // Gateway: one owner-signed grant of the daemon's key for 90 days.
    let grants = relay.of_kind(30181);
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["pubkey"], owner_hex);
    assert_eq!(tag(&grants[0], "d"), [gateway.as_str()]);
    assert_eq!(tag(&grants[0], "p"), [gateway.as_str()]);
    let expires: u64 = tag(&grants[0], "expiration")[0].parse().unwrap();
    assert!(expires.abs_diff(now() + 90 * 86_400) < 120, "{expires}");
    assert_eq!(summary["gateway_grant"]["expires_at"], expires);
    assert!(transcript.contains("Granted your Paseo daemon's gateway key until"));

    // Secrets: never shown, summarised, or saved beyond the key files.
    let saved = std::fs::read_to_string(state_dir.join("onboard.json")).unwrap();
    let state: Value = serde_json::from_str(&saved).unwrap();
    let mut keys: Vec<String> = state["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["secret"].as_str().unwrap().to_owned())
        .collect();
    let owner_nsec = owner.secret_key().to_bech32().unwrap();
    keys.push(owner_nsec.clone());
    assert_no_secrets(&transcript, &keys);
    keys.push(PASSWORD.into());
    assert_no_secrets(&summary.to_string(), &keys);
    assert_no_secrets(&saved, &[owner_nsec, PASSWORD.into()]);
    #[cfg(unix)]
    {
        assert_eq!(mode(&state_dir), 0o700);
        assert_eq!(mode(&state_dir.join("onboard.json")), 0o600);
        assert_eq!(mode(&state_dir.join("identity.nsec")), 0o600);
    }

    // Re-run with no secrets and no flags: nothing is sent again.
    let published = relay.state().published;
    let mut prompter = ScriptPrompter::quiet();
    let again = onboard_with(
        Some(Box::new(FakeGrants)),
        args(&state_dir, &manager, &["--yes"]),
        EnvSecrets::default(),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap();
    assert_eq!(relay.state().published, published);
    assert_eq!(manager.state().ops.len(), 5);
    assert_eq!(manager.state().ops[4], Op::Profile);
    assert_eq!(*tools.keys_made.lock().unwrap(), 1);
    assert_eq!(tools.gh_added.lock().unwrap().len(), 1);
    assert_eq!(again["agents"], summary["agents"]);
    assert_eq!(again["channel"], summary["channel"]);
    assert!(prompter.transcript.join("\n").contains("is granted until"));

    // A grant near its expiry is renewed, once.
    let store = Store::new(state_dir.clone());
    let mut state = store.load().unwrap();
    state.gateway_grant.as_mut().unwrap().expires_at = now() + 86_400;
    store.save(&state).unwrap();
    for _ in 0..2 {
        let mut prompter = ScriptPrompter::quiet();
        onboard_with(
            Some(Box::new(FakeGrants)),
            args(&state_dir, &manager, &["--yes", "--gateway-expires", "2w"]),
            EnvSecrets::default(),
            &mut prompter,
            &relay,
            &manager,
            &tools,
        )
        .await
        .unwrap();
    }
    let grants = relay.of_kind(30181);
    assert_eq!(grants.len(), 2);
    let renewed: u64 = tag(&grants[1], "expiration")[0].parse().unwrap();
    assert!(renewed.abs_diff(now() + 14 * 86_400) < 120, "{renewed}");
}

/// The interactive path: a generated key, gh's token, prompts for every
/// secret, a signing key gh cannot add yet, then a resumed run that adds it.
#[tokio::test]
async fn interactive_run_redacts_secrets_and_resumes_the_signing_key() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("buzz");
    let relay = FakeRelay::with_invite(INVITE);
    let manager = FakeManager::for_developer("");
    manager.state().any_developer = true;
    let mut tools = tools();
    tools.gh_token = Some(GH_CLI.into());
    tools
        .gh_results
        .lock()
        .unwrap()
        .push_back(Err("gh needs the admin:ssh_signing_key scope"));
    let mut prompter = ScriptPrompter::scripted(vec![
        Answer::Text(""),     // generate a key
        Answer::Text(INVITE), // invite
        Answer::Text("Dev"),  // display name
        Answer::Text(""),     // use gh's token
        Answer::Text(""),     // the default Git email
        Answer::Text("n"),    // do not run claude setup-token
        Answer::Secret(CLAUDE.into()),
        Answer::Secret(String::new()), // no Codex token
        Answer::Secret(String::new()), // no OpenRouter key
        Answer::Secret(OP.into()),
        Answer::Text("brokkr"), // agents
        Answer::Text(""),       // the default channel
    ]);
    let summary = onboard(
        args(&state_dir, &manager, &[]),
        EnvSecrets::default(),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap();
    assert!(
        prompter.answers.is_empty(),
        "every scripted answer was asked"
    );
    let transcript = prompter.transcript.join("\n");
    assert!(
        transcript.contains("gho_"),
        "gh's token is named by its prefix"
    );
    assert_no_secrets(&transcript, &[]);
    assert_eq!(
        manager.state().profile.as_ref().unwrap()["github"]["token"],
        GH_CLI
    );
    let actions = summary["action_needed"].as_array().unwrap();
    assert!(actions
        .iter()
        .any(|a| a.as_str().unwrap().contains("ssh-ed25519 AAAAPUBLIC1")));
    assert!(transcript.contains("isn't provisioned"));
    assert!(Store::new(state_dir.clone())
        .load()
        .unwrap()
        .pending_key
        .is_some());

    // The resumed run retries only the GitHub step.
    let mut prompter = ScriptPrompter::quiet();
    let again = onboard(
        args(&state_dir, &manager, &["--yes"]),
        EnvSecrets::default(),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap();
    assert_eq!(tools.gh_added.lock().unwrap().len(), 1);
    assert!(Store::new(state_dir.clone())
        .load()
        .unwrap()
        .pending_key
        .is_none());
    assert!(again["action_needed"].as_array().unwrap().is_empty());
    assert_eq!(manager.state().versions, 1, "nothing was enrolled twice");

    // Enabling Paseo later, and rotating its Secret later still, both work
    // without collecting credentials again or replacing agent identities.
    let before = manager.state().profile.clone();
    manager.state().paseo_enabled = true;
    manager.state().provisioned_pairing = Some(paseo(""));
    for version in [2, 3] {
        manager.state().paseo = None;
        let mut prompter = ScriptPrompter::quiet();
        let summary = onboard(
            args(&state_dir, &manager, &["--yes"]),
            EnvSecrets::default(),
            &mut prompter,
            &relay,
            &manager,
            &tools,
        )
        .await
        .unwrap();
        assert_eq!(summary["paseo"]["provisioned"], true);
        assert_eq!(manager.state().versions, version);
        assert_eq!(manager.state().profile, before);
        assert_eq!(tools.gh_added.lock().unwrap().len(), 1);
    }
}

/// A failed publish and a failed first start each fail the run, are
/// reported, and finish on later runs without repeating what landed.
#[tokio::test]
async fn failures_are_reported_and_resume_without_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("buzz");
    let owner = Keys::generate();
    let relay = FakeRelay::with_invite(INVITE);
    relay.state().fail_kind = Some(30177);
    let manager = FakeManager::for_developer(&owner.public_key().to_hex());
    let tools = tools();
    let extra = [
        "--yes",
        "--import-key",
        "--invite",
        INVITE,
        "--name",
        "O",
        "--agents",
        "brokkr",
    ];
    let mut prompter = ScriptPrompter::quiet();
    let error = onboard(
        args(&state_dir, &manager, &extra),
        env(&owner),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, CliError::Relay { status: 503, .. }));
    let state = Store::new(state_dir.clone()).load().unwrap();
    assert!(state.agents[0].profile_published && !state.agents[0].listed);

    // Second run: the listing lands, enrollment happens, the start fails.
    // Third run: the manager starts it but cannot update the Paseo
    // daemon's agent list, so the start stays pending.
    let agent = state.agents[0].pubkey.clone();
    for (index, code) in ["manager_timeout", "paseo_store_failed"].iter().enumerate() {
        manager
            .state()
            .failing_deploys
            .insert(agent.clone(), (*code).to_owned());
        let mut prompter = ScriptPrompter::quiet();
        let error = onboard(
            args(&state_dir, &manager, &["--yes"]),
            if index == 0 {
                env(&owner)
            } else {
                EnvSecrets::default()
            },
            &mut prompter,
            &relay,
            &manager,
            &tools,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("did not start"), "{error}");
        let transcript = prompter.transcript.join("\n");
        assert!(transcript.contains(code), "{transcript}");
        assert_eq!(
            transcript.contains("agent list was not updated"),
            *code == "paseo_store_failed"
        );
        assert!(!Store::new(state_dir.clone()).load().unwrap().agents[0].registered);
    }

    // Fourth run: only the start is retried.
    // A completed deploy whose reply is lost also resumes the same root.
    manager.state().lost_deploy_reply = true;
    let mut prompter = ScriptPrompter::quiet();
    let error = onboard(
        args(&state_dir, &manager, &["--yes"]),
        EnvSecrets::default(),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("manager reply lost after deploy"));
    assert!(manager.state().registrations.contains_key(&agent));
    let mut prompter = ScriptPrompter::quiet();
    onboard(
        args(&state_dir, &manager, &["--yes"]),
        EnvSecrets::default(),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap();
    assert!(manager.state().registrations.contains_key(&agent));
    let profiles = relay
        .of_kind(0)
        .into_iter()
        .filter(|p| p["pubkey"] == agent)
        .count();
    assert_eq!(profiles, 1);
    assert_eq!(relay.of_kind(30177).len(), 1);
    assert_eq!(relay.of_kind(9000).len(), 1);
    assert_eq!(
        relay.of_kind(9).len(),
        1,
        "retries reuse the durable kickoff"
    );
    let roots = manager.state().deploy_roots.clone();
    let kickoff = relay.of_kind(9)[0]["id"].as_str().unwrap().to_owned();
    assert!(roots.iter().all(|root| root == &kickoff));
    assert_eq!(manager.state().versions, 1);
}

/// An owner the manager does not know stops before any credential is asked
/// for, and is told which key to give the admin.
#[tokio::test]
async fn an_unconfigured_owner_stops_before_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("buzz");
    let owner = Keys::generate();
    let relay = FakeRelay::with_invite(INVITE);
    let manager = FakeManager::for_developer("someone-else");
    let tools = tools();
    let extra = ["--yes", "--import-key", "--invite", INVITE, "--name", "O"];
    let mut prompter = ScriptPrompter::quiet();
    let error = onboard(
        args(&state_dir, &manager, &extra),
        env(&owner),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap_err();
    let hex = owner.public_key().to_hex();
    assert!(
        matches!(&error, CliError::Auth(text) if text.contains(&hex)),
        "{error}"
    );
    assert_eq!(manager.state().ops, [Op::Profile]);
    assert_eq!(*tools.keys_made.lock().unwrap(), 0);
    assert!(state_dir.join("identity.nsec").exists());
}

/// Agents the Buzz app already lists are recognised and never duplicated;
/// mutually exclusive credentials are refused before anything is sent.
#[tokio::test]
async fn app_agents_are_kept_and_exclusive_credentials_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("buzz");
    let owner = Keys::generate();
    let owner_hex = owner.public_key().to_hex();
    let relay = FakeRelay::with_invite(INVITE);
    let app_agent = Keys::generate().public_key().to_hex();
    relay.state().events.push(json!({
        "kind": 30177, "pubkey": owner_hex, "created_at": 1,
        "tags": [["d", app_agent], ["runtime", agents::RUNTIME_TYPE]],
        "content": json!({"name": "Brokkr", "parallelism": 1, "respond_to": "owner-only"}).to_string(),
    }));
    let manager = FakeManager::for_developer(&owner_hex);
    manager.state().paseo =
        Some(json!({"password": "x", "server_id": "s", "offer_url": "https://example.com"}));
    let malformed = "The Sandbox manager's Paseo pairing was malformed.";
    let tools = tools();
    let mut secrets = env(&owner);
    secrets.openai_api_key = secret("sk-openai");
    let extra = [
        "--yes",
        "--import-key",
        "--invite",
        INVITE,
        "--name",
        "O",
        "--agents",
        "brokkr,bifrost",
    ];
    let mut prompter = ScriptPrompter::quiet();
    let error = onboard(
        args(&state_dir, &manager, &extra),
        secrets,
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Choose either an OpenAI API key or a Codex access token"
    );
    assert_eq!(manager.state().ops, [Op::Profile]);

    let mut prompter = ScriptPrompter::quiet();
    let summary = onboard(
        args(&state_dir, &manager, &extra),
        env(&owner),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap();
    let names: Vec<_> = summary["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].clone())
        .collect();
    assert_eq!(names, [json!("Bifrost")]);
    assert!(prompter
        .transcript
        .join("\n")
        .contains("Already in your Buzz app: Brokkr"));
    let actions = summary["action_needed"].as_array().unwrap();
    assert_eq!(actions.iter().filter(|a| *a == malformed).count(), 1);
    assert_eq!(summary["paseo"]["provisioned"], false);
}

/// Without the gateway SDK the grant is never faked: the exact command is
/// printed and listed, nothing is published or saved as granted, and a
/// daemon key that is the owner's own is refused.
#[tokio::test]
async fn the_gateway_grant_is_never_faked_before_the_sdk_lands() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("buzz");
    let owner = Keys::generate();
    let owner_hex = owner.public_key().to_hex();
    let relay = FakeRelay::with_invite(INVITE);
    let manager = FakeManager::for_developer(&owner_hex);
    let gateway = Keys::generate().public_key().to_hex();
    manager.state().paseo = Some(paseo(&gateway));
    let tools = tools();
    let extra = [
        "--yes",
        "--import-key",
        "--invite",
        INVITE,
        "--name",
        "O",
        "--agents",
        "brokkr",
        "--gateway-expires",
        "30D",
    ];
    for _ in 0..2 {
        let mut prompter = ScriptPrompter::quiet();
        let summary = onboard(
            args(&state_dir, &manager, &extra),
            env(&owner),
            &mut prompter,
            &relay,
            &manager,
            &tools,
        )
        .await
        .unwrap();
        let command = format!("buzz gateway grant {gateway} --expires 30d");
        let transcript = prompter.transcript.join("\n");
        assert!(transcript.contains("not available in this build yet"));
        assert!(transcript.contains(&command));
        assert!(transcript.contains(&format!(
            "buzz-admin add-member --pubkey {gateway} --role member"
        )));
        assert!(transcript.contains(&format!(
            "--pubkey {gateway}. The grant does not add membership."
        )));
        let actions = summary["action_needed"].as_array().unwrap();
        assert!(actions
            .iter()
            .any(|a| a.as_str().unwrap().ends_with(&command)));
        assert!(summary["gateway_grant"].is_null());
        assert!(relay.of_kind(30181).is_empty());
        assert!(Store::new(state_dir.clone())
            .load()
            .unwrap()
            .gateway_grant
            .is_none());
    }

    // A daemon key that is the owner's own is refused, even with the SDK.
    manager.state().paseo = Some(paseo(&owner_hex));
    let mut prompter = ScriptPrompter::quiet();
    let summary = onboard_with(
        Some(Box::new(FakeGrants)),
        args(&state_dir, &manager, &["--yes"]),
        EnvSecrets::default(),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap();
    assert!(relay.of_kind(30181).is_empty());
    assert!(summary["action_needed"][0]
        .as_str()
        .unwrap()
        .contains("is your own or an agent's key"));

    // Declining in the terminal grants nothing and says how to grant later.
    manager.state().paseo = Some(paseo(&gateway));
    let mut prompter = ScriptPrompter::scripted(vec![
        Answer::Secret(String::new()), // keep the Claude token on file
        Answer::Secret(String::new()), // no Codex token
        Answer::Secret(String::new()), // no OpenRouter key
        Answer::Text("n"),             // do not grant the gateway key
    ]);
    onboard_with(
        Some(Box::new(FakeGrants)),
        args(&state_dir, &manager, &[]),
        EnvSecrets::default(),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    )
    .await
    .unwrap();
    assert!(prompter.answers.is_empty());
    assert!(relay.of_kind(30181).is_empty());
    assert!(prompter.transcript.join("\n").contains(&format!(
        "Skipped. To grant it later, run: buzz gateway grant {gateway} --expires 90d"
    )));
}

/// Every enroll refusal, `paseo_store_failed` included, fails the run with
/// its code and guidance, adds no signing key to GitHub (the app adds it
/// only after the manager stores it), saves nothing as enrolled, and a
/// plain re-run then enrolls once.
#[tokio::test]
async fn every_enroll_refusal_is_reported_and_resumes() {
    let codes = profile::REFUSALS
        .iter()
        .map(|(code, _)| *code)
        .chain(["something_new"]);
    for code in codes {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("buzz");
        let owner = Keys::generate();
        let relay = FakeRelay::with_invite(INVITE);
        let manager = FakeManager::for_developer(&owner.public_key().to_hex());
        manager.state().paseo = Some(paseo(""));
        manager.state().enroll_refusal = Some(code.to_owned());
        let tools = tools();
        let extra = [
            "--yes",
            "--import-key",
            "--invite",
            INVITE,
            "--name",
            "O",
            "--agents",
            "brokkr",
        ];
        let mut prompter = ScriptPrompter::quiet();
        let error = onboard(
            args(&state_dir, &manager, &extra),
            env(&owner),
            &mut prompter,
            &relay,
            &manager,
            &tools,
        )
        .await
        .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains(&format!("Refused ({code})")),
            "{code}: {text}"
        );
        let reason = profile::REFUSALS.iter().find(|(known, _)| *known == code);
        if let Some((_, reason)) = reason {
            assert!(text.contains(reason), "{code}: {text}");
        }
        assert_eq!(
            matches!(error, CliError::Auth(_)),
            code == "unauthorized_owner",
            "{code}"
        );
        assert!(tools.gh_added.lock().unwrap().is_empty(), "{code}");
        let state = Store::new(state_dir.clone()).load().unwrap();
        assert!(
            state.enrolled_version.is_none() && state.pending_key.is_none(),
            "{code}"
        );
        assert!(!prompter.transcript.join("\n").contains(PASSWORD), "{code}");

        let mut prompter = ScriptPrompter::quiet();
        let summary = onboard(
            args(&state_dir, &manager, &["--yes"]),
            env(&owner),
            &mut prompter,
            &relay,
            &manager,
            &tools,
        )
        .await
        .unwrap();
        assert_eq!(manager.state().versions, 1, "{code}");
        assert_eq!(tools.gh_added.lock().unwrap().len(), 1, "{code}");
        assert_eq!(summary["paseo"]["provisioned"], true, "{code}");
        assert_eq!(summary["agents"][0]["registered"], true, "{code}");
    }
}

/// The pairing and grant-lifetime contracts, row by row.
#[test]
fn paseo_pairings_and_grant_lifetimes_are_checked() {
    let key = Keys::generate().public_key();
    let hex = key.to_hex();
    let with = |field: &str, value: Value| {
        let mut pairing = paseo("");
        pairing[field] = value;
        json!({"ok": true, "paseo": pairing})
    };
    let gateway = |reply: Value| paseo::pairing(&reply).map(|p| p.map(|p| p.gateway));
    let malformed = Err("The Sandbox manager's Paseo pairing was malformed.".to_owned());
    for (reply, expected) in [
        (json!({"ok": true}), Ok(None)),
        (json!({"ok": true, "paseo": null}), Ok(None)),
        (json!({"ok": true, "paseo": paseo("")}), Ok(Some(None))),
        (with("gateway_pubkey", Value::Null), Ok(Some(None))),
        (with("gateway_pubkey", json!(hex)), Ok(Some(Some(key)))),
        (
            with("gateway_pubkey", json!(key.to_bech32().unwrap())),
            malformed.clone(),
        ),
        (with("gateway_pubkey", json!(&hex[1..])), malformed.clone()),
        (
            with("gateway_pubkey", json!("z".repeat(64))),
            malformed.clone(),
        ),
        (with("gateway_pubkey", json!(7)), malformed.clone()),
        (
            with("offer_url", json!("https://example.com/#offer=x")),
            malformed.clone(),
        ),
        (with("password", json!("")), malformed.clone()),
    ] {
        assert_eq!(gateway(reply.clone()), expected, "{reply}");
    }
    for (input, expected) in [
        ("90d", Some(("90d", 90 * 86_400))),
        (" 36H ", Some(("36h", 36 * 3_600))),
        ("2w", Some(("2w", 14 * 86_400))),
        ("91d", None),
        ("365d", None),
        ("0d", None),
        ("90", None),
        ("d", None),
        ("-1d", None),
        ("9é", None),
        ("", None),
    ] {
        let got = gateway::lifetime(input).ok();
        assert_eq!(
            got.as_ref().map(|(text, secs)| (text.as_str(), *secs)),
            expected,
            "{input:?}"
        );
    }
    assert!(gateway::needs_renewal(100, 100, 90 * 86_400));
    assert!(gateway::needs_renewal(100 + 7 * 86_400, 100, 90 * 86_400));
    assert!(!gateway::needs_renewal(101 + 7 * 86_400, 100, 90 * 86_400));
    assert!(!gateway::needs_renewal(100 + 3 * 3_600, 100, 4 * 3_600));
}

/// Only wss:// community origins are accepted, before anything is written.
#[test]
fn only_wss_origins_are_accepted() {
    for (input, expected) in [
        ("wss://buzz.voltage.cloud", Some("wss://buzz.voltage.cloud")),
        (
            "wss://buzz.voltage.cloud/",
            Some("wss://buzz.voltage.cloud"),
        ),
        ("wss://relay.example:8443", Some("wss://relay.example:8443")),
        ("ws://buzz.voltage.cloud", None),
        ("https://buzz.voltage.cloud", None),
        ("http://localhost:3000", None),
        ("wss://buzz.voltage.cloud/path", None),
        ("wss://user@buzz.voltage.cloud", None),
        ("wss://buzz.voltage.cloud?x=1", None),
        ("buzz.voltage.cloud", None),
    ] {
        assert_eq!(canonical_relay(input).ok().as_deref(), expected, "{input}");
    }
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("buzz");
    let manager = FakeManager::for_developer("");
    let relay = FakeRelay::default();
    let tools = tools();
    let mut prompter = ScriptPrompter::quiet();
    let refused = Wizard::new(
        args(
            &state_dir,
            &manager,
            &["--relay", "ws://buzz.voltage.cloud"],
        ),
        EnvSecrets::default(),
        Store::new(state_dir.clone()),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    );
    assert!(matches!(refused, Err(CliError::Usage(_))));
    let refused = Wizard::new(
        args(&state_dir, &manager, &["--gateway-expires", "0d"]),
        EnvSecrets::default(),
        Store::new(state_dir.clone()),
        &mut prompter,
        &relay,
        &manager,
        &tools,
    );
    assert!(matches!(refused, Err(CliError::Usage(_))));
    assert!(!state_dir.exists());
}

/// The model credential rules, including both mutual exclusions.
#[test]
fn model_credentials_follow_the_profile_rules() {
    let cases: [(Value, Result<(), &str>); 7] = [
        (
            json!({"claude_oauth_token": CLAUDE, "codex_access_token": CODEX}),
            Ok(()),
        ),
        (json!({"openrouter_api_key": "sk-or-1"}), Ok(())),
        (
            json!({"codex_access_token": "sk-not-at"}),
            Err("Use a Codex access token (at-...)"),
        ),
        (
            json!({"openai_api_key": "k", "codex_access_token": CODEX}),
            Err("Choose either an OpenAI API key or a Codex access token"),
        ),
        (
            json!({"anthropic_api_key": "k", "claude_oauth_token": CLAUDE}),
            Err("Choose either a Claude API key or a Claude subscription token"),
        ),
        (
            json!({"claude_oauth_token": "has space"}),
            Err("Invalid credential"),
        ),
        (
            json!({"gemini": "k"}),
            Err("Unsupported model credential type"),
        ),
    ];
    for (models, expected) in cases {
        assert_eq!(
            profile::models(&models),
            expected.map_err(str::to_owned),
            "{models}"
        );
    }
}

/// The embedded via profile is the documented shared one, and valid.
#[test]
fn the_shared_via_profile_matches_the_docs() {
    let doc = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../voltage/remote-agents.md"),
    )
    .unwrap();
    let block = doc
        .split("### Shared via profile")
        .nth(1)
        .and_then(|rest| rest.split("```toml\n").nth(1))
        .and_then(|rest| rest.split("```").next())
        .unwrap();
    assert_eq!(block, SHARED_VIA_PROFILE);
    let partial = json!({"version": 1, "developer": "", "owner_pubkey": "a".repeat(64),
        "via": {"profile": SHARED_VIA_PROFILE}});
    assert_eq!(profile::validate_partial(&partial), Ok(()));
}

/// Invite input, secret debug output and GPUI launch parity.
#[test]
fn small_contracts_hold() {
    for (input, expected) in [
        ("v2.abc", Some("v2.abc")),
        ("https://buzz.voltage.cloud/invite/v2.abc", Some("v2.abc")),
        ("  code_1-x ", Some("code_1-x")),
        ("https://buzz.voltage.cloud/", None),
        ("bad code", None),
    ] {
        assert_eq!(
            steps_identity::invite_code(input).ok().as_deref(),
            expected,
            "{input}"
        );
    }
    assert_eq!(
        format!("{:?}", Secret::new("nsec1secret".into())),
        "<redacted>"
    );
    let owner = Keys::generate();
    let mut agent = agents::create(agents::preset("openrouter").unwrap(), &owner).unwrap();
    let env = agents::policy_env(&agent);
    assert_eq!(env["BUZZ_AGENT_MODEL"], "qwen/qwen3-coder");
    assert_eq!(env["MCP_HOOK_SERVERS"], "*");
    assert!(!env.contains_key("BUZZ_ACP_EFFORT_LEVEL"));
    agent.runtime = "claude".into();
    agent.model = "claude-opus-5-5".into();
    assert_eq!(
        agents::policy_env(&agent)["ANTHROPIC_MODEL"],
        "claude-opus-5-5"
    );
    for preset in agents::PRESETS {
        assert!(["", "low", "medium", "high", "xhigh", "max"].contains(&preset.effort));
        assert!(agents::RUNTIMES.iter().any(|r| r.id == preset.runtime));
    }
    let mut all = Vec::new();
    strings(&json!({"a": ["b"]}), &mut all);
    assert_eq!(all, ["b"]);
}

/// The production HTTP boundary requests writer reads for every provisioning
/// lookup. A simulated lagging replica otherwise hides the existing agent.
#[tokio::test]
async fn provisioning_reads_see_writer_state() {
    use super::relay::{HttpRelay, Relay};
    use axum::{routing::post, Json, Router};
    let owner = Keys::generate();
    let agent = nostr::EventBuilder::new(nostr::Kind::Custom(30177), "existing agent")
        .tags([nostr::Tag::parse(["d", &Keys::generate().public_key().to_hex()]).unwrap()])
        .sign_with_keys(&owner)
        .unwrap();
    let expected = serde_json::to_value(agent).unwrap();
    let writer = expected.clone();
    let app = Router::new().route(
        "/query",
        post(move |Json(filters): Json<Value>| {
            let writer = writer.clone();
            async move {
                let strong = filters
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|f| f["consistency"] == "strong");
                Json(if strong { json!([writer]) } else { json!([]) })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let relay = HttpRelay::new(&format!("http://{address}"));
    let events = Relay::query(&relay, &owner, json!({"kinds": [30177], "limit": 1})).await;
    server.abort();
    assert_eq!(events.unwrap(), vec![expected]);
}
