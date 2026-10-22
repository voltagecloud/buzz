use crate::process::spawn_with_busy_retry as spawn_provider;
use crate::SessionError;
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

const STDERR_CAP: usize = 65536;
/// Provider responses should be small JSON objects. Cap stdout to prevent a
/// buggy or malicious provider from OOM-ing the host process.
const STDOUT_CAP: usize = 1_048_576; // 1 MB
/// The provider protocol version this client speaks (spec §Info).
pub const PROTOCOL_VERSION: u64 = 1;
/// The `info.ops` entry a provider advertises when `deploy` accepts
/// `provider_config.sandbox.recovery`, so ended thread sessions can be
/// recovered.
const RECOVER_OP: &str = "recover";

/// A provider's validated `info` response.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Operations the provider advertises. A provider that omits `ops`
    /// answers only `info` and `deploy`.
    pub ops: Vec<String>,
    pub config_schema: serde_json::Value,
}

impl ProviderInfo {
    /// Whether the provider advertises `op` in `info.ops`.
    pub fn supports(&self, op: &str) -> bool {
        self.ops.iter().any(|advertised| advertised == op)
    }

    /// Whether the provider can end thread sessions (`stop`) and recover
    /// ended ones (`recover`). Session actions need both.
    pub fn supports_lifecycle(&self) -> bool {
        self.supports("stop") && self.supports(RECOVER_OP)
    }
}

/// The state a `stop` response reports for the thread's session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StopState {
    /// The stop is draining; the session ends once its Pod is gone.
    Ending,
    /// The session has ended.
    Ended,
    /// The thread has no session.
    Absent,
}

/// A provider's refusal code, if it is a well-formed snake_case token.
/// Anything else is dropped rather than passed to the host.
fn refusal_code(response: &serde_json::Value) -> Option<String> {
    response
        .get("code")
        .and_then(serde_json::Value::as_str)
        .filter(|code| {
            (1..=64).contains(&code.len())
                && code.starts_with(|c: char| c.is_ascii_lowercase())
                && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        })
        .map(String::from)
}

/// Validate an `info` response against protocol v1's flat wire shape.
fn parse_provider_info(info: &serde_json::Value) -> Result<ProviderInfo, String> {
    let object = info
        .as_object()
        .ok_or_else(|| "provider info response must be a JSON object".to_string())?;
    let actual_version = object
        .get("protocol_version")
        .and_then(serde_json::Value::as_u64);
    if actual_version != Some(PROTOCOL_VERSION) {
        return Err(match actual_version {
            Some(version) => format!(
                "unsupported provider protocol version {version}; this client requires {PROTOCOL_VERSION}"
            ),
            None => "provider info response missing integer protocol_version".to_string(),
        });
    }
    if object.get("ok") != Some(&serde_json::Value::Bool(true)) {
        return Err("provider info response must contain ok: true".to_string());
    }
    let text = |field: &str| {
        object
            .get(field)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(String::from)
            .ok_or_else(|| format!("provider info response missing non-empty string {field}"))
    };
    let (name, version, description) = (text("name")?, text("version")?, text("description")?);
    let config_schema = object
        .get("config_schema")
        .filter(|schema| schema.is_object())
        .cloned()
        .ok_or_else(|| "provider info response missing object config_schema".to_string())?;

    const FIELDS: &[&str] = &[
        "ok",
        "name",
        "version",
        "protocol_version",
        "ops",
        "description",
        "config_schema",
    ];
    if let Some(field) = object
        .keys()
        .find(|field| !FIELDS.contains(&field.as_str()))
    {
        return Err(format!(
            "provider info response contains unknown field {field}"
        ));
    }
    Ok(ProviderInfo {
        name,
        version,
        description,
        ops: advertised_ops(info)?,
        config_schema,
    })
}

/// Operations a provider advertises in `info.ops`. The field is additive
/// within protocol v1: a provider that omits it answers only `info` and
/// `deploy`, so it has no `stop` and no session recovery.
fn advertised_ops(info: &serde_json::Value) -> Result<Vec<String>, String> {
    const MAX_OPS: usize = 32;
    let Some(ops) = info.get("ops") else {
        return Ok(vec!["info".to_string(), "deploy".to_string()]);
    };
    ops.as_array()
        .filter(|ops| ops.len() <= MAX_OPS)
        .ok_or_else(|| format!("provider info ops must be an array of at most {MAX_OPS} strings"))?
        .iter()
        .map(|op| {
            op.as_str()
                .filter(|op| !op.is_empty())
                .map(String::from)
                .ok_or_else(|| "provider info ops must contain non-empty strings".to_string())
        })
        .collect()
}

/// The terminal state a `stop` response reports for the thread's session.
fn stop_response_state(response: &serde_json::Value) -> Result<StopState, String> {
    match response.get("state").and_then(serde_json::Value::as_str) {
        Some("ending") => Ok(StopState::Ending),
        Some("ended") => Ok(StopState::Ended),
        Some("absent") => Ok(StopState::Absent),
        Some(state) => Err(format!("stop response has unknown state {state:?}")),
        None => Err("stop response missing state".to_string()),
    }
}

/// The backend agent id a successful `deploy` response carries.
fn deployed_agent_id(response: &serde_json::Value) -> Result<String, String> {
    response["agent_id"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "deploy response missing agent_id".to_string())
}

/// The in-band failure a provider reported with `ok: false`, if any, with
/// `secrets` redacted from its message.
fn reported_failure(response: &serde_json::Value, secrets: &[&str]) -> Option<SessionError> {
    if response.get("ok").and_then(serde_json::Value::as_bool) != Some(false) {
        return None;
    }
    let error = response["error"].as_str().unwrap_or("unknown error");
    Some(SessionError {
        message: redact_secrets_with(error, secrets),
        code: refusal_code(response),
    })
}

/// Invoke a provider binary: write JSON to stdin, read JSON from stdout.
///
/// Reader threads stream lines/chunks over channels so the caller can receive
/// data as it arrives and time-box the wait. No `read_to_end` — if a provider
/// daemonizes or leaves descendants holding pipes open, the caller still gets
/// all data written before the child exited and returns without leaking threads
/// (the readers drop naturally when the sender is gone and the pipe closes or
/// the host process exits). The provider runs in `cwd` when one is given.
pub fn invoke_provider(
    binary: &Path,
    cwd: Option<&Path>,
    request: &serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, SessionError> {
    let request_bytes = format!(
        "{}\n",
        serde_json::to_string(request).map_err(|e| e.to_string())?
    );

    let mut cmd = std::process::Command::new(binary);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW: no console window flashes up for each call.
        cmd.creation_flags(0x0800_0000);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = spawn_provider(&mut cmd)
        .map_err(|e| format!("failed to spawn {}: {e}", binary.display()))?;

    // Write request and close stdin immediately so the provider sees EOF.
    let stdin_result = if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(request_bytes.as_bytes())
    } else {
        Ok(())
    };

    // Stream stdout as raw chunks over a channel. The caller appends chunks
    // to a buffer and attempts incremental JSON parsing — no dependency on
    // newlines or EOF. If a descendant holds the pipe open after the provider
    // exits, the thread blocks on the next read — but the caller already has
    // the response data and proceeds. The thread is not joined; it terminates
    // when the pipe eventually closes or the process exits.
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 8192];
            let mut reader = BufReader::new(stdout);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stdout_tx.send(buf[..n].to_vec()).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // Drain stderr into a bounded channel. sync_channel(8) caps in-flight
    // chunks — the producer blocks when the buffer is full, applying natural
    // backpressure. The consumer drains during the try_wait loop and caps
    // total bytes at STDERR_CAP, so memory is bounded even for long-running
    // or malicious providers.
    let (stderr_tx, stderr_rx) = mpsc::sync_channel::<Vec<u8>>(8);
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 8192];
            let mut reader = BufReader::new(stderr);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stderr_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // Bail early if stdin write failed — child may be in a bad state.
    if let Err(e) = stdin_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("stdin write failed: {e}").into());
    }

    // Poll try_wait with a deadline, collecting stdout chunks and draining
    // stderr as data arrives. Incremental JSON parsing on stdout means we
    // capture the response even without a trailing newline or EOF.
    let timeout_secs = timeout.as_secs();
    let deadline = std::time::Instant::now() + timeout;
    let mut stdout_buf = Vec::new();
    let mut stderr_bytes = Vec::new();
    let exit_status = loop {
        // Drain stdout chunks (non-blocking), enforce byte cap.
        while stdout_buf.len() < STDOUT_CAP {
            match stdout_rx.try_recv() {
                Ok(chunk) => stdout_buf.extend_from_slice(&chunk),
                Err(_) => break,
            }
        }
        // Drain stderr chunks (non-blocking), enforce byte cap.
        while stderr_bytes.len() < STDERR_CAP {
            match stderr_rx.try_recv() {
                Ok(chunk) => stderr_bytes.extend_from_slice(&chunk),
                Err(_) => break,
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                break status;
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("provider timed out after {timeout_secs}s").into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait error: {e}").into());
            }
        }
    };

    // Drain remaining stdout chunks buffered between last poll and child exit.
    // Keep draining until the channel disconnects (reader finished) or the
    // 2s deadline expires (descendant holding pipe open). Do NOT break on the
    // first timeout — a slightly delayed final chunk should still be captured.
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if stdout_buf.len() >= STDOUT_CAP {
            break;
        }
        let remaining = drain_deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match stdout_rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(chunk) => stdout_buf.extend_from_slice(&chunk),
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // reader done
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Keep waiting until the full drain deadline expires.
                if std::time::Instant::now() >= drain_deadline {
                    break;
                }
            }
        }
    }

    // Final stderr drain (non-blocking, cap already enforced).
    while stderr_bytes.len() < STDERR_CAP {
        match stderr_rx.try_recv() {
            Ok(chunk) => stderr_bytes.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }
    stderr_bytes.truncate(STDERR_CAP);
    stdout_buf.truncate(STDOUT_CAP);

    let stderr = String::from_utf8_lossy(&stderr_bytes);
    let env_secrets = env_secrets_from_request(request);
    let env_secret_refs: Vec<&str> = env_secrets.iter().map(String::as_str).collect();
    let stderr_redacted = redact_secrets_with(&stderr, &env_secret_refs);

    let exit_info = exit_status
        .code()
        .map(|c| format!("exit code {c}"))
        .unwrap_or_else(|| "killed by signal".to_string());

    // Fail on non-zero exit regardless of stdout content. A provider that
    // crashes mid-deploy may flush partial JSON before dying — trusting that
    // output would be worse than surfacing the failure.
    let exited_ok = exit_status.success();
    if !exited_ok {
        let stderr_snippet = &stderr_redacted[..stderr_redacted.len().min(4096)];
        if stderr_snippet.is_empty() {
            return Err(format!("provider failed ({exit_info}, empty stderr)").into());
        } else {
            return Err(format!("provider failed ({exit_info}). stderr: {stderr_snippet}").into());
        }
    }

    // Incremental JSON parse: try each line, then try the entire buffer.
    // Handles providers that emit JSON on a single line (common) as well as
    // providers that write JSON without a trailing newline.
    let stdout_str = String::from_utf8_lossy(&stdout_buf);
    let response: serde_json::Value = stdout_str
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .or_else(|| serde_json::from_str(stdout_str.trim()).ok())
        .ok_or_else(|| {
            let stderr_snippet = &stderr_redacted[..stderr_redacted.len().min(4096)];
            if stderr_snippet.is_empty() {
                format!("provider produced no JSON response ({exit_info}, empty stderr)")
            } else {
                format!(
                    "provider produced no JSON response ({exit_info}). stderr: {stderr_snippet}"
                )
            }
        })?;

    if let Some(failure) = reported_failure(&response, &env_secret_refs) {
        return Err(failure);
    }

    Ok(response)
}

/// Split a config key into lowercase words on `_`, `-`, `.`, and camelCase boundaries.
///
/// Handles acronyms: consecutive uppercase runs stay together until a lowercase follows.
/// "apiKey" → ["api", "key"], "apiKEY" → ["api", "key"], "APIKey" → ["api", "key"],
/// "access_token" → ["access", "token"], "keyboard" → ["keyboard"],
/// "clientSecret" → ["client", "secret"].
fn split_config_key(key: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = key.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        if ch == '_' || ch == '-' || ch == '.' {
            if !current.is_empty() {
                words.push(current.to_lowercase());
                current.clear();
            }
        } else if ch.is_uppercase() {
            // Start a new word on: (a) transition from lowercase to uppercase, or
            // (b) uppercase followed by lowercase (end of acronym run, e.g. "APIKey" → "API" + "Key").
            let prev_lower =
                !current.is_empty() && current.chars().last().is_some_and(|c| c.is_lowercase());
            let acronym_end = !current.is_empty()
                && current.chars().last().is_some_and(|c| c.is_uppercase())
                && chars.get(i + 1).is_some_and(|c| c.is_lowercase());
            if prev_lower || acronym_end {
                words.push(current.to_lowercase());
                current.clear();
            }
            current.push(ch);
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        words.push(current.to_lowercase());
    }
    words
}

#[cfg(test)]
fn redact_secrets(s: &str) -> String {
    redact_secrets_with(s, &[])
}

/// Like the (test-only) prefix-only `redact_secrets`, but also redacts
/// every occurrence of each
/// `extras` entry verbatim. Used to scrub user-supplied env values out of
/// provider stderr/JSON-error text — providers may echo their request
/// back in failure messages, and persona/agent `env_vars` may carry API
/// keys that a host may persist as an agent's last error.
///
/// Entries shorter than 4 chars are skipped: too noisy to scrub blindly
/// (would match every short token in normal log output). Entries are
/// applied in decreasing length order so superstrings get scrubbed before
/// substrings — protects against partial overlap leaks.
pub fn redact_secrets_with(s: &str, extras: &[&str]) -> String {
    let mut result = s.to_string();

    // Extras: longest first to avoid partial-overlap leaks. We use
    // `str::replace` (single-pass, non-overlapping) instead of a `find` +
    // `replace_range` loop — the loop variant would never terminate if a
    // user-supplied env value happened to be a substring of the
    // replacement marker (e.g. value="REDACTED" or "EDACTE").
    let mut sorted: Vec<&str> = extras.iter().copied().filter(|v| v.len() >= 4).collect();
    sorted.sort_by_key(|v| std::cmp::Reverse(v.len()));
    sorted.dedup();
    for value in sorted {
        if !value.is_empty() {
            result = result.replace(value, "[REDACTED]");
        }
    }

    // Then prefix-based scrubbing. This loop *can* re-scan because each
    // replacement shortens the buffer past the matched prefix — the
    // replacement marker `[REDACTED]` contains none of these prefixes, so
    // progress is guaranteed. Any prefix added here must preserve that.
    //
    // GitHub tokens are recognised by shape as well as by variable name: a
    // token reaches output from outside our environment too — embedded in a
    // git remote URL an installer echoes, say — where no name-based rule can
    // see it.
    for prefix in &[
        "nsec1",
        "sprt_tok_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
    ] {
        while let Some(pos) = result.find(prefix) {
            let end = result[pos..]
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .map(|i| pos + i)
                .unwrap_or(result.len());
            result.replace_range(pos..end, "[REDACTED]");
        }
    }
    result
}

/// Collect string values from every environment map a deploy request can
/// carry. Providers may echo any of these values in diagnostics, including
/// definition/baked values that exist only in the resolved launch block.
fn env_secrets_from_request(request: &serde_json::Value) -> Vec<String> {
    let agent = request.get("agent");
    let maps = [
        agent.and_then(|value| value.get("env_vars")),
        agent
            .and_then(|value| value.get("launch"))
            .and_then(|value| value.get("env")),
        agent
            .and_then(|value| value.get("launch"))
            .and_then(|value| value.get("policy_env")),
    ];

    maps.into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_object)
        .flat_map(|map| map.values())
        .filter_map(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(String::from)
        .collect()
}

/// Redact every non-empty value from `env` (plus the standard nsec/sprt_tok
/// prefix scrubbing) out of `s`. Used by callers that already have a flat env
/// map handy — e.g. model discovery formatting child stderr into a
/// user-visible error.
pub fn redact_env_values_in(s: &str, env: &std::collections::BTreeMap<String, String>) -> String {
    let values: Vec<&str> = env
        .values()
        .filter(|v| !v.is_empty())
        .map(String::as_str)
        .collect();
    redact_secrets_with(s, &values)
}

/// Copy a resolved provider into a private staging directory while hashing
/// exactly the bytes copied. The staged file becomes non-writable before either
/// invocation, closing the path replacement and in-place rewrite races.
fn stage_provider(
    binary: &Path,
) -> Result<(tempfile::TempDir, PathBuf, String, std::fs::File), String> {
    let directory = tempfile::Builder::new()
        .prefix("buzz-provider-")
        .tempdir()
        .map_err(|error| format!("failed to create provider staging directory: {error}"))?;
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let staged_path = directory.path().join(format!("provider{suffix}"));
    let mut source = std::fs::File::open(binary)
        .map_err(|error| format!("failed to open provider for staging: {error}"))?;
    let mut staged = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged_path)
        .map_err(|error| format!("failed to create staged provider: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = source
            .read(&mut buffer)
            .map_err(|error| format!("failed to read provider for staging: {error}"))?;
        if count == 0 {
            break;
        }
        staged
            .write_all(&buffer[..count])
            .map_err(|error| format!("failed to write staged provider: {error}"))?;
        hasher.update(&buffer[..count]);
    }
    staged
        .sync_all()
        .map_err(|error| format!("failed to sync staged provider: {error}"))?;

    let mut permissions = staged
        .metadata()
        .map_err(|error| format!("failed to inspect staged provider: {error}"))?
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o500);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);
    std::fs::set_permissions(&staged_path, permissions)
        .map_err(|error| format!("failed to protect staged provider: {error}"))?;
    drop(staged);

    #[cfg(windows)]
    let execution_guard = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            // Permit CreateProcess to read the image while denying replacement,
            // writes, and deletion until both invocations finish.
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .open(&staged_path)
    };
    #[cfg(not(windows))]
    let execution_guard = std::fs::File::open(&staged_path);
    let execution_guard = execution_guard
        .map_err(|error| format!("failed to lock staged provider for execution: {error}"))?;
    Ok((
        directory,
        staged_path,
        hex::encode(hasher.finalize()),
        execution_guard,
    ))
}

/// Run `info` on a staged provider and validate its answer.
fn negotiate_staged_provider(
    staged: &Path,
    cwd: Option<&Path>,
) -> Result<ProviderInfo, SessionError> {
    let info_request = serde_json::json!({
        "op": "info",
        "request_id": uuid::Uuid::new_v4().to_string(),
    });
    let info = invoke_provider(staged, cwd, &info_request, Duration::from_secs(10))?;
    Ok(parse_provider_info(&info)?)
}

/// A provider's validated `info`. Runs only `info`, which carries no agent
/// secret, on a staged copy of the binary.
pub fn provider_info(binary: &Path, cwd: Option<&Path>) -> Result<ProviderInfo, SessionError> {
    let (_directory, staged, _digest, _execution_guard) = stage_provider(binary)?;
    negotiate_staged_provider(&staged, cwd)
}

/// Deploy through one immutable staged copy: negotiate protocol v1 before the
/// secret-bearing request, then invoke deploy on those exact same bytes.
/// Returns the backend agent id.
pub fn provider_deploy(
    binary: &Path,
    cwd: Option<&Path>,
    agent: &serde_json::Value,
    provider_config: &serde_json::Value,
) -> Result<String, SessionError> {
    let (_directory, staged, _digest, _execution_guard) = stage_provider(binary)?;
    negotiate_staged_provider(&staged, cwd)?;

    let request = serde_json::json!({
        "op": "deploy",
        "request_id": uuid::Uuid::new_v4().to_string(),
        "agent": agent,
        "provider_config": provider_config,
    });
    let resp = invoke_provider(&staged, cwd, &request, Duration::from_secs(600))?;
    Ok(deployed_agent_id(&resp)?)
}

/// End a remote session through one immutable staged copy. The `stop`
/// request carries the same secret-bearing payload as `deploy`, so it is sent
/// only after the same bytes negotiated protocol v1 and advertised `stop`.
pub fn provider_stop(
    binary: &Path,
    cwd: Option<&Path>,
    agent: &serde_json::Value,
    provider_config: &serde_json::Value,
) -> Result<StopState, SessionError> {
    let (_directory, staged, _digest, _execution_guard) = stage_provider(binary)?;
    if !negotiate_staged_provider(&staged, cwd)?.supports("stop") {
        return Err(
            "this provider cannot end remote sessions; update the provider and try again".into(),
        );
    }
    let request = serde_json::json!({
        "op": "stop",
        "request_id": uuid::Uuid::new_v4().to_string(),
        "agent": agent,
        "provider_config": provider_config,
    });
    let response = invoke_provider(&staged, cwd, &request, Duration::from_secs(120))?;
    Ok(stop_response_state(&response)?)
}

/// Bound public provider configuration and reject secret-like keys at every depth.
pub fn validate_provider_config(config: &serde_json::Value) -> Result<(), String> {
    let obj = config
        .as_object()
        .ok_or("provider_config must be a JSON object")?;
    if obj.len() > 20 {
        return Err("provider_config: max 20 fields".to_string());
    }
    let json_str = serde_json::to_string(config).unwrap_or_default();
    if json_str.len() > 65536 {
        return Err("provider_config: max 64KB".to_string());
    }
    validate_config_value(config, 0, ConfigPath::Root)
}

/// Where a value sits relative to the one public field allowed to be named
/// `key`: `pod_options.tolerations[].key`.
#[derive(Clone, Copy, PartialEq)]
enum ConfigPath {
    Root,
    PodOptions,
    Tolerations,
    Toleration,
    Other,
}

impl ConfigPath {
    fn field(self, name: &str) -> Self {
        match (self, name) {
            (Self::Root, "pod_options") => Self::PodOptions,
            (Self::PodOptions, "tolerations") => Self::Tolerations,
            _ => Self::Other,
        }
    }

    fn entry(self) -> Self {
        if self == Self::Tolerations {
            Self::Toleration
        } else {
            Self::Other
        }
    }
}

fn validate_config_value(
    value: &serde_json::Value,
    depth: usize,
    path: ConfigPath,
) -> Result<(), String> {
    if depth > 4 {
        return Err("provider_config: max nesting depth is 4".into());
    }
    if let Some(values) = value.as_array() {
        if values.len() > 64 {
            return Err("provider_config: max 64 array entries".into());
        }
        for entry in values {
            validate_config_value(entry, depth + 1, path.entry())?;
        }
        return Ok(());
    }
    let Some(obj) = value.as_object() else {
        return Ok(());
    };
    if obj.len() > 20 {
        return Err("provider_config: max 20 fields per object".into());
    }
    // Split on separators AND camelCase boundaries, then check each word.
    // Catches: api_key, apiKey, access-token, clientSecret, etc.
    // Allows: keyboard, monkey_wrench (no forbidden word as a segment).
    let forbidden = ["secret", "password", "token", "key", "credential"];
    for (k, v) in obj {
        let words = split_config_key(k);
        // A Kubernetes toleration names its taint with a literal `key`.
        let toleration_key = path == ConfigPath::Toleration && k == "key";
        for f in &forbidden {
            if toleration_key && *f == "key" {
                continue;
            }
            if words.iter().any(|w| w == f) {
                return Err(format!("provider_config: key '{}' looks like a secret", k));
            }
        }
        validate_config_value(v, depth + 1, path.field(k))?;
    }
    Ok(())
}

/// Derive a provider id from a `buzz-backend-*` filename. Tauri removes its
/// target-triple suffix while copying an external binary, but on Windows
/// leaves the executable/script extension, which is not part of the provider
/// id.
fn provider_id_from_filename(name: &str) -> Option<&str> {
    let raw = name.strip_prefix("buzz-backend-")?;
    let id = [".exe", ".bat", ".cmd"]
        .into_iter()
        .find_map(|extension| {
            raw.get(raw.len().saturating_sub(extension.len())..)
                .filter(|suffix| suffix.eq_ignore_ascii_case(extension))
                .map(|_| &raw[..raw.len() - extension.len()])
        })
        .unwrap_or(raw);

    (!id.is_empty()).then_some(id)
}

/// The directories searched for `buzz-backend-*` providers, in order.
///
/// A snapshot: build one per operation when `PATH` can change at runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    pub dirs: Vec<PathBuf>,
}

impl Discovery {
    /// `bundled` (when given and not already on `PATH`), then `PATH`, then
    /// `~/.local/bin`.
    ///
    /// `bundled` is the directory a host ships providers in, such as a
    /// `.app` bundle's `Contents/MacOS/` next to the executable; `None` when
    /// the host bundles none. `~/.local/bin` is the conventional location for
    /// user-installed Buzz extension binaries (symlinks created by install
    /// scripts). On macOS, GUI apps inherit a minimal PATH from launchd
    /// (`/usr/bin:/bin:/usr/sbin:/sbin`) which excludes both, so they are
    /// added regardless of how the host was launched.
    pub fn from_env(bundled: Option<PathBuf>) -> Self {
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
        if let Some(bundled) = bundled {
            if !dirs.contains(&bundled) {
                dirs.insert(0, bundled);
            }
        }
        if let Some(home) = dirs::home_dir() {
            let local_bin = home.join(".local").join("bin");
            if !dirs.contains(&local_bin) {
                dirs.push(local_bin);
            }
        }
        Self { dirs }
    }

    /// Enumerate `buzz-backend-*` executables. Returns (id, path) pairs,
    /// first directory first. Does NOT execute any binaries.
    pub fn candidates(&self) -> Vec<(String, PathBuf)> {
        let prefix = "buzz-backend-";
        let mut seen = std::collections::HashSet::new();
        let mut results = Vec::new();

        for dir in &self.dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(prefix) {
                    if let Some(id) = provider_id_from_filename(&name) {
                        if !seen.contains(&name) && is_executable(&entry.path()) {
                            seen.insert(name.clone());
                            results.push((id.to_string(), entry.path()));
                        }
                    }
                }
            }
        }
        results
    }
}

/// Resolve a provider ID to a discovered, executable binary path.
///
/// This is the ONLY way to resolve provider binaries for execution. It:
/// 1. Validates the ID against `^[a-z0-9][a-z0-9_-]*$` (no path traversal)
/// 2. Keeps `cached` only when it is a discovered candidate for this ID, so a
///    tampered record cannot redirect calls to another provider's binary
/// 3. Otherwise looks up the ID in [`Discovery::candidates`]
/// 4. Returns the canonical path of the discovered binary
///
/// Every deploy, stop, and info path MUST use this instead of resolving
/// `buzz-backend-{id}` as a command, to prevent a compromised caller from
/// steering execution to an arbitrary binary.
pub fn resolve_provider_binary(
    provider_id: &str,
    cached: Option<&Path>,
    discovery: &Discovery,
) -> Result<PathBuf, SessionError> {
    // Reject IDs that could be path components or shell metacharacters.
    let valid_id = provider_id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && !provider_id.is_empty()
        && provider_id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !valid_id {
        return Err(
            format!("invalid provider ID '{provider_id}': must match [a-z0-9][a-z0-9_-]*").into(),
        );
    }

    let candidates = discovery.candidates();
    let cached = cached
        .filter(|path| path.exists())
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
        .filter(|canonical| {
            candidates.iter().any(|(id, candidate)| {
                id == provider_id && candidate.canonicalize().ok().as_ref() == Some(canonical)
            })
        });
    if let Some(cached) = cached {
        return Ok(cached);
    }

    let found = candidates
        .into_iter()
        .find(|(id, _)| id == provider_id)
        .map(|(_, path)| path);

    match found {
        Some(path) => path
            .canonicalize()
            .map_err(|e| format!("provider binary not accessible: {e}").into()),
        None => Err(format!("provider 'buzz-backend-{provider_id}' not found on PATH").into()),
    }
}

/// Check if a file is executable (Unix: mode bits; other platforms: always true).
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

#[cfg(test)]
#[path = "provider_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "wire_fixture_tests.rs"]
mod wire_fixture_tests;
