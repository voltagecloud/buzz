//! Capturing the actual CLI must never expose the pairing password.
#[test]
fn pairing_refuses_noninteractive_capture_and_delegated_auth() {
    let owner = nostr::Keys::generate();
    let agent = nostr::Keys::generate();
    let auth = buzz_sdk::nip_oa::compute_auth_tag(&owner, &agent.public_key(), "").unwrap();
    for delegated in [false, true] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_buzz"));
        command
            .args([
                "paseo",
                "pairing",
                "--manager",
                &owner.public_key().to_hex(),
            ])
            .env("BUZZ_RELAY_URL", "wss://127.0.0.1:9")
            .env("BUZZ_PRIVATE_KEY", agent.secret_key().to_secret_hex())
            .env_remove("BUZZ_AUTH_TAG");
        if delegated {
            command.env("BUZZ_AUTH_TAG", &auth);
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(
            stderr.contains(if delegated {
                "direct signing key"
            } else {
                "interactive stderr"
            }),
            "{stderr}"
        );
    }
}
