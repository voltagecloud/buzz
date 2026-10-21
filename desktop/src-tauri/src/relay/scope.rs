use super::relay_http_base_url;

// The fail-closed tenant checks live in `buzz-remote-sessions`, which applies
// them to remote-session payloads; every desktop side effect uses the same
// functions.
pub use buzz_remote_sessions::{assert_expected_relay_scope, assert_expected_signer};

/// A workspace-relay read that has passed the caller-captured scope check.
///
/// The only constructor is [`bind_expected_relay_scope`], so any side effect
/// that takes this type is proven — by construction — to consume the exact
/// value the check passed on, never a re-read of the mutable override. This
/// closes the check/use gap where a workspace switch landing between a scope
/// assertion and the side effect retargets it to a tenant the caller never
/// validated.
#[derive(Debug)]
pub struct ScopedWorkspaceRelay(String);

impl ScopedWorkspaceRelay {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validate a caller-captured relay scope against one workspace-relay read
/// and bind that exact read for the side effect to consume.
///
/// `None` preserves the unscoped behavior for callers without a tenant
/// boundary — the read is still bound so the side effect stays single-read.
pub fn bind_expected_relay_scope(
    expected_relay_url: Option<&str>,
    workspace_relay_url: String,
) -> Result<ScopedWorkspaceRelay, String> {
    assert_expected_relay_scope(
        expected_relay_url,
        &relay_http_base_url(&workspace_relay_url),
    )?;
    Ok(ScopedWorkspaceRelay(workspace_relay_url))
}

/// A workspace-signer read that has passed the caller-captured identity check.
///
/// The only constructor is [`bind_expected_signer`], so side effects consume
/// the exact owner read that was validated rather than a stale pre-await value.
#[derive(Debug)]
pub struct ScopedWorkspaceSigner(String);

impl ScopedWorkspaceSigner {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validate a caller-captured signer against one active-owner read and bind
/// that exact read for the side effect to consume. `None` preserves unscoped
/// callers while still making the owner input single-read.
pub fn bind_expected_signer(
    expected_signer_pubkey: Option<&str>,
    actual_signer_hex: String,
) -> Result<ScopedWorkspaceSigner, String> {
    assert_expected_signer(expected_signer_pubkey, &actual_signer_hex)?;
    Ok(ScopedWorkspaceSigner(actual_signer_hex))
}

#[cfg(test)]
mod tests {
    use super::{bind_expected_relay_scope, bind_expected_signer};

    #[test]
    fn bound_scope_is_immune_to_a_switch_landing_after_the_bind() {
        // Models the round-7 startup race: the caller captured tenant A, the
        // post-preflight bind reads the workspace relay while it is still A,
        // and THEN the switch to B lands — after the check, before the spawn.
        // The spawn consumes the BOUND value, not a re-read, so the pair can
        // only ever be keyed to the tenant the caller validated; the switch
        // mutates state the spawn no longer consults.
        let mut workspace = "wss://tenant-a.example".to_string();
        let bound =
            bind_expected_relay_scope(Some("wss://tenant-a.example"), workspace.clone()).unwrap();
        workspace = "wss://tenant-b.example".to_string(); // the switch lands post-check
        assert_eq!(bound.as_str(), "wss://tenant-a.example");
        assert_ne!(
            bound.as_str(),
            workspace,
            "spawn input must be the checked value"
        );
    }

    #[test]
    fn bind_fails_closed_when_the_switch_lands_before_the_read() {
        // The switch landed during the preflight await, so the one workspace
        // read already sees tenant B: no relay may be released to the spawn.
        let error = bind_expected_relay_scope(
            Some("wss://tenant-a.example"),
            "wss://tenant-b.example".to_string(),
        )
        .unwrap_err();
        assert!(error.contains("active community changed"), "{error}");
    }

    #[test]
    fn bind_returns_the_exact_read_for_unscoped_callers() {
        let bound = bind_expected_relay_scope(None, "wss://anything.example".to_string()).unwrap();
        assert_eq!(bound.as_str(), "wss://anything.example");
    }

    // The round-7 pair-key regression moved to
    // `managed_agents::runtime::tests::production_spawn_key_derives_from_the_bound_relay_not_the_post_switch_workspace`,
    // which exercises `bound_runtime_key` — the seam production spawn keys on —
    // instead of reconstructing the derivation by hand here.

    #[test]
    fn signer_bind_fails_closed_after_same_relay_identity_switch() {
        let captured = nostr::Keys::generate().public_key().to_hex();
        let switched = nostr::Keys::generate().public_key().to_hex();
        let error = bind_expected_signer(Some(&captured), switched).unwrap_err();
        assert!(error.contains("active identity changed"), "{error}");
    }

    #[test]
    fn signer_bind_returns_exact_read_for_scoped_and_unscoped_callers() {
        let actual = nostr::Keys::generate().public_key().to_hex();
        let scoped = bind_expected_signer(Some(&actual), actual.clone()).unwrap();
        assert_eq!(scoped.as_str(), actual);

        let unscoped_actual = nostr::Keys::generate().public_key().to_hex();
        let unscoped = bind_expected_signer(None, unscoped_actual.clone()).unwrap();
        assert_eq!(unscoped.as_str(), unscoped_actual);
    }
}
