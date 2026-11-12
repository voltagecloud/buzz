//! Pairing generation and the offer URL, against vectors from the daemon
//! image itself (`ghcr.io/getpaseo/paseo@sha256:57048c43…`). The Secret
//! write and the enroll round trip are in `enroll_tests.rs`.

use super::*;

/// `nacl.box.keyPair.fromSecretKey(secret)` in the image's tweetnacl:
///
/// ```sh
/// docker run --rm --entrypoint node ghcr.io/getpaseo/paseo@sha256:57048c43… -e '
///   const nacl = require("/usr/local/lib/node_modules/@getpaseo/relay/node_modules/tweetnacl");
///   const kp = nacl.box.keyPair.fromSecretKey(Buffer.from(process.argv[1], "hex"));
///   console.log(Buffer.from(kp.publicKey).toString("base64"))' <secret hex>
/// ```
///
/// The first secret is RFC 7748 §6.1's Alice key. The second needs clamping
/// at both ends, so an unclamped multiplication fails it.
const KEYPAIRS: &[(&str, &str)] = &[
    (
        "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
        "hSDwCYkwp1R0i33ctD73Wg2/Og0mOBr066SpjqqbTmo=",
    ),
    (
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "hHwNLDdSNPNl5mCVUYejc1oPdhPRYJ06ak2MU66qWiI=",
    ),
];

fn secret(hex: &str) -> [u8; 32] {
    let mut secret = [0u8; 32];
    for (i, byte) in secret.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    secret
}

#[test]
fn the_keypair_matches_tweetnacl() {
    for (hex, public) in KEYPAIRS {
        let secret = secret(hex);
        assert_eq!(
            keypair(&secret),
            json!({"v": 2, "publicKeyB64": public, "secretKeyB64": STANDARD.encode(secret)}),
            "{hex}"
        );
    }
}

/// The daemon's own `createConnectionOfferV2` and `encodeOfferToFragmentUrl`
/// (`dist/server/server/connection-offer.js`), run in the image with
/// `serverId: "srv_AAECAwQFBgcI"`, the first vector's public key, each
/// endpoint, `useTls: true` and `appBaseUrl: "https://app.paseo.sh"`:
///
/// ```sh
/// docker run --rm --entrypoint node ghcr.io/getpaseo/paseo@sha256:57048c43… --input-type=module -e '
///   import { createConnectionOfferV2, encodeOfferToFragmentUrl } from
///     "/usr/local/lib/node_modules/@getpaseo/server/dist/server/server/connection-offer.js";
///   const offer = await createConnectionOfferV2({ serverId: "srv_AAECAwQFBgcI",
///     daemonPublicKeyB64: "hSDw…", relay: { endpoint: "<endpoint>", useTls: true } });
///   console.log(encodeOfferToFragmentUrl({ offer, appBaseUrl: "https://app.paseo.sh" }))'
/// ```
#[test]
fn the_offer_url_matches_the_daemons() {
    for (endpoint, url) in [
        (
            RELAY_ENDPOINT,
            "https://app.paseo.sh/#offer=eyJ2IjoyLCJzZXJ2ZXJJZCI6InNydl9BQUVDQXdRRkJnY0kiLCJkYWVtb25QdWJsaWNLZXlCNjQiOiJoU0R3Q1lrd3AxUjBpMzNjdEQ3M1dnMi9PZzBtT0JyMDY2U3BqcXFiVG1vPSIsInJlbGF5Ijp7ImVuZHBvaW50IjoiYnV6ei52b2x0YWdlLmNsb3VkOjQ0MyIsInVzZVRscyI6dHJ1ZX19",
        ),
        (
            "paseo.example.test:8443",
            "https://app.paseo.sh/#offer=eyJ2IjoyLCJzZXJ2ZXJJZCI6InNydl9BQUVDQXdRRkJnY0kiLCJkYWVtb25QdWJsaWNLZXlCNjQiOiJoU0R3Q1lrd3AxUjBpMzNjdEQ3M1dnMi9PZzBtT0JyMDY2U3BqcXFiVG1vPSIsInJlbGF5Ijp7ImVuZHBvaW50IjoicGFzZW8uZXhhbXBsZS50ZXN0Ojg0NDMiLCJ1c2VUbHMiOnRydWV9fQ",
        ),
    ] {
        assert_eq!(offer_url("srv_AAECAwQFBgcI", KEYPAIRS[0].1, endpoint), url);
    }
}

#[test]
fn relay_endpoints_are_host_and_port_only() {
    for good in [RELAY_ENDPOINT, "paseo.example.test:8443", "10.0.0.1:443"] {
        assert!(is_relay_endpoint(good), "{good}");
    }
    for bad in [
        "buzz.voltage.cloud",
        "wss://buzz.voltage.cloud:443",
        "buzz.voltage.cloud:443/ws",
        "user@buzz.voltage.cloud:443",
        ":443",
        "buzz.voltage.cloud:0",
        "buzz.voltage.cloud:65536",
        "",
    ] {
        assert!(!is_relay_endpoint(bad), "{bad}");
    }
}

/// The daemon gets only its identity, agents, and selected model tokens.
#[test]
fn daemon_environment_preserves_identity_and_refuses_malformed_saved_fields() {
    let mut profile = crate::profile::tests::valid("dev", &"a".repeat(64));
    let agents = json!([]);
    let first = daemon_env(&profile, &agents, None).unwrap();
    assert_eq!(first.len(), 5);
    let keys = nostr::Keys::parse(&first["BUZZ_PRIVATE_KEY"]).unwrap();
    assert_eq!(
        pairing(&first, RELAY_ENDPOINT).unwrap()["gateway_pubkey"],
        keys.public_key().to_hex()
    );
    profile["models"] = json!({"claude_oauth_token": "sk-ant-oat01-fixture", "codex_access_token": "at-fixture",
        "openai_api_key": "sk-fixture", "openrouter_api_key": "sk-or-fixture"});
    let mut legacy = first.clone();
    legacy.insert("CODEX_ACCESS_TOKEN".into(), "legacy-unused-token".into());
    let next = daemon_env(&profile, &agents, Some(legacy)).unwrap();
    assert_eq!(next.len(), 6);
    assert!(!next.contains_key("CODEX_ACCESS_TOKEN"));
    assert_eq!(profile["models"]["codex_access_token"], "at-fixture");
    for key in [
        "PASEO_PASSWORD",
        "PASEO_SERVER_ID",
        "PASEO_DAEMON_KEYPAIR",
        "BUZZ_PRIVATE_KEY",
    ] {
        assert_eq!(next[key], first[key]);
        let mut bad = first.clone();
        bad.insert(key.into(), "malformed".into());
        assert!(daemon_env(&profile, &agents, Some(bad)).is_err(), "{key}");
        let mut missing = first.clone();
        missing.remove(key);
        assert_eq!(
            daemon_env(&profile, &agents, Some(missing)),
            Err(Failed("invalid_pairing")),
            "{key}"
        );
    }
    let mut unexpected = first;
    unexpected.insert("GITHUB_TOKEN".into(), "sensitive".into());
    assert_eq!(
        daemon_env(&profile, &agents, Some(unexpected)),
        Err(Failed("unsupported_secret_key"))
    );
}

/// IDs the daemon accepts, unique and never a provider the image defines;
/// labels from the launch's display name, the last precedence tier winning.
#[test]
fn agent_ids_are_valid_unique_and_never_reserved() {
    let key = |c: char| c.to_string().repeat(64);
    let channel = "0b6c1f0e-2a7d-4c1e-9f3a-5d2b8e4c7a10".to_owned();
    let entries = vec![
        ("Brokkr".to_owned(), key('2'), channel.clone()),
        ("Brokkr".to_owned(), key('1'), channel.clone()),
        ("Claude".to_owned(), key('3'), channel.clone()),
        ("Cursor".to_owned(), key('7'), channel.clone()),
        ("9 Lives!".to_owned(), key('4'), channel.clone()),
        ("Ünïcode only".to_owned(), key('5'), channel.clone()),
        ("日本".to_owned(), key('6'), channel.clone()),
    ];
    let ids: Vec<_> = agent_list(entries)
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        [
            "agent-9-lives",
            "brokkr",
            "brokkr-22222222",
            "claude-33333333",
            "cursor-77777777",
            "n-code-only",
            "agent"
        ]
    );
    let payload = json!({"env_vars": {"BUZZ_ACP_DISPLAY_NAME": "Old"},
        "launch": {"policy_env": {"BUZZ_ACP_DISPLAY_NAME": " Policy "},
            "env": {"BUZZ_ACP_DISPLAY_NAME": "  "}}});
    assert_eq!(label(&payload, &key('a')), "Policy");
    assert_eq!(label(&json!({}), &key('a')), "Agent aaaaaaaa");
}

/// Generated pairings pass the profile validation, carry a public key that
/// belongs to their secret, and never repeat.
#[test]
fn generated_pairings_are_valid_consistent_and_fresh() {
    let first = generate().unwrap();
    let profile = crate::profile::tests::valid("dev", &"a".repeat(64));
    crate::profile::validate(&profile).unwrap();
    let decoded = STANDARD
        .decode(first["daemon_keypair"]["secretKeyB64"].as_str().unwrap())
        .unwrap();
    let secret: [u8; 32] = decoded.try_into().unwrap();
    assert_eq!(first["daemon_keypair"], keypair(&secret));
    let password = URL_SAFE_NO_PAD
        .decode(first["password"].as_str().unwrap())
        .unwrap();
    assert_eq!(password.len(), 32);
    let second = generate().unwrap();
    for field in ["password", "server_id", "daemon_keypair"] {
        assert_ne!(first[field], second[field], "{field}");
    }
}

#[test]
fn existing_partial_pairing_never_generates_a_replacement_identity() {
    let profile = crate::profile::tests::valid("dev", &"a".repeat(64));
    let agents = json!([]);
    let first = daemon_env(&profile, &agents, None).unwrap();
    for mask in 0..7 {
        let mut partial = first.clone();
        for (index, key) in ["PASEO_PASSWORD", "PASEO_SERVER_ID", "PASEO_DAEMON_KEYPAIR"]
            .iter()
            .enumerate()
        {
            if mask & (1 << index) == 0 {
                partial.remove(*key);
            }
        }
        assert_eq!(
            daemon_env(&profile, &agents, Some(partial)),
            Err(Failed("invalid_pairing")),
            "partial mask {mask}"
        );
    }
    assert_eq!(
        daemon_env(&profile, &agents, Some(BTreeMap::new())),
        Err(Failed("invalid_pairing"))
    );
}

#[test]
fn narrow_git_read_token_is_preserved_without_copying_the_sandbox_token() {
    let mut profile = crate::profile::tests::valid("dev", &"a".repeat(64));
    let agents = json!([]);
    let first = daemon_env(&profile, &agents, None).unwrap();
    assert!(!first.contains_key(GIT_READ_TOKEN_KEY));
    profile["github"]["token"] = json!("github_pat_sandbox_write_fixture");
    let mut saved = first.clone();
    saved.insert(
        GIT_READ_TOKEN_KEY.into(),
        "github_pat_selected_read_fixture".into(),
    );
    let renewed = daemon_env(&profile, &agents, Some(saved.clone())).unwrap();
    assert_eq!(renewed[GIT_READ_TOKEN_KEY], saved[GIT_READ_TOKEN_KEY]);
    assert!(renewed
        .values()
        .all(|value| value != "github_pat_sandbox_write_fixture"));
    for invalid in [
        "",
        "gho_broad_fixture",
        "ghp_classic_fixture",
        "github_pat_",
        "github_pat_read\n",
        "github_pat_read!",
        "github_pat_read-token",
        "github_pat_read$()",
    ] {
        let mut bad = saved.clone();
        bad.insert(GIT_READ_TOKEN_KEY.into(), invalid.into());
        assert_eq!(
            daemon_env(&profile, &agents, Some(bad)),
            Err(Failed("invalid_git_read_token"))
        );
    }
    let public = pairing(&renewed, RELAY_ENDPOINT).unwrap().to_string();
    assert!(!public.contains("github_pat_"));
}
