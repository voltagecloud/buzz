use super::*;
use serde_json::json;

/// An agent signing key in the shape `ssh-keygen -t ed25519` writes.
pub(crate) const SSH_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQ==\nAAAAMwAAAAtzc2gtZWQyNTUxOQ\n-----END OPENSSH PRIVATE KEY-----\n";

/// A profile `profile.py` accepts, with fixture secrets.
pub(crate) fn valid(developer: &str, owner: &str) -> Value {
    json!({
        "version": 1,
        "developer": developer,
        "owner_pubkey": owner,
        "git": {"name": "Ada Lovelace", "email": "ada@example.com",
            "ssh_signing_key": SSH_KEY},
        "github": {"login": "ada-l", "token": "ghp_fixtureGitHubTokenDoNotLeak"},
        "via": {"profile": "[providers.op]\nkind = \"1password\"\n\n[services.github]\nprovider = \"op\"\n",
            "op_token": "ops_fixtureOnePasswordTokenDoNotLeak"},
        "models": {"anthropic_api_key": "sk-ant-fixtureModelKeyDoNotLeak"}
    })
}

fn profile() -> Value {
    valid("ada", &"a".repeat(64))
}

fn refused(pointer: &str, bad: Value) -> Result<(), Invalid> {
    let mut value = profile();
    let (parent, field) = pointer.rsplit_once('/').unwrap();
    value.pointer_mut(parent).unwrap()[field] = bad;
    validate(&value)
}

#[test]
fn accepts_a_complete_profile() {
    validate(&profile()).unwrap();
    let mut value = profile();
    value["models"] = json!({});
    validate(&value).unwrap();
    value["models"] = json!({"openai_api_key": "sk-x", "claude_oauth_token": "t"});
    validate(&value).unwrap();
    // Both subscriptions, for Codex and Claude Code in parallel.
    value["models"] = json!({"codex_access_token": "at-x", "claude_oauth_token": "t"});
    validate(&value).unwrap();
    value["models"] = json!({"openrouter_api_key": "sk-or-x", "anthropic_api_key": "k"});
    validate(&value).unwrap();
    // The final newline is optional, as `profile.py`'s `\n?` allows.
    value["git"]["ssh_signing_key"] = json!(SSH_KEY.trim_end_matches('\n'));
    validate(&value).unwrap();
}

#[test]
fn each_section_needs_exactly_its_fields() {
    const FIELDS: &str = "Profile fields are incomplete or unsupported";
    let mut extra = profile();
    extra["extra"] = json!(true);
    assert_eq!(validate(&extra), Err(Invalid(FIELDS)));
    let mut missing = profile();
    missing.as_object_mut().unwrap().remove("via");
    assert_eq!(validate(&missing), Err(Invalid(FIELDS)));
    assert_eq!(validate(&json!([])), Err(Invalid(FIELDS)));
    for (pointer, bad) in [
        ("/git", json!({"name": "a", "email": "b"})),
        ("/github", json!({"login": "a", "token": "b", "scope": "c"})),
        ("/via", json!("profile")),
    ] {
        assert_eq!(refused(pointer, bad), Err(Invalid(FIELDS)), "{pointer}");
    }
}

/// Every rule, with a value just outside it, and the message `profile.py`
/// gives for it.
#[test]
fn refuses_each_rule_with_its_message() {
    let cases = [
        ("/version", json!(2), "Unsupported profile version"),
        ("/version", json!(1.0), "Unsupported profile version"),
        ("/version", json!(true), "Unsupported profile version"),
        ("/version", json!("1"), "Unsupported profile version"),
        ("/developer", json!("Ada"), "Invalid developer ID"),
        ("/developer", json!("-ada"), "Invalid developer ID"),
        ("/developer", json!("ada-"), "Invalid developer ID"),
        ("/developer", json!("a".repeat(33)), "Invalid developer ID"),
        ("/owner_pubkey", json!("A".repeat(64)), "Invalid Buzz owner"),
        ("/owner_pubkey", json!("a".repeat(63)), "Invalid Buzz owner"),
        ("/git/name", json!(""), "Invalid Git identity"),
        ("/git/email", json!("a".repeat(257)), "Invalid Git identity"),
        ("/git/name", json!("Ada\nLovelace"), "Invalid Git identity"),
        ("/git/email", json!(7), "Invalid Git identity"),
        // The old OpenPGP fingerprint is not a signing key the Sandbox can use.
        (
            "/git/ssh_signing_key",
            json!("ABCDEF0123456789ABCDEF0123456789ABCDEF01"),
            "Invalid SSH signing key",
        ),
        (
            "/git/ssh_signing_key",
            json!("-----BEGIN OPENSSH PRIVATE KEY-----\n-----END OPENSSH PRIVATE KEY-----\n"),
            "Invalid SSH signing key",
        ),
        (
            "/git/ssh_signing_key",
            json!(
                "-----BEGIN OPENSSH PRIVATE KEY-----\nab cd\n-----END OPENSSH PRIVATE KEY-----\n"
            ),
            "Invalid SSH signing key",
        ),
        (
            "/git/ssh_signing_key",
            json!(format!(
                "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
                "a".repeat(4000)
            )),
            "Invalid SSH signing key",
        ),
        (
            "/git/ssh_signing_key",
            json!("-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----\n"),
            "Invalid SSH signing key",
        ),
        ("/github/login", json!("-ada"), "Invalid GitHub login"),
        (
            "/github/login",
            json!("a".repeat(40)),
            "Invalid GitHub login",
        ),
        ("/github/login", json!("ada_l"), "Invalid GitHub login"),
        ("/github/token", json!(""), "Invalid credential"),
        ("/github/token", json!("a b"), "Invalid credential"),
        (
            "/github/token",
            json!("a".repeat(8193)),
            "Invalid credential",
        ),
        // Python's str.isspace includes U+001C to U+001F; Rust's does not.
        ("/github/token", json!("a\u{1f}b"), "Invalid credential"),
        ("/github/token", json!("a\u{a0}b"), "Invalid credential"),
        ("/via/op_token", json!(null), "Invalid credential"),
        ("/via/profile", json!(7), "Invalid via profile"),
        ("/via/profile", json!("not = [toml"), "Invalid via profile"),
        (
            "/via/profile",
            json!("[providers.op]\nkind = \"x\"\n"),
            "via profile requires providers and services",
        ),
        (
            "/via/profile",
            json!("services = []\nproviders = 1\n"),
            "via profile requires providers and services",
        ),
        // The vault, not the profile, holds the shared Depot credential.
        (
            "/depot",
            json!({"organization": "voltage", "token": "t"}),
            "Profile fields are incomplete or unsupported",
        ),
        ("/models", json!([]), "Unsupported model credential type"),
        (
            "/models",
            json!({"gemini_api_key": "k"}),
            "Unsupported model credential type",
        ),
        (
            "/models",
            json!({"anthropic_api_key": "k", "claude_oauth_token": "t"}),
            "Choose either a Claude API key or a Claude subscription token",
        ),
        (
            "/models",
            json!({"openai_api_key": "k", "codex_access_token": "at-t"}),
            "Choose either an OpenAI API key or a Codex access token",
        ),
        (
            "/models",
            json!({"codex_access_token": "sk-proj-k"}),
            "Use a Codex access token (at-...)",
        ),
        (
            "/models",
            json!({"codex_access_token": "at t"}),
            "Invalid credential",
        ),
        (
            "/models",
            json!({"openai_api_key": ""}),
            "Invalid credential",
        ),
    ];
    for (pointer, bad, message) in cases {
        assert_eq!(
            refused(pointer, bad.clone()),
            Err(Invalid(message)),
            "{pointer} = {bad}"
        );
    }
}

/// The limit is on `json.dumps` output, where every non-ASCII character
/// takes six bytes: a profile well under 64 KiB as UTF-8 can exceed it.
#[test]
fn the_size_limit_counts_bytes_as_python_writes_them() {
    let toml = |comment: &str| format!("services = 1\nproviders = 1\n# {comment}\n");
    let mut value = profile();
    value["via"]["profile"] = json!(toml(&"\u{20ac}".repeat(11_000)));
    assert!(serde_json::to_string(&value).unwrap().len() < MAX_BYTES);
    assert_eq!(
        validate(&value),
        Err(Invalid("Profile exceeds Secrets Manager size limit"))
    );
    value["via"]["profile"] = json!(toml(&"x".repeat(60_000)));
    validate(&value).unwrap();
    value["via"]["profile"] = json!(toml(&"x".repeat(66_000)));
    assert!(validate(&value).is_err());
}

/// Byte counts for the separators and escapes Python's `json.dumps` writes.
#[test]
fn python_json_length_matches_json_dumps() {
    // Python writes this as 57 bytes: a two-byte newline escape, six-byte
    // escapes for the accented letter and DEL, and a twelve-byte surrogate pair.
    let value = json!({"a": [1, true, null], "b": "\n\u{e9}\u{1f600}\u{7f}"});
    assert_eq!(python_json_len(&value), 57);
}

/// A partial profile may omit any section or key, but not add one, and a
/// model credential is a string to set or `null` to remove.
#[test]
fn a_partial_profile_may_omit_fields_but_not_add_them() {
    let base = || json!({"version": 1, "owner_pubkey": "a".repeat(64)});
    assert_eq!(check_partial(&base()), Ok(()));
    let mut partial = base();
    partial["models"] = json!({"codex_access_token": "at-x", "claude_oauth_token": null});
    partial["git"] = json!({"email": "ada@example.com"});
    assert_eq!(check_partial(&partial), Ok(()));
    for (pointer, bad, message) in [
        (
            "/models",
            json!({"codex_access_token": 1}),
            "Unsupported model credential type",
        ),
        (
            "/models",
            json!({"gemini_api_key": "x"}),
            "Unsupported model credential type",
        ),
        (
            "/git",
            json!({"signing": "x"}),
            "Profile fields are incomplete or unsupported",
        ),
        (
            "/github",
            json!({"token": "ghp_x"}),
            "A GitHub token needs its login",
        ),
        ("/version", json!(2), "Unsupported profile version"),
    ] {
        let mut partial = base();
        partial[&pointer[1..]] = bad;
        assert_eq!(check_partial(&partial), Err(Invalid(message)), "{pointer}");
    }
}
