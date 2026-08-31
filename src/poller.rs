use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use std::os::windows::process::CommandExt;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

use crate::models::{ProviderUsage, UsageData, UsageSection};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage?platform=codex";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
// Public OAuth client id used by the Codex CLI; required for refresh grants.
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CREATE_NO_WINDOW: u32 = 0x08000000;

const MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const CLAUDE_USER_AGENT: &str = "code-agent-usage-monitor";
const CLAUDE_SESSION_WINDOW: Duration = Duration::from_secs(5 * 60 * 60);
const CLAUDE_WEEKLY_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const CODEX_PRIMARY_WINDOW: Duration = Duration::from_secs(5 * 60 * 60);
const CODEX_SECONDARY_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);
// Boundary separating the short "session" window from the long "weekly" window.
// OpenAI reports each window's true length; anything at or below this is the
// session slot, anything above is the weekly slot. A window a given account
// lacks (e.g. the 5-hour limit while suspended) simply never fills its slot.
const CODEX_SESSION_MAX_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug)]
pub enum PollError {
    RequestFailed,
}

#[derive(Deserialize)]
struct CodexSessionEvent {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    event_type: String,
    payload: CodexSessionPayload,
}

#[derive(Deserialize)]
struct CodexSessionPayload {
    #[serde(rename = "type")]
    payload_type: Option<String>,
    rate_limits: Option<CodexRateLimits>,
}

#[derive(Deserialize)]
struct CodexRateLimits {
    limit_id: Option<String>,
    primary: CodexLimitWindow,
    secondary: CodexLimitWindow,
}

#[derive(Deserialize)]
struct CodexLimitWindow {
    used_percent: f64,
    resets_at: Option<i64>,
    window_minutes: Option<i64>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

#[derive(Deserialize)]
struct OpenAiAuthFile {
    tokens: OpenAiTokens,
}

#[derive(Deserialize)]
struct OpenAiTokens {
    access_token: String,
    account_id: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeDesktopToken {
    token: String,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
}

#[derive(Deserialize)]
struct CodexTokenRefresh {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

enum CodexFetch {
    Unauthorized,
    Failed,
}

#[derive(Deserialize)]
struct CodexUsageResponse {
    rate_limit: CodexUsageRateLimit,
}

#[derive(Deserialize)]
struct CodexUsageRateLimit {
    primary_window: CodexUsageWindow,
    secondary_window: Option<CodexUsageWindow>,
}

#[derive(Deserialize)]
struct CodexUsageWindow {
    used_percent: f64,
    reset_at: Option<i64>,
    limit_window_seconds: Option<i64>,
}

pub fn poll() -> Result<UsageData, PollError> {
    let mut codex = fetch_codex_usage()
        .or_else(read_codex_rate_limits)
        .unwrap_or_default();
    roll_over_codex(&mut codex);
    let claude = match read_credentials() {
        Some(mut creds) => {
            if is_token_expired(creds.expires_at) {
                cli_refresh_token(&creds.source);

                match read_credentials_from_source(&creds.source) {
                    Some(refreshed) => creds = refreshed,
                    None => {
                        return Ok(UsageData {
                            claude: ProviderUsage::default(),
                            codex,
                        })
                    }
                }

                if is_token_expired(creds.expires_at) {
                    return Ok(UsageData {
                        claude: ProviderUsage::default(),
                        codex,
                    });
                }
            }

            fetch_usage_with_fallback(&creds.access_token).unwrap_or_default()
        }
        None => ProviderUsage::default(),
    };

    Ok(UsageData { claude, codex })
}

fn fetch_codex_usage() -> Option<ProviderUsage> {
    let creds = read_openai_auth()?;
    match request_codex_usage(&creds.access_token, &creds.account_id) {
        Ok(usage) => Some(usage),
        // Bearer rejected: refresh the OAuth token, persist it, and retry once.
        Err(CodexFetch::Unauthorized) => {
            let refreshed = refresh_codex_token(&creds)?;
            request_codex_usage(&refreshed.access_token, &refreshed.account_id).ok()
        }
        Err(CodexFetch::Failed) => None,
    }
}

fn request_codex_usage(access_token: &str, account_id: &str) -> Result<ProviderUsage, CodexFetch> {
    let agent = build_agent().map_err(|_| CodexFetch::Failed)?;
    let resp = match agent
        .get(CODEX_USAGE_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("ChatGPT-Account-Id", account_id)
        .set("Accept", "application/json")
        .set("User-Agent", "CodexBar")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            return Err(CodexFetch::Unauthorized)
        }
        Err(_) => return Err(CodexFetch::Failed),
    };

    let response: CodexUsageResponse = resp.into_json().map_err(|_| CodexFetch::Failed)?;
    Ok(map_codex_usage(response))
}

/// One rate-limit window normalized across the API and session-file schemas,
/// carrying its own length so it can be routed to the correct UI slot.
struct CodexWindow {
    percentage: f64,
    resets_at: Option<SystemTime>,
    window_secs: Option<u64>,
}

fn map_codex_usage(response: CodexUsageResponse) -> ProviderUsage {
    let primary = &response.rate_limit.primary_window;
    let mut windows = vec![CodexWindow {
        percentage: primary.used_percent,
        resets_at: unix_to_system_time(primary.reset_at),
        window_secs: primary
            .limit_window_seconds
            .filter(|s| *s > 0)
            .map(|s| s as u64),
    }];
    if let Some(secondary) = &response.rate_limit.secondary_window {
        windows.push(CodexWindow {
            percentage: secondary.used_percent,
            resets_at: unix_to_system_time(secondary.reset_at),
            window_secs: secondary
                .limit_window_seconds
                .filter(|s| *s > 0)
                .map(|s| s as u64),
        });
    }
    assign_codex_windows(windows)
}

/// Route each reported window to the session or weekly slot by its true length
/// rather than its position. OpenAI has, at times, returned only the long
/// (weekly) window in the primary position while suspending the 5-hour window;
/// positional mapping would then mislabel a 7-day window as a 5-hour one. When
/// a window's length is unknown (older session-file payloads), fall back to
/// position: the first window is the session, the second is the weekly.
fn assign_codex_windows(windows: Vec<CodexWindow>) -> ProviderUsage {
    let mut usage = ProviderUsage::default();
    for (index, window) in windows.into_iter().enumerate() {
        let is_session = match window.window_secs {
            Some(secs) => secs <= CODEX_SESSION_MAX_WINDOW.as_secs(),
            None => index == 0,
        };
        let section = UsageSection {
            percentage: window.percentage,
            resets_at: window.resets_at,
            has_data: true,
        };
        if is_session {
            usage.session = section;
        } else {
            usage.weekly = section;
        }
    }
    usage
}

/// Exchange the stored refresh token for a fresh access token via the OpenAI
/// OAuth endpoint, then write the rotated tokens back to auth.json so the next
/// poll (and the Codex CLI itself) sees current credentials.
fn refresh_codex_token(current: &OpenAiTokens) -> Option<OpenAiTokens> {
    let refresh_token = current.refresh_token.as_deref()?;
    let agent = build_agent().ok()?;
    let body = serde_json::json!({
        "client_id": CODEX_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "scope": "openid profile email",
    });

    let resp = agent
        .post(CODEX_TOKEN_URL)
        .set("Content-Type", "application/json")
        .send_json(body)
        .ok()?;

    let refreshed: CodexTokenRefresh = resp.into_json().ok()?;
    let new_tokens = OpenAiTokens {
        access_token: refreshed.access_token,
        account_id: current.account_id.clone(),
        refresh_token: refreshed
            .refresh_token
            .or_else(|| current.refresh_token.clone()),
        id_token: refreshed.id_token.or_else(|| current.id_token.clone()),
    };

    persist_codex_auth(&new_tokens);
    Some(new_tokens)
}

fn persist_codex_auth(tokens: &OpenAiTokens) {
    let Some(path) = codex_auth_path() else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut root) = serde_json::from_str::<Value>(&content) else {
        return;
    };

    if let Some(obj) = root.get_mut("tokens").and_then(|t| t.as_object_mut()) {
        obj.insert(
            "access_token".to_string(),
            Value::String(tokens.access_token.clone()),
        );
        if let Some(id_token) = &tokens.id_token {
            obj.insert("id_token".to_string(), Value::String(id_token.clone()));
        }
        if let Some(refresh_token) = &tokens.refresh_token {
            obj.insert(
                "refresh_token".to_string(),
                Value::String(refresh_token.clone()),
            );
        }
    }

    if let (Some(obj), Some(now)) = (root.as_object_mut(), rfc3339_now()) {
        obj.insert("last_refresh".to_string(), Value::String(now));
    }

    if let Ok(serialized) = serde_json::to_string_pretty(&root) {
        let _ = std::fs::write(&path, serialized);
    }
}

fn rfc3339_now() -> Option<String> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (hours, mins, seconds) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hours:02}:{mins:02}:{seconds:02}Z"
    ))
}

// Hinnant's days-from-epoch to civil-date conversion (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Invoke the Claude CLI with a minimal prompt to force its internal
/// OAuth token refresh.
fn cli_refresh_token(source: &CredentialSource) {
    match source {
        CredentialSource::Windows(_) => cli_refresh_windows_token(),
        CredentialSource::ClaudeDesktop { .. } => {}
        CredentialSource::Wsl { distro } => cli_refresh_wsl_token(distro),
    }
}

fn cli_refresh_windows_token() {
    let claude_path = resolve_windows_claude_path();
    let is_cmd = claude_path.to_lowercase().ends_with(".cmd");

    let args: &[&str] = &["-p", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&claude_path).args(args);
        c
    } else {
        let mut c = Command::new(&claude_path);
        c.args(args);
        c
    };
    cmd.env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return,
    };

    // Wait up to 30 seconds — don't block the poll thread forever
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

fn cli_refresh_wsl_token(distro: &str) {
    let mut cmd = Command::new("wsl.exe");
    cmd.arg("-d")
        .arg(distro)
        .arg("--")
        .arg("bash")
        .arg("-lic")
        .arg("if command -v claude >/dev/null 2>&1; then claude -p .; elif [ -x \"$HOME/.local/bin/claude\" ]; then \"$HOME/.local/bin/claude\" -p .; else exit 127; fi")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return,
    };

    wait_for_refresh(&mut child);
}

/// Spawn a command and wait up to `timeout` for it to finish.
/// Returns None if the process fails to start or exceeds the deadline.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

fn wait_for_refresh(child: &mut std::process::Child) {
    // Wait up to 30 seconds; don't block the poll thread forever.
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

/// Resolve the full path to the `claude` CLI executable.
fn resolve_windows_claude_path() -> String {
    for name in &["claude.cmd", "claude"] {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return name.to_string();
        }
    }

    for name in &["claude.cmd", "claude"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "claude.cmd".to_string()
}

fn build_agent() -> Result<ureq::Agent, PollError> {
    let tls = native_tls::TlsConnector::new().map_err(|_| PollError::RequestFailed)?;
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

fn fetch_usage_with_fallback(token: &str) -> Result<ProviderUsage, PollError> {
    // Try the dedicated usage endpoint first
    if let Some(data) = try_usage_endpoint(token) {
        // If reset timers are missing, fill them in from the Messages API.
        if data.session.resets_at.is_none() || data.weekly.resets_at.is_none() {
            if let Ok(fallback) = fetch_usage_via_messages(token) {
                let mut merged = data;
                if merged.session.resets_at.is_none() {
                    merged.session.resets_at = fallback.session.resets_at;
                }
                if merged.weekly.resets_at.is_none() {
                    merged.weekly.resets_at = fallback.weekly.resets_at;
                }
                return Ok(merged);
            }
        }
        return Ok(data);
    }

    // Fall back to Messages API with rate limit headers.
    fetch_usage_via_messages(token)
}

fn try_usage_endpoint(token: &str) -> Option<ProviderUsage> {
    let agent = build_agent().ok()?;

    let response = match agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("Accept", "application/json")
        .set("User-Agent", CLAUDE_USER_AGENT)
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(_code, resp)) => resp,
        Err(_) => return None,
    };

    let header_fallback = if usage_headers_present(&response) {
        Some(parse_rate_limit_headers(&response))
    } else {
        None
    };
    let body = response.into_string().ok()?;

    parse_usage_response(&body).or(header_fallback)
}

fn fetch_available_models(agent: &ureq::Agent, token: &str) -> Vec<String> {
    let resp = match agent
        .get(MODELS_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-version", "2023-06-01")
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("Accept", "application/json")
        .set("User-Agent", CLAUDE_USER_AGENT)
        .call()
    {
        Ok(r) => r,
        _ => return Vec::new(),
    };

    let list: ModelsResponse = match resp.into_json() {
        Ok(l) => l,
        _ => return Vec::new(),
    };

    let mut models: Vec<String> = list.data.into_iter().map(|m| m.id).collect();

    // cheapest first: haiku < sonnet < opus
    fn tier(id: &str) -> u8 {
        if id.contains("haiku") {
            0
        } else if id.contains("sonnet") {
            1
        } else {
            2
        }
    }
    models.sort_by_key(|id| tier(id));
    models
}

fn fetch_usage_via_messages(token: &str) -> Result<ProviderUsage, PollError> {
    let agent = build_agent()?;
    let models = fetch_available_models(&agent, token);

    if models.is_empty() {
        return Err(PollError::RequestFailed);
    }

    for model in &models {
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "."}]
        });

        let response = match agent
            .post(MESSAGES_URL)
            .set("Authorization", &format!("Bearer {token}"))
            .set("anthropic-version", "2023-06-01")
            .set("anthropic-beta", "oauth-2025-04-20")
            .set("Accept", "application/json")
            .set("User-Agent", CLAUDE_USER_AGENT)
            .send_json(&body)
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(_code, resp)) => resp,
            Err(_) => continue,
        };

        let h5 = response.header("anthropic-ratelimit-unified-5h-utilization");
        let h7 = response.header("anthropic-ratelimit-unified-7d-utilization");
        let hs = response.header("anthropic-ratelimit-unified-status");

        if h5.is_some() || h7.is_some() || hs.is_some() {
            return Ok(parse_rate_limit_headers(&response));
        }
    }

    Err(PollError::RequestFailed)
}

fn parse_rate_limit_headers(response: &ureq::Response) -> ProviderUsage {
    let mut data = ProviderUsage::default();

    data.session.has_data = response
        .header("anthropic-ratelimit-unified-5h-utilization")
        .is_some();
    data.session.percentage =
        get_header_percentage(response, "anthropic-ratelimit-unified-5h-utilization");
    data.session.resets_at =
        get_header_reset_time(response, "anthropic-ratelimit-unified-5h-reset");

    data.weekly.has_data = response
        .header("anthropic-ratelimit-unified-7d-utilization")
        .is_some();
    data.weekly.percentage =
        get_header_percentage(response, "anthropic-ratelimit-unified-7d-utilization");
    data.weekly.resets_at = get_header_reset_time(response, "anthropic-ratelimit-unified-7d-reset");

    let overall_reset = get_header_reset_time(response, "anthropic-ratelimit-unified-reset");

    let status = response.header("anthropic-ratelimit-unified-status");
    if !data.session.has_data && !data.weekly.has_data && status.is_some() {
        data.session.has_data = true;
        data.weekly.has_data = true;
    }

    if data.session.percentage == 0.0 && data.weekly.percentage == 0.0 {
        if status == Some("rejected") {
            let claim = response.header("anthropic-ratelimit-unified-representative-claim");
            match claim {
                Some("five_hour") => data.session.percentage = 100.0,
                Some("seven_day") => data.weekly.percentage = 100.0,
                _ => {}
            }
        }

        if data.session.resets_at.is_none() && overall_reset.is_some() {
            data.session.resets_at = overall_reset;
        }
    }

    populate_missing_claude_resets(&mut data);

    data
}

fn parse_usage_response(content: &str) -> Option<ProviderUsage> {
    let json: Value = serde_json::from_str(content).ok()?;
    let mut data = ProviderUsage::default();
    let mut found_bucket = false;

    if let Some(section) = parse_usage_section(
        json.get("five_hour")
            .or_else(|| json.get("fiveHour"))
            .or_else(|| json.get("primary_window")),
    ) {
        data.session = section;
        found_bucket = true;
    }

    if let Some(section) = parse_usage_section(
        json.get("seven_day")
            .or_else(|| json.get("sevenDay"))
            .or_else(|| json.get("secondary_window")),
    ) {
        data.weekly = section;
        found_bucket = true;
    }

    if found_bucket {
        populate_missing_claude_resets(&mut data);
        Some(data)
    } else {
        None
    }
}

fn parse_usage_section(value: Option<&Value>) -> Option<UsageSection> {
    let bucket = value?;
    let percentage = parse_percentage(
        bucket
            .get("utilization")
            .or_else(|| bucket.get("used_percent"))
            .or_else(|| bucket.get("percentage"))
            .or_else(|| bucket.get("used_percentage"))?
            .as_f64()?,
    );
    let resets_at = parse_reset_value(bucket.get("resets_at").or_else(|| bucket.get("reset_at")));

    Some(UsageSection {
        percentage,
        resets_at,
        has_data: true,
    })
}

fn parse_percentage(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }

    let normalized = if value > 0.0 && value < 1.0 {
        value * 100.0
    } else {
        value
    };

    normalized.clamp(0.0, 100.0)
}

fn parse_reset_value(value: Option<&Value>) -> Option<SystemTime> {
    match value? {
        Value::Number(number) => unix_to_system_time(number.as_i64()),
        Value::String(text) => parse_iso8601(Some(text)),
        _ => None,
    }
}

fn populate_missing_claude_resets(data: &mut ProviderUsage) {
    fill_missing_reset(&mut data.session, CLAUDE_SESSION_WINDOW);
    fill_missing_reset(&mut data.weekly, CLAUDE_WEEKLY_WINDOW);
}

fn fill_missing_reset(section: &mut UsageSection, window: Duration) {
    if !section.has_data || section.resets_at.is_some() || section.percentage != 0.0 {
        return;
    }

    section.resets_at = SystemTime::now().checked_add(window);
}

fn roll_over_codex(data: &mut ProviderUsage) {
    roll_over_section(&mut data.session, CODEX_PRIMARY_WINDOW);
    roll_over_section(&mut data.weekly, CODEX_SECONDARY_WINDOW);
}

/// A snapshot whose reset boundary has already passed describes a window that
/// has since rolled over: usage is back to zero. Zero the percentage and
/// advance the boundary by whole windows so the countdown stays aligned to the
/// real reset cadence. Live readings (reset in the future) are left untouched.
fn roll_over_section(section: &mut UsageSection, window: Duration) {
    let Some(reset) = section.resets_at else {
        return;
    };
    let now = SystemTime::now();
    let Ok(elapsed) = now.duration_since(reset) else {
        return;
    };

    let window_secs = window.as_secs();
    if window_secs == 0 {
        return;
    }

    let windows_passed = elapsed.as_secs() / window_secs + 1;
    section.percentage = 0.0;
    section.resets_at = reset.checked_add(Duration::from_secs(windows_passed * window_secs));
}

fn usage_headers_present(response: &ureq::Response) -> bool {
    response
        .header("anthropic-ratelimit-unified-5h-utilization")
        .is_some()
        || response
            .header("anthropic-ratelimit-unified-7d-utilization")
            .is_some()
        || response
            .header("anthropic-ratelimit-unified-status")
            .is_some()
}

fn get_header_percentage(response: &ureq::Response, name: &str) -> f64 {
    response
        .header(name)
        .and_then(|s| s.parse::<f64>().ok())
        .map(parse_percentage)
        .unwrap_or(0.0)
}

fn get_header_reset_time(response: &ureq::Response, name: &str) -> Option<SystemTime> {
    let value = response.header(name)?;

    if let Ok(unix_secs) = value.parse::<i64>() {
        return unix_to_system_time(Some(unix_secs));
    }

    parse_iso8601(Some(value))
}

fn unix_to_system_time(unix_secs: Option<i64>) -> Option<SystemTime> {
    let secs = unix_secs?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

struct Credentials {
    access_token: String,
    expires_at: Option<i64>,
    source: CredentialSource,
}

#[derive(Clone, Debug)]
enum CredentialSource {
    Windows(PathBuf),
    ClaudeDesktop {
        config_path: PathBuf,
        local_state_path: PathBuf,
    },
    Wsl {
        distro: String,
    },
}

fn read_credentials() -> Option<Credentials> {
    let mut candidates = Vec::new();

    if let Some(creds) = read_windows_credentials() {
        candidates.push(creds);
    }

    if let Some(creds) = read_claude_desktop_credentials() {
        candidates.push(creds);
    }

    for distro in list_wsl_distros() {
        if let Some(creds) = read_wsl_credentials(&distro) {
            candidates.push(creds);
        }
    }

    choose_best_credentials(candidates)
}

fn read_windows_credentials() -> Option<Credentials> {
    let home = dirs::home_dir()?;
    let cred_path = home.join(".claude").join(".credentials.json");
    let content = std::fs::read_to_string(&cred_path).ok()?;
    parse_credentials(&content, CredentialSource::Windows(cred_path))
}

fn read_claude_desktop_credentials() -> Option<Credentials> {
    let app_data = dirs::config_dir()?;
    let claude_dir = app_data.join("Claude");
    let config_path = claude_dir.join("config.json");
    let local_state_path = claude_dir.join("Local State");
    read_claude_desktop_credentials_from_paths(&config_path, &local_state_path)
}

fn read_claude_desktop_credentials_from_paths(
    config_path: &Path,
    local_state_path: &Path,
) -> Option<Credentials> {
    let local_state_content = std::fs::read_to_string(local_state_path).ok()?;
    let local_state: Value = serde_json::from_str(&local_state_content).ok()?;
    let encoded_key = local_state.pointer("/os_crypt/encrypted_key")?.as_str()?;
    let encrypted_key = STANDARD.decode(encoded_key).ok()?;
    let protected_key = encrypted_key.strip_prefix(b"DPAPI")?;
    let chromium_key = unprotect_windows_data(protected_key)?;

    let config_content = std::fs::read_to_string(config_path).ok()?;
    let config: Value = serde_json::from_str(&config_content).ok()?;
    let encoded_cache = config.get("oauth:tokenCacheV2")?.as_str()?;
    let decrypted_cache = decrypt_chromium_value(encoded_cache, &chromium_key)?;
    parse_claude_desktop_token_cache(
        &decrypted_cache,
        CredentialSource::ClaudeDesktop {
            config_path: config_path.to_path_buf(),
            local_state_path: local_state_path.to_path_buf(),
        },
    )
}

fn parse_claude_desktop_token_cache(
    decrypted_cache: &[u8],
    source: CredentialSource,
) -> Option<Credentials> {
    let token_cache: HashMap<String, ClaudeDesktopToken> =
        serde_json::from_slice(decrypted_cache).ok()?;

    let token = token_cache
        .into_iter()
        .filter(|(cache_key, token)| {
            cache_key.contains("user:inference") && !token.token.is_empty()
        })
        .max_by_key(|(_, token)| token.expires_at.unwrap_or_default())?
        .1;

    Some(Credentials {
        access_token: token.token,
        expires_at: token.expires_at,
        source,
    })
}

fn unprotect_windows_data(protected: &[u8]) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(protected.len()).ok()?,
        pbData: protected.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&input, None, None, None, None, 0, &mut output).ok()?;
    }
    if output.pbData.is_null() {
        return None;
    }

    let decrypted =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(HLOCAL(output.pbData.cast()));
    }
    Some(decrypted)
}

fn decrypt_chromium_value(encoded: &str, key: &[u8]) -> Option<Vec<u8>> {
    let encrypted = STANDARD.decode(encoded).ok()?;
    let payload = encrypted.strip_prefix(b"v10")?;
    let nonce_bytes = payload.get(..12)?;
    let ciphertext = payload.get(12..)?;
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .ok()
}

fn read_credentials_from_source(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(path) => {
            let content = std::fs::read_to_string(path).ok()?;
            parse_credentials(&content, source.clone())
        }
        CredentialSource::ClaudeDesktop {
            config_path,
            local_state_path,
        } => read_claude_desktop_credentials_from_paths(config_path, local_state_path),
        CredentialSource::Wsl { distro } => read_wsl_credentials(distro),
    }
}

fn read_wsl_credentials(distro: &str) -> Option<Credentials> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg("cat ~/.claude/.credentials.json")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    if !output.status.success() {
        return None;
    }

    let content = String::from_utf8(output.stdout).ok()?;
    parse_credentials(
        &content,
        CredentialSource::Wsl {
            distro: distro.to_string(),
        },
    )
}

fn parse_credentials(content: &str, source: CredentialSource) -> Option<Credentials> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;

    let oauth = json.get("claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())?
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64());

    Some(Credentials {
        access_token,
        expires_at,
        source,
    })
}

fn choose_best_credentials(mut candidates: Vec<Credentials>) -> Option<Credentials> {
    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by_key(|creds| is_token_expired(creds.expires_at));
    candidates.into_iter().next()
}

fn list_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe")
            .args(["-l", "-q"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) {
        Some(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    let stdout = decode_wsl_text(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    if let Some(decoded) = decode_utf16le(bytes) {
        return decoded;
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let body = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else if looks_like_utf16le(bytes) {
        bytes
    } else {
        return None;
    };

    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    Some(String::from_utf16_lossy(&units))
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(128);
    let units = sample_len / 2;
    if units == 0 {
        return false;
    }

    let nul_high_bytes = bytes[..sample_len]
        .chunks_exact(2)
        .filter(|chunk| chunk[1] == 0)
        .count();

    nul_high_bytes * 2 >= units
}

fn is_token_expired(expires_at: Option<i64>) -> bool {
    let Some(exp) = expires_at else { return false };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    now >= exp
}

/// Parse an ISO 8601 timestamp string into a SystemTime.
fn parse_iso8601(s: Option<&str>) -> Option<SystemTime> {
    let s = s?;
    // Strip timezone offset to get "YYYY-MM-DDTHH:MM:SS" or with fractional seconds
    // The API returns formats like "2026-03-05T08:00:00.321598+00:00"
    let datetime_part = s.split('+').next().unwrap_or(s);
    let datetime_part = datetime_part.split('Z').next().unwrap_or(datetime_part);

    // Try parsing with and without fractional seconds
    let formats = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"];
    for fmt in &formats {
        if let Ok(secs) = parse_datetime_to_unix(datetime_part, fmt) {
            return Some(UNIX_EPOCH + Duration::from_secs(secs));
        }
    }
    None
}

/// Minimal datetime parser — avoids pulling in chrono/time crates.
fn parse_datetime_to_unix(s: &str, _fmt: &str) -> Result<u64, ()> {
    // Extract date and time parts from "YYYY-MM-DDTHH:MM:SS[.frac]"
    let (date_str, time_str) = s.split_once('T').ok_or(())?;
    let date_parts: Vec<&str> = date_str.split('-').collect();
    if date_parts.len() != 3 {
        return Err(());
    }

    let year: u64 = date_parts[0].parse().map_err(|_| ())?;
    let month: u64 = date_parts[1].parse().map_err(|_| ())?;
    let day: u64 = date_parts[2].parse().map_err(|_| ())?;

    // Strip fractional seconds
    let time_base = time_str.split('.').next().unwrap_or(time_str);
    let time_parts: Vec<&str> = time_base.split(':').collect();
    if time_parts.len() != 3 {
        return Err(());
    }

    let hour: u64 = time_parts[0].parse().map_err(|_| ())?;
    let min: u64 = time_parts[1].parse().map_err(|_| ())?;
    let sec: u64 = time_parts[2].parse().map_err(|_| ())?;

    // Days from year (using a simplified calculation for dates after 1970)
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Format a usage section as "X% · Yh" style text
pub fn format_line(section: &UsageSection) -> String {
    if !section.has_data && section.resets_at.is_none() {
        return "--".to_string();
    }

    let pct = format!("{:.0}%", section.percentage);
    let cd = format_countdown(section.resets_at);
    if cd.is_empty() {
        pct
    } else {
        format!("{pct} \u{00b7} {cd}")
    }
}

fn format_countdown(resets_at: Option<SystemTime>) -> String {
    let reset = match resets_at {
        Some(t) => t,
        None => return String::new(),
    };

    let remaining = match reset.duration_since(SystemTime::now()) {
        Ok(d) => d,
        Err(_) => return "0m".to_string(),
    };

    let total_secs = remaining.as_secs();
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    if total_days >= 1 {
        format!("{total_days}d")
    } else if total_hours >= 1 {
        format!("{total_hours}h")
    } else {
        format!("{total_mins}m")
    }
}

/// Calculate how long until the display text would change
pub fn time_until_display_change(resets_at: Option<SystemTime>) -> Option<Duration> {
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?;

    let total_secs = remaining.as_secs();
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    let next_boundary = if total_days >= 1 {
        Duration::from_secs(total_days * 86400)
    } else if total_hours >= 1 {
        Duration::from_secs(total_hours * 3600)
    } else {
        Duration::from_secs(total_mins * 60)
    };

    let delay = remaining.saturating_sub(next_boundary);
    if delay > Duration::ZERO {
        Some(delay + Duration::from_secs(1))
    } else {
        Some(Duration::from_secs(1))
    }
}

pub fn time_until_reset(resets_at: Option<SystemTime>) -> Option<Duration> {
    let reset = resets_at?;
    reset.duration_since(SystemTime::now()).ok()
}

/// Returns true if either section has reached its reset time.
pub fn is_past_reset(data: &ProviderUsage) -> bool {
    let now = SystemTime::now();
    let past = |s: &UsageSection| matches!(s.resets_at, Some(t) if now.duration_since(t).is_ok());
    past(&data.session) || past(&data.weekly)
}

fn read_codex_rate_limits() -> Option<ProviderUsage> {
    let sessions_dir = dirs::home_dir()?.join(".codex").join("sessions");
    read_codex_rate_limits_from_dir(&sessions_dir)
}

fn codex_auth_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".codex").join("auth.json"))
}

fn read_openai_auth() -> Option<OpenAiTokens> {
    let content = std::fs::read_to_string(codex_auth_path()?).ok()?;
    let auth: OpenAiAuthFile = serde_json::from_str(&content).ok()?;
    Some(auth.tokens)
}

fn read_codex_rate_limits_from_dir(sessions_dir: &Path) -> Option<ProviderUsage> {
    let mut session_files: Vec<PathBuf> = Vec::new();
    visit_session_files(sessions_dir, &mut session_files);

    let mut newest: Option<(SystemTime, ProviderUsage)> = None;
    for path in session_files {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };

        for line in content.lines() {
            let event: CodexSessionEvent = match serde_json::from_str(line) {
                Ok(event) => event,
                Err(_) => continue,
            };

            let Some(parsed) = codex_usage_from_event(event) else {
                continue;
            };

            let should_replace = match &newest {
                Some((current, _)) => parsed.0 > *current,
                None => true,
            };
            if should_replace {
                newest = Some(parsed);
            }
        }
    }

    newest.map(|(_, usage)| usage)
}

fn codex_usage_from_event(event: CodexSessionEvent) -> Option<(SystemTime, ProviderUsage)> {
    if event.event_type != "event_msg" {
        return None;
    }

    if event.payload.payload_type.as_deref() != Some("token_count") {
        return None;
    }

    let timestamp = parse_iso8601(event.timestamp.as_deref())?;
    let limits = event.payload.rate_limits?;
    if limits.limit_id.as_deref() != Some("codex") {
        return None;
    }

    let windows = vec![
        CodexWindow {
            percentage: limits.primary.used_percent,
            resets_at: unix_to_system_time(limits.primary.resets_at),
            window_secs: limits
                .primary
                .window_minutes
                .filter(|m| *m > 0)
                .map(|m| m as u64 * 60),
        },
        CodexWindow {
            percentage: limits.secondary.used_percent,
            resets_at: unix_to_system_time(limits.secondary.resets_at),
            window_secs: limits
                .secondary
                .window_minutes
                .filter(|m| *m > 0)
                .map(|m| m as u64 * 60),
        },
    ];

    Some((timestamp, assign_codex_windows(windows)))
}

fn visit_session_files(dir: &Path, session_files: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_session_files(&path, session_files);
            continue;
        }

        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        session_files.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        civil_from_days, format_line, map_codex_usage, parse_claude_desktop_token_cache,
        parse_percentage, parse_usage_response, read_codex_rate_limits_from_dir, roll_over_section,
        time_until_reset, CodexUsageResponse, CredentialSource, UsageSection,
        CLAUDE_SESSION_WINDOW, CLAUDE_WEEKLY_WINDOW, CODEX_PRIMARY_WINDOW, UNIX_EPOCH,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("code-agent-usage-monitor-{name}-{nanos}"))
    }

    #[test]
    fn format_line_uses_0m_for_past_reset() {
        let section = UsageSection {
            percentage: 15.0,
            resets_at: Some(SystemTime::now() - Duration::from_secs(5)),
            has_data: true,
        };

        assert_eq!(format_line(&section), "15% \u{00b7} 0m");
    }

    #[test]
    fn claude_desktop_cache_selects_latest_inference_token() {
        let source = CredentialSource::Windows(PathBuf::from("unused"));
        let credentials = parse_claude_desktop_token_cache(
            br#"{
                "client:org:https://api.anthropic.com:user:profile": {
                    "token": "profile-only",
                    "expiresAt": 9999
                },
                "client:org:https://api.anthropic.com:user:inference user:profile": {
                    "token": "older-inference",
                    "expiresAt": 1000
                },
                "client:org:https://api.anthropic.com:user:inference user:file_upload": {
                    "token": "newer-inference",
                    "expiresAt": 2000
                }
            }"#,
            source,
        )
        .expect("desktop credentials");

        assert_eq!(credentials.access_token, "newer-inference");
        assert_eq!(credentials.expires_at, Some(2000));
    }

    #[test]
    fn format_line_uses_minutes_for_sub_hour_reset() {
        let section = UsageSection {
            percentage: 15.0,
            resets_at: Some(SystemTime::now() + Duration::from_secs(59 * 60 + 30)),
            has_data: true,
        };

        assert_eq!(format_line(&section), "15% \u{00b7} 59m");
    }

    #[test]
    fn format_line_uses_minutes_for_sub_minute_reset() {
        let section = UsageSection {
            percentage: 15.0,
            resets_at: Some(SystemTime::now() + Duration::from_secs(59)),
            has_data: true,
        };

        assert_eq!(format_line(&section), "15% \u{00b7} 0m");
    }

    #[test]
    fn format_line_shows_pct_when_data_received_without_reset() {
        let section = UsageSection {
            percentage: 0.0,
            resets_at: None,
            has_data: true,
        };

        assert_eq!(format_line(&section), "0%");
    }

    #[test]
    fn format_line_shows_dash_when_no_data() {
        let section = UsageSection {
            percentage: 0.0,
            resets_at: None,
            has_data: false,
        };

        assert_eq!(format_line(&section), "--");
    }

    #[test]
    fn codex_reader_uses_latest_event_timestamp_across_files() {
        let root = unique_temp_dir("codex-sessions");
        let older_dir = root.join("2026").join("03").join("24");
        let newer_dir = root.join("2026").join("03").join("25");
        fs::create_dir_all(&older_dir).expect("create older dir");
        fs::create_dir_all(&newer_dir).expect("create newer dir");

        let older_file = older_dir.join("older.jsonl");
        let newer_file = newer_dir.join("newer.jsonl");

        fs::write(
            &older_file,
            concat!(
                "{\"timestamp\":\"2026-03-25T12:34:34.363Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"rate_limits\":{\"limit_id\":\"codex\",\"primary\":{\"used_percent\":0.0,\"resets_at\":1774460043},\"secondary\":{\"used_percent\":7.0,\"resets_at\":1774532923}}}}\n"
            ),
        )
        .expect("write older file");

        fs::write(
            &newer_file,
            concat!(
                "{\"timestamp\":\"2026-03-25T11:00:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"rate_limits\":{\"limit_id\":\"codex\",\"primary\":{\"used_percent\":15.0,\"resets_at\":1774453200},\"secondary\":{\"used_percent\":7.0,\"resets_at\":1774532923}}}}\n"
            ),
        )
        .expect("write newer file");

        let usage = read_codex_rate_limits_from_dir(&root).expect("usage");

        assert_eq!(usage.session.percentage, 0.0);
        assert_eq!(usage.weekly.percentage, 7.0);

        fs::remove_dir_all(&root).expect("cleanup temp dir");
    }

    #[test]
    fn codex_usage_response_maps_primary_and_secondary_windows() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 7.0,
                        "reset_at": 1774980685
                    },
                    "secondary_window": {
                        "used_percent": 11.0,
                        "reset_at": 1775220865
                    }
                }
            }"#,
        )
        .expect("parse response");

        assert_eq!(response.rate_limit.primary_window.used_percent, 7.0);
        assert_eq!(
            response
                .rate_limit
                .secondary_window
                .as_ref()
                .expect("secondary")
                .used_percent,
            11.0
        );
    }

    #[test]
    fn parse_usage_response_reads_oauth_usage_shape() {
        let usage = parse_usage_response(
            r#"{
                "five_hour": {
                    "utilization": 2.0,
                    "resets_at": "2026-04-09T18:00:00.000000+00:00"
                },
                "seven_day": {
                    "utilization": 14.0,
                    "resets_at": "2026-04-13T00:00:00.000000+00:00"
                }
            }"#,
        )
        .expect("usage");

        assert_eq!(usage.session.percentage, 2.0);
        assert_eq!(usage.weekly.percentage, 14.0);
        assert!(usage.session.resets_at.is_some());
        assert!(usage.weekly.resets_at.is_some());
    }

    #[test]
    fn parse_usage_response_normalizes_fractional_utilization() {
        let usage = parse_usage_response(
            r#"{
                "five_hour": {
                    "utilization": 0.02,
                    "resets_at": "2026-04-09T18:00:00Z"
                },
                "seven_day": {
                    "utilization": 0.14,
                    "resets_at": "2026-04-13T00:00:00Z"
                }
            }"#,
        )
        .expect("usage");

        assert!((usage.session.percentage - 2.0).abs() < 1e-9);
        assert!((usage.weekly.percentage - 14.0).abs() < 1e-9);
    }

    #[test]
    fn parse_usage_response_infers_missing_reset_times_for_zeroed_windows() {
        let before = SystemTime::now();
        let usage = parse_usage_response(
            r#"{
                "five_hour": {
                    "utilization": 0.0
                },
                "seven_day": {
                    "utilization": 0.0
                }
            }"#,
        )
        .expect("usage");
        let after = SystemTime::now();

        let session_reset = usage.session.resets_at.expect("session reset");
        let weekly_reset = usage.weekly.resets_at.expect("weekly reset");

        assert!(session_reset >= before + CLAUDE_SESSION_WINDOW);
        assert!(session_reset <= after + CLAUDE_SESSION_WINDOW);
        assert!(weekly_reset >= before + CLAUDE_WEEKLY_WINDOW);
        assert!(weekly_reset <= after + CLAUDE_WEEKLY_WINDOW);
    }

    #[test]
    fn parse_percentage_keeps_percent_values_and_scales_fractional_values() {
        assert_eq!(parse_percentage(14.0), 14.0);
        assert!((parse_percentage(0.14) - 14.0).abs() < 1e-9);
        assert_eq!(parse_percentage(0.0), 0.0);
    }

    #[test]
    fn time_until_reset_returns_none_for_past_reset() {
        let remaining = time_until_reset(Some(SystemTime::now() - Duration::from_secs(1)));
        assert!(remaining.is_none());
    }

    #[test]
    fn time_until_reset_returns_future_duration() {
        let remaining = time_until_reset(Some(SystemTime::now() + Duration::from_secs(30)))
            .expect("future reset");
        assert!(remaining <= Duration::from_secs(30));
        assert!(remaining > Duration::from_secs(25));
    }

    #[test]
    fn roll_over_section_zeros_stale_window_and_advances_reset() {
        // a snapshot recorded at 100% whose 5h window expired one window ago
        let mut section = UsageSection {
            percentage: 100.0,
            resets_at: Some(SystemTime::now() - Duration::from_secs(60)),
            has_data: true,
        };

        roll_over_section(&mut section, CODEX_PRIMARY_WINDOW);

        assert_eq!(section.percentage, 0.0);
        let reset = section.resets_at.expect("advanced reset");
        let remaining = reset
            .duration_since(SystemTime::now())
            .expect("reset moved into the future");
        assert!(remaining <= CODEX_PRIMARY_WINDOW);
        assert!(remaining > CODEX_PRIMARY_WINDOW - Duration::from_secs(120));
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        // unix epoch
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-06-17 is 20621 days after epoch
        assert_eq!(civil_from_days(20621), (2026, 6, 17));
        // leap day
        assert_eq!(civil_from_days(18321), (2020, 2, 29));
    }

    #[test]
    fn roll_over_section_leaves_live_reading_untouched() {
        let future = SystemTime::now() + Duration::from_secs(1800);
        let mut section = UsageSection {
            percentage: 73.0,
            resets_at: Some(future),
            has_data: true,
        };

        roll_over_section(&mut section, CODEX_PRIMARY_WINDOW);

        assert_eq!(section.percentage, 73.0);
        assert_eq!(section.resets_at, Some(future));
    }

    #[test]
    fn suspended_five_hour_routes_weekly_window_to_weekly_slot() {
        // Live shape while OpenAI has the 5h window suspended: a single 7-day
        // primary window and a null secondary.
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 0.0,
                        "reset_at": 1784552566,
                        "limit_window_seconds": 604800
                    },
                    "secondary_window": null
                }
            }"#,
        )
        .expect("parse response");

        let usage = map_codex_usage(response);

        assert!(!usage.session.has_data);
        assert_eq!(format_line(&usage.session), "--");
        assert!(usage.weekly.has_data);
        assert_eq!(usage.weekly.percentage, 0.0);
        assert!(usage.weekly.resets_at.is_some());
    }

    #[test]
    fn active_windows_route_by_length_not_position() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 40.0,
                        "reset_at": 1774460043,
                        "limit_window_seconds": 18000
                    },
                    "secondary_window": {
                        "used_percent": 7.0,
                        "reset_at": 1774532923,
                        "limit_window_seconds": 604800
                    }
                }
            }"#,
        )
        .expect("parse response");

        let usage = map_codex_usage(response);

        assert!(usage.session.has_data);
        assert_eq!(usage.session.percentage, 40.0);
        assert!(usage.weekly.has_data);
        assert_eq!(usage.weekly.percentage, 7.0);
    }

    #[test]
    fn windows_without_length_fall_back_to_position() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": { "used_percent": 40.0, "reset_at": 1774460043 },
                    "secondary_window": { "used_percent": 7.0, "reset_at": 1774532923 }
                }
            }"#,
        )
        .expect("parse response");

        let usage = map_codex_usage(response);

        assert_eq!(usage.session.percentage, 40.0);
        assert_eq!(usage.weekly.percentage, 7.0);
    }
}
