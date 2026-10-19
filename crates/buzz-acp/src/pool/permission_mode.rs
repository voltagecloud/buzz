use super::{AcpClient, AcpError, PermissionMode, PERMISSION_MODE_TIMEOUT};

/// Read both current config options and the legacy mode advertisement.
pub(super) fn agent_supports_mode(session_new_result: &serde_json::Value, mode_wire: &str) -> bool {
    session_new_result
        .get("modes")
        .and_then(|m| m.get("availableModes"))
        .and_then(|a| a.as_array())
        .map(|modes| {
            modes
                .iter()
                .any(|m| m.get("id").and_then(|v| v.as_str()) == Some(mode_wire))
        })
        .unwrap_or(false)
        || session_new_result
            .get("configOptions")
            .and_then(|options| options.as_array())
            .is_some_and(|options| {
                options.iter().any(|option| {
                    option
                        .get("configId")
                        .or_else(|| option.get("id"))
                        .and_then(|id| id.as_str())
                        == Some("mode")
                        && option
                            .get("options")
                            .and_then(|values| values.as_array())
                            .is_some_and(|values| {
                                values.iter().any(|value| {
                                    value.get("value").and_then(|value| value.as_str())
                                        == Some(mode_wire)
                                })
                            })
                })
            })
}

/// Set the requested mode. Full access requires explicit confirmation;
/// other modes may fall back to per-tool approval on adapter rejection.
/// Transport errors always propagate so the caller can respawn the adapter.
pub(super) async fn apply_permission_mode(
    acp: &mut AcpClient,
    session_id: &str,
    mode: &PermissionMode,
) -> Result<(), AcpError> {
    let wire = mode.as_wire_str();
    let result = tokio::time::timeout(PERMISSION_MODE_TIMEOUT, async {
        acp.session_set_config_option(session_id, "mode", wire)
            .await
    })
    .await;

    match result {
        Ok(Ok(result)) => {
            if *mode == PermissionMode::AgentFullAccess
                && !result
                    .get("configOptions")
                    .and_then(|v| v.as_array())
                    .is_some_and(|options| {
                        options.iter().any(|option| {
                            option
                                .get("configId")
                                .or_else(|| option.get("id"))
                                .and_then(|v| v.as_str())
                                == Some("mode")
                                && option.get("currentValue").and_then(|v| v.as_str()) == Some(wire)
                        })
                    })
            {
                return Err(AcpError::PermissionModeUnavailable(
                    "the adapter did not confirm agent-full-access".into(),
                ));
            }
            tracing::info!(
                target: "pool::permission",
                "applied permission mode {wire:?} on session {session_id}"
            );
        }
        // Transport-class errors may have corrupted the stdio stream — propagate
        // so the caller can respawn the agent.
        Ok(Err(e @ AcpError::Io(_)))
        | Ok(Err(e @ AcpError::WriteTimeout(_)))
        | Ok(Err(e @ AcpError::Timeout(_)))
        | Ok(Err(e @ AcpError::Protocol(_)))
        | Ok(Err(e @ AcpError::AgentExited)) => {
            tracing::error!(
                target: "pool::permission",
                "fatal error setting permission mode {wire:?}: {e}"
            );
            return Err(e);
        }
        // Application-level errors — agent is fine, just uses default permission mode.
        Ok(Err(e)) => {
            if *mode == PermissionMode::AgentFullAccess {
                return Err(AcpError::PermissionModeUnavailable(format!(
                    "the adapter rejected agent-full-access: {e}"
                )));
            }
            tracing::warn!(
                target: "pool::permission",
                "failed to set permission mode {wire:?}: {e} — falling back to per-tool auto-approval"
            );
        }
        Err(_) => {
            // Outer timeout fired — stream may be in unknown state.
            tracing::error!(
                target: "pool::permission",
                "permission mode set timed out ({PERMISSION_MODE_TIMEOUT:?}) — treating as fatal"
            );
            return Err(AcpError::Timeout(PERMISSION_MODE_TIMEOUT));
        }
    }
    Ok(())
}

#[cfg(test)]
mod advertisement_tests {
    use super::agent_supports_mode;
    use serde_json::json;

    #[test]
    fn full_access_accepts_both_config_identifiers_without_legacy_modes() {
        for option in [
            json!({"id": "mode", "options": [{"value": "agent-full-access"}]}),
            json!({"configId": "mode", "options": [{"value": "agent-full-access"}]}),
        ] {
            assert!(agent_supports_mode(
                &json!({"configOptions": [option]}),
                "agent-full-access"
            ));
        }
    }

    #[test]
    fn full_access_requires_an_advertised_choice_for_the_mode_option() {
        for option in [
            json!({"configId": "mode", "options": [{"value": "default"}]}),
            json!({"configId": "model", "id": "mode", "options": [{"value": "agent-full-access"}]}),
            json!({"configId": "mode", "currentValue": "agent-full-access"}),
            json!({"options": [{"value": "agent-full-access"}]}),
        ] {
            assert!(!agent_supports_mode(
                &json!({"configOptions": [option]}),
                "agent-full-access"
            ));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Reply to the single `session/set_config_option` request with `reply`.
    async fn apply_with_reply(mode: PermissionMode, reply: &str) -> Result<(), AcpError> {
        let script = format!(
            "read -r line; printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":0,{reply}}}'; sleep 5"
        );
        let mut acp = AcpClient::spawn("bash", &["-c".to_string(), script], &[], false)
            .await
            .expect("spawn ACP script");
        let result = apply_permission_mode(&mut acp, "sess-1", &mode).await;
        acp.shutdown().await;
        result
    }

    #[tokio::test]
    async fn container_mode_requires_confirmation_and_never_falls_back() {
        for confirmed in [
            r#""result":{"configOptions":[{"id":"mode","currentValue":"agent-full-access"}]}"#,
            r#""result":{"configOptions":[{"configId":"mode","currentValue":"agent-full-access"}]}"#,
        ] {
            assert!(apply_with_reply(PermissionMode::AgentFullAccess, confirmed)
                .await
                .is_ok());
        }
        for reply in [
            r#""result":{}"#,
            r#""result":{"configOptions":[{"configId":"mode","currentValue":"default"}]}"#,
            r#""result":{"configOptions":[{"configId":"model","id":"mode","currentValue":"agent-full-access"}]}"#,
            r#""result":{"configOptions":[{"id":"mode","currentValue":"default"}]}"#,
            r#""error":{"code":-32602,"message":"unsupported mode"}"#,
        ] {
            assert!(
                matches!(
                    apply_with_reply(PermissionMode::AgentFullAccess, reply).await,
                    Err(AcpError::PermissionModeUnavailable(_))
                ),
                "{reply}"
            );
        }
        // Other modes keep the per-tool auto-approval fallback.
        assert!(apply_with_reply(
            PermissionMode::BypassPermissions,
            r#""error":{"code":-32602,"message":"unsupported mode"}"#
        )
        .await
        .is_ok());
    }
}
