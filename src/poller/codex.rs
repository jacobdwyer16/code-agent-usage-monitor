use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

use super::{build_agent, parse_iso8601, unix_to_system_time, PollError};
use crate::app_settings;
use crate::diagnose;
use crate::models::{CodexCreditsState, CreditsSection, UsageData, UsageSection};

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CREATE_NO_WINDOW: u32 = 0x08000000;
const CODEX_SESSION_WINDOW: Duration = Duration::from_secs(5 * 60 * 60);
const CODEX_WEEKLY_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokenData>,
}

#[derive(Clone, Deserialize)]
struct CodexTokenData {
    access_token: String,
    account_id: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct CodexTokenRefresh {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// One line of a Codex CLI session log (`<CODEX_HOME>/sessions/**/*.jsonl`).
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
    rate_limits: Option<CodexSessionRateLimits>,
}

#[derive(Deserialize)]
struct CodexSessionRateLimits {
    limit_id: Option<String>,
    primary: CodexSessionLimitWindow,
    secondary: CodexSessionLimitWindow,
}

#[derive(Deserialize)]
struct CodexSessionLimitWindow {
    used_percent: f64,
    resets_at: Option<i64>,
    window_minutes: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct CodexUsageResponse {
    rate_limit: Option<Option<Box<CodexRateLimitDetails>>>,
    spend_control: Option<CodexSpendControl>,
    credits: Option<Option<Box<CodexCredits>>>,
}

#[derive(Deserialize)]
struct CodexSpendControl {
    individual_limit: Option<CodexIndividualLimit>,
}

#[derive(Deserialize)]
struct CodexIndividualLimit {
    used_percent: Option<f64>,
    reset_at: Option<i64>,
}

#[derive(Deserialize)]
struct CodexCredits {
    #[serde(default)]
    has_credits: bool,
    #[serde(default)]
    unlimited: bool,
    #[serde(default)]
    overage_limit_reached: bool,
    /// Sent as a decimal string, in credits rather than currency.
    balance: Option<String>,
}

/// Codex bills credits at 25 to the dollar. Only the displayed amount depends
/// on this, never the gauge: a ratio of two credit figures is unit-free, so a
/// change to this rate cannot make the bar wrong.
const CODEX_CREDITS_PER_DOLLAR: f64 = 25.0;

#[derive(Deserialize)]
struct CodexRateLimitDetails {
    primary_window: Option<Option<Box<CodexRateLimitWindow>>>,
    secondary_window: Option<Option<Box<CodexRateLimitWindow>>>,
    /// True once any window is spent, whichever one it was. Better than
    /// reading a percentage back out of a window we mapped ourselves, and it
    /// keeps working if the five-hour window is switched on again.
    #[serde(default)]
    limit_reached: bool,
}

#[derive(Deserialize)]
pub(super) struct CodexRateLimitWindow {
    used_percent: f64,
    #[serde(default)]
    reset_at: Option<i64>,
    limit_window_seconds: Option<i64>,
}

/// A window at or above this length is a weekly allowance rather than a
/// session one. Codex currently sends 604800 for weekly and 18000 for the
/// five-hour window, so anything from a day up is unambiguously weekly.
const WEEKLY_WINDOW_THRESHOLD_SECONDS: i64 = 86_400;

pub(super) fn poll_codex() -> Result<UsageData, PollError> {
    let path = codex_auth_path().ok_or(PollError::NoCredentials)?;
    poll_account(&path)
}

pub(super) fn poll_account(path: &Path) -> Result<UsageData, PollError> {
    poll_account_online(path).or_else(|error| {
        // login failures keep their sign-in prompt; only outages fall back
        if !error.is_transient() {
            return Err(error);
        }
        let sessions = path.parent().map(|directory| directory.join("sessions"));
        match sessions
            .as_deref()
            .and_then(read_codex_rate_limits_from_dir)
        {
            Some(mut data) => {
                diagnose::log(format!(
                    "Codex usage endpoint failed ({error:?}); using latest local session snapshot"
                ));
                roll_over_section(&mut data.session, CODEX_SESSION_WINDOW);
                roll_over_section(&mut data.weekly, CODEX_WEEKLY_WINDOW);
                data.stale = true;
                Ok(data)
            }
            None => Err(error),
        }
    })
}

fn poll_account_online(path: &Path) -> Result<UsageData, PollError> {
    let creds = match read_codex_credentials_at(path) {
        Some(creds) => creds,
        None => {
            diagnose::log("Codex usage poll failed: no Codex credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    match fetch_codex_usage_at(&creds.access_token, creds.account_id.as_deref(), Some(path)) {
        Ok(data) => Ok(data),
        Err(PollError::AuthRequired) => {
            // the OAuth refresh grant avoids the CLI fallback below, which
            // spends a real model request to rotate the token
            if let Some(refreshed) = refresh_codex_token(path, &creds) {
                return fetch_codex_usage_at(
                    &refreshed.access_token,
                    refreshed.account_id.as_deref(),
                    Some(path),
                );
            }
            if path.file_name().is_some_and(|name| name == "auth.json") {
                if let Some(directory) = path.parent() {
                    cli_refresh_codex_token(directory);
                }
            }
            let refreshed = read_codex_credentials_at(path).ok_or(PollError::TokenExpired)?;
            fetch_codex_usage_at(
                &refreshed.access_token,
                refreshed.account_id.as_deref(),
                Some(path),
            )
        }
        Err(error) => Err(error),
    }
}

fn fetch_codex_usage_at(
    token: &str,
    account_id: Option<&str>,
    path: Option<&Path>,
) -> Result<UsageData, PollError> {
    let account_id = account_id.filter(|value| !value.is_empty());
    let agent = build_agent()?;
    let mut request = agent
        .get(CODEX_USAGE_URL)
        .header("Authorization", &format!("Bearer {token}"))
        .header("User-Agent", "codex-cli");

    if let Some(account_id) = account_id {
        request = request.header("ChatGPT-Account-Id", account_id);
    }

    let mut resp = match request.call().and_then(super::check_http_status) {
        Ok(resp) => resp,
        Err(ureq::Error::StatusCode(code)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Codex usage endpoint returned auth error status {code}; refresh required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Codex usage endpoint request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: CodexUsageResponse = match resp.body_mut().read_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Codex usage response", error);
            return Err(PollError::RequestFailed);
        }
    };

    codex_usage_from_response_at(response, account_id, path).ok_or(PollError::RequestFailed)
}

#[cfg(test)]
pub(super) fn codex_usage_from_response(
    response: CodexUsageResponse,
    account_id: Option<&str>,
) -> Option<UsageData> {
    codex_usage_from_response_at(response, account_id, None)
}

fn codex_usage_from_response_at(
    response: CodexUsageResponse,
    account_id: Option<&str>,
    path: Option<&Path>,
) -> Option<UsageData> {
    let credits = response.credits.flatten();
    let details = response.rate_limit.flatten().map(|details| *details);
    let monthly = response
        .spend_control
        .and_then(|spend_control| spend_control.individual_limit)
        .as_ref()
        .and_then(codex_section_from_individual_limit);
    if details.is_none() && monthly.is_none() {
        return None;
    }
    let mut data = UsageData::default();
    let limit_reached = details
        .as_ref()
        .is_some_and(|details| details.limit_reached);

    // Assign by window length, not by slot. Codex has shipped the weekly
    // allowance in `primary_window` with `secondary_window` empty while the
    // five-hour window is switched off, so trusting the slot order puts a
    // weekly figure in the session bar.
    if let Some(details) = details {
        for (window, default_is_weekly) in [
            (details.primary_window.flatten(), false),
            (details.secondary_window.flatten(), true),
        ]
        .into_iter()
        .filter_map(|(window, default_is_weekly)| window.map(|window| (window, default_is_weekly)))
        {
            let section = codex_section_from_window(&window);
            if window_is_weekly(&window).unwrap_or(default_is_weekly) {
                data.weekly = section;
            } else {
                data.session = section;
            }
        }
    }

    if let Some(monthly) = monthly {
        if !data.weekly.available {
            data.weekly = monthly.clone();
            data.weekly_label = Some("30d".to_string());
        }
        data.monthly = Some(monthly);
    }

    data.credits = credits.and_then(|credits| {
        let state_path = path.map(|path| {
            app_settings::app_data_directory().join(credit_state_file_name(path, account_id))
        });
        let previous = match &state_path {
            Some(path) => std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .or_else(|| {
                    app_settings::load_codex_credits().filter(|state| {
                        account_id.is_some() && state.account_id.as_deref() == account_id
                    })
                }),
            None => app_settings::load_codex_credits(),
        };
        let (state, section) = codex_credits(previous, &credits, limit_reached, account_id);
        let saved = match &state_path {
            Some(path) => app_settings::write_json_atomic(path, &state),
            None => app_settings::save_codex_credits(&state),
        };
        if let Err(error) = saved {
            diagnose::log(format!("unable to persist Codex credit baseline: {error}"));
        }
        section
    });

    Some(data)
}

fn credit_state_file_name(path: &Path, account_id: Option<&str>) -> String {
    format!(
        "codex-credits-{}.json",
        crate::accounts::fingerprint(&format!(
            "{}|{account_id:?}",
            crate::accounts::source_key(path)
        ))
    )
}

/// Tracks the balance across polls and turns it into a gauge.
///
/// The balance only ever falls as credits are spent, so any rise is a top-up
/// and re-baselines the gauge. Tracking continues whether or not the gauge is
/// shown, because a top-up that happens while the bar is hidden still has to
/// move the baseline.
fn codex_credits(
    previous: Option<CodexCreditsState>,
    credits: &CodexCredits,
    limit_reached: bool,
    account_id: Option<&str>,
) -> (CodexCreditsState, Option<CreditsSection>) {
    let balance = credits
        .balance
        .as_deref()
        .and_then(|balance| balance.parse::<f64>().ok())
        .filter(|balance| balance.is_finite() && *balance >= 0.0)
        .unwrap_or_default();

    let previous = previous.filter(|state| state.account_id.as_deref() == account_id);
    let baseline = match previous {
        // A rise can only come from a top-up. Seed from the first balance we
        // see, which reads as untouched until the next top-up corrects it.
        Some(previous) if balance <= previous.balance => previous.baseline.max(balance),
        _ => balance,
    };
    let state = CodexCreditsState {
        account_id: account_id.map(str::to_owned),
        balance,
        baseline,
    };

    // The bars stay on the ordinary windows until two things are true at once:
    // an allowance is spent, and credits have actually started going down
    // against the current top-up. The second half is an observation rather
    // than an assumption about when a provider decides to bill credits, and it
    // holds steady while idle, so the gauge does not flicker away on a poll
    // that happens to see no change.
    let in_use = balance < baseline;
    let applicable =
        credits.has_credits && !credits.unlimited && limit_reached && in_use && baseline > 0.0;
    if !applicable {
        return (state, None);
    }

    let percentage = if credits.overage_limit_reached {
        100.0
    } else {
        (((baseline - balance) / baseline) * 100.0).clamp(0.0, 100.0)
    };

    (
        state,
        Some(CreditsSection {
            percentage,
            remaining: balance / CODEX_CREDITS_PER_DOLLAR,
            total: baseline / CODEX_CREDITS_PER_DOLLAR,
        }),
    )
}

/// Returns no classification when the API omits the duration. The caller then
/// preserves the legacy slot mapping: primary is session, secondary is weekly.
fn window_is_weekly(window: &CodexRateLimitWindow) -> Option<bool> {
    window
        .limit_window_seconds
        .map(|seconds| seconds >= WEEKLY_WINDOW_THRESHOLD_SECONDS)
}

pub(super) fn codex_section_from_window(window: &CodexRateLimitWindow) -> UsageSection {
    UsageSection {
        available: true,
        percentage: window.used_percent,
        resets_at: unix_to_system_time(window.reset_at),
    }
}

/// Rotated tokens are written back to auth.json so the Codex CLI keeps working.
fn refresh_codex_token(path: &Path, current: &CodexTokenData) -> Option<CodexTokenData> {
    let refresh_token = current.refresh_token.as_deref()?;
    let agent = build_agent().ok()?;
    let body = serde_json::json!({
        "client_id": CODEX_CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "scope": "openid profile email",
    });
    let mut response = match agent
        .post(CODEX_TOKEN_URL)
        .header("Content-Type", "application/json")
        .send_json(body)
        .and_then(super::check_http_status)
    {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("Codex OAuth token refresh failed", error);
            return None;
        }
    };
    let refreshed: CodexTokenRefresh = match response.body_mut().read_json() {
        Ok(refreshed) => refreshed,
        Err(error) => {
            diagnose::log_error("unable to parse Codex OAuth token refresh response", error);
            return None;
        }
    };
    let tokens = CodexTokenData {
        access_token: refreshed.access_token,
        account_id: current.account_id.clone(),
        refresh_token: refreshed
            .refresh_token
            .or_else(|| current.refresh_token.clone()),
        id_token: refreshed.id_token.or_else(|| current.id_token.clone()),
    };
    if let Err(error) = persist_codex_auth(path, &tokens) {
        diagnose::log(format!("unable to persist refreshed Codex tokens: {error}"));
    }
    Some(tokens)
}

fn persist_codex_auth(path: &Path, tokens: &CodexTokenData) -> Result<(), String> {
    let content = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut root: Value = serde_json::from_str(&content).map_err(|error| error.to_string())?;
    let token_fields = root
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or("auth.json has no tokens object")?;
    token_fields.insert(
        "access_token".into(),
        Value::String(tokens.access_token.clone()),
    );
    if let Some(id_token) = &tokens.id_token {
        token_fields.insert("id_token".into(), Value::String(id_token.clone()));
    }
    if let Some(refresh_token) = &tokens.refresh_token {
        token_fields.insert("refresh_token".into(), Value::String(refresh_token.clone()));
    }
    if let (Some(fields), Some(now)) = (root.as_object_mut(), rfc3339_now()) {
        fields.insert("last_refresh".into(), Value::String(now));
    }
    app_settings::write_json_atomic(path, &root)
}

fn rfc3339_now() -> Option<String> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hours, minutes, seconds) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z"
    ))
}

// Hinnant's days-from-epoch to civil-date conversion (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Newest rate-limit snapshot the Codex CLI logged, chosen by event timestamp
/// across every session file: an older file can hold a newer event.
fn read_codex_rate_limits_from_dir(sessions_dir: &Path) -> Option<UsageData> {
    let mut session_files = Vec::new();
    visit_session_files(sessions_dir, &mut session_files);

    let mut newest: Option<(SystemTime, UsageData)> = None;
    for path in session_files {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines() {
            let Ok(event) = serde_json::from_str::<CodexSessionEvent>(line) else {
                continue;
            };
            let Some((timestamp, usage)) = codex_usage_from_event(event) else {
                continue;
            };
            if newest
                .as_ref()
                .is_none_or(|(current, _)| timestamp > *current)
            {
                newest = Some((timestamp, usage));
            }
        }
    }
    newest.map(|(_, usage)| usage)
}

fn codex_usage_from_event(event: CodexSessionEvent) -> Option<(SystemTime, UsageData)> {
    if event.event_type != "event_msg"
        || event.payload.payload_type.as_deref() != Some("token_count")
    {
        return None;
    }
    let timestamp = parse_iso8601(event.timestamp.as_deref())?;
    let limits = event.payload.rate_limits?;
    if limits.limit_id.as_deref() != Some("codex") {
        return None;
    }

    let mut data = UsageData::default();
    for (window, default_is_weekly) in [(&limits.primary, false), (&limits.secondary, true)] {
        let section = UsageSection {
            available: true,
            percentage: window.used_percent,
            resets_at: unix_to_system_time(window.resets_at),
        };
        let is_weekly = window
            .window_minutes
            .filter(|minutes| *minutes > 0)
            .map(|minutes| minutes * 60 >= WEEKLY_WINDOW_THRESHOLD_SECONDS)
            .unwrap_or(default_is_weekly);
        if is_weekly {
            data.weekly = section;
        } else {
            data.session = section;
        }
    }
    Some((timestamp, data))
}

fn visit_session_files(directory: &Path, session_files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_session_files(&path, session_files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            session_files.push(path);
        }
    }
}

/// A snapshot whose reset has passed is a window that has since rolled over.
/// Without this, an old snapshot keeps the post-reset fast poll firing forever.
fn roll_over_section(section: &mut UsageSection, window: Duration) {
    let Some(reset) = section.resets_at else {
        return;
    };
    let Ok(elapsed) = SystemTime::now().duration_since(reset) else {
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

fn codex_section_from_individual_limit(limit: &CodexIndividualLimit) -> Option<UsageSection> {
    let percentage = limit.used_percent?.clamp(0.0, 100.0);
    percentage.is_finite().then(|| UsageSection {
        available: true,
        percentage,
        resets_at: unix_to_system_time(limit.reset_at),
    })
}

pub(super) fn credential_watch_snapshot() -> Vec<String> {
    let Some(path) = codex_auth_path() else {
        return vec!["codex:auth-path-missing".into()];
    };
    let key = format!("codex:{}", path.display());
    let signature = match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_nanos())
                .unwrap_or(0);
            format!("{key}|present|{}|{modified}", metadata.len())
        }
        Err(_) => format!("{key}|missing"),
    };
    vec![signature]
}

pub(super) fn codex_auth_path() -> Option<PathBuf> {
    if std::env::var_os("CODEX_HOME").is_some_and(|value| !value.is_empty()) {
        let codex_home =
            crate::accounts::environment_directory(crate::providers::ProviderId::Codex)?;
        return Some(codex_home.join("auth.json"));
    }
    Some(
        crate::known_folders::home_dir()?
            .join(".codex")
            .join("auth.json"),
    )
}

fn read_codex_credentials_at(auth_path: &Path) -> Option<CodexTokenData> {
    let content = match std::fs::read_to_string(auth_path) {
        Ok(content) => content,
        Err(error) => {
            diagnose::log_error(
                &format!(
                    "unable to read Codex credentials at {}",
                    auth_path.display()
                ),
                error,
            );
            return None;
        }
    };
    let auth: CodexAuthFile = serde_json::from_str(&content).ok()?;
    auth.tokens
        .filter(|tokens| !tokens.access_token.trim().is_empty())
}

fn cli_refresh_codex_token(directory: &Path) {
    let codex_path = resolve_windows_codex_path();
    let is_cmd = codex_path.to_lowercase().ends_with(".cmd");
    let is_ps1 = codex_path.to_lowercase().ends_with(".ps1");
    diagnose::log(format!(
        "attempting Windows Codex token refresh via {codex_path}"
    ));

    let args: &[&str] = &["exec", "."];
    let mut command = if is_cmd {
        let mut command = Command::new("cmd.exe");
        command.arg("/c").arg(&codex_path).args(args);
        command
    } else if is_ps1 {
        let mut command = Command::new("powershell.exe");
        command
            .arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&codex_path)
            .args(args);
        command
    } else {
        let mut command = Command::new(&codex_path);
        command.args(args);
        command
    };
    command
        .env("CODEX_HOME", directory)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Codex token refresh", error);
            return;
        }
    };
    wait_for_refresh(&mut child);
}

fn resolve_windows_codex_path() -> String {
    for name in ["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
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

    for name in ["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(path) = stdout
                    .lines()
                    .next()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                {
                    return path.to_string();
                }
            }
        }
    }
    "codex.cmd".to_string()
}

fn wait_for_refresh(child: &mut std::process::Child) {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > Duration::from_secs(30) => {
                let _ = child.kill();
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(500)),
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_history_is_scoped_to_source_and_account() {
        let first = Path::new("C:\\account-tests\\work\\auth.json");
        let second = Path::new("C:\\account-tests\\personal\\auth.json");
        assert_ne!(
            credit_state_file_name(first, Some("work")),
            credit_state_file_name(second, Some("work"))
        );
        assert_ne!(
            credit_state_file_name(first, Some("work")),
            credit_state_file_name(first, Some("personal"))
        );
        assert_ne!(
            credit_state_file_name(first, None),
            credit_state_file_name(second, None)
        );
    }

    fn usage_from_json(json: &str) -> UsageData {
        let response: CodexUsageResponse =
            serde_json::from_str(json).expect("the fixture should deserialize");
        codex_usage_from_response(response, None).expect("the fixture should carry usage")
    }

    fn credits(balance: &str, has_credits: bool) -> CodexCredits {
        CodexCredits {
            has_credits,
            unlimited: false,
            overage_limit_reached: false,
            balance: Some(balance.into()),
        }
    }

    #[test]
    fn the_first_balance_seeds_the_baseline_and_reads_untouched() {
        let (state, section) = codex_credits(None, &credits("1026.112935", true), true, None);

        assert_eq!(state.baseline, 1026.112935);
        // Nothing has been drawn against the seeded baseline yet, so the bars
        // stay on the ordinary windows until a later poll sees it fall.
        assert!(section.is_none());

        // That later poll, with 25 credits to the dollar.
        let previous = state;
        let (_, section) = codex_credits(Some(previous), &credits("1016.190898", true), true, None);
        let section = section.expect("a falling balance should expose the gauge");
        assert!(
            (section.remaining - 40.64763592).abs() < 1e-6,
            "{section:?}"
        );
    }

    #[test]
    fn spending_against_a_baseline_fills_the_gauge() {
        let previous = CodexCreditsState {
            account_id: None,
            balance: 2500.0,
            baseline: 2500.0,
        };
        let (state, section) = codex_credits(Some(previous), &credits("1250.0", true), true, None);

        assert_eq!(state.baseline, 2500.0);
        let section = section.expect("gauge");
        assert_eq!(section.percentage, 50.0);
        assert_eq!(section.remaining, 50.0);
        assert_eq!(section.total, 100.0);
    }

    #[test]
    fn a_rise_in_the_balance_is_a_reload_and_rebaselines() {
        let previous = CodexCreditsState {
            account_id: None,
            balance: 100.0,
            baseline: 2500.0,
        };
        let (state, section) = codex_credits(Some(previous), &credits("2600.0", true), true, None);

        assert_eq!(state.baseline, 2600.0);
        // A fresh top-up has nothing spent against it, so the gauge stands
        // down until credits start being drawn on again.
        assert!(section.is_none());
    }

    #[test]
    fn changing_accounts_reseeds_the_credit_baseline() {
        let previous = CodexCreditsState {
            account_id: Some("old-account".into()),
            balance: 100.0,
            baseline: 2500.0,
        };
        let (state, section) = codex_credits(
            Some(previous),
            &credits("50.0", true),
            true,
            Some("new-account"),
        );

        assert_eq!(state.account_id.as_deref(), Some("new-account"));
        assert_eq!(state.baseline, 50.0);
        assert!(
            section.is_none(),
            "a different account's lower balance is not prior spending"
        );
    }

    #[test]
    fn the_gauge_hides_while_an_allowance_remains() {
        let previous = CodexCreditsState {
            account_id: None,
            balance: 2000.0,
            baseline: 2500.0,
        };
        let (state, section) = codex_credits(Some(previous), &credits("1000.0", true), false, None);

        // Tracking continues while hidden so a reload still moves the baseline.
        assert_eq!(state.balance, 1000.0);
        assert_eq!(state.baseline, 2500.0);
        assert!(section.is_none());
    }

    #[test]
    fn accounts_without_credits_get_no_gauge() {
        let (_, section) = codex_credits(None, &credits("0", false), true, None);
        assert!(section.is_none());

        let unlimited = CodexCredits {
            unlimited: true,
            ..credits("1000.0", true)
        };
        let (_, section) = codex_credits(None, &unlimited, true, None);
        assert!(section.is_none());
    }

    #[test]
    fn a_reached_overage_limit_pins_the_gauge_full() {
        let previous = CodexCreditsState {
            account_id: None,
            balance: 500.0,
            baseline: 1000.0,
        };
        let reached = CodexCredits {
            overage_limit_reached: true,
            ..credits("500.0", true)
        };
        let (_, section) = codex_credits(Some(previous), &reached, true, None);

        assert_eq!(section.expect("gauge").percentage, 100.0);
    }

    #[test]
    fn a_lone_weekly_window_lands_in_the_weekly_bar() {
        // Codex ships this shape while the five-hour window is switched off:
        // the weekly allowance arrives in `primary_window`.
        let data = usage_from_json(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 100,
                        "limit_window_seconds": 604800,
                        "reset_at": 1787198224
                    },
                    "secondary_window": null
                }
            }"#,
        );

        assert_eq!(data.weekly.percentage, 100.0);
        assert_eq!(data.session.percentage, 0.0);
        assert!(data.weekly.resets_at.is_some());
        assert!(data.session.resets_at.is_none());
        assert!(data.weekly.available);
        assert!(!data.session.available);
    }

    #[test]
    fn a_reported_zero_usage_window_is_available_without_a_usable_reset() {
        let data = usage_from_json(
            r#"{
            "rate_limit": {
                "primary_window": {"used_percent":0,"limit_window_seconds":18000,"reset_at":-1},
                "secondary_window": null
            }
        }"#,
        );
        assert!(data.session.available);
        assert_eq!(data.session.percentage, 0.0);
        assert!(data.session.resets_at.is_none());
        assert!(!data.weekly.available);
    }

    #[test]
    fn windows_are_assigned_by_length_regardless_of_slot_order() {
        let data = usage_from_json(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 80,
                        "limit_window_seconds": 604800,
                        "reset_at": 1787198224
                    },
                    "secondary_window": {
                        "used_percent": 20,
                        "limit_window_seconds": 18000,
                        "reset_at": 1787100000
                    }
                }
            }"#,
        );

        assert_eq!(data.weekly.percentage, 80.0);
        assert_eq!(data.session.percentage, 20.0);
    }

    #[test]
    fn an_unlabelled_window_stays_in_the_session_bar() {
        let data = usage_from_json(
            r#"{
                "rate_limit": {
                    "primary_window": {"used_percent": 42, "reset_at": 1787100000},
                    "secondary_window": null
                }
            }"#,
        );

        assert_eq!(data.session.percentage, 42.0);
        assert_eq!(data.weekly.percentage, 0.0);
    }

    #[test]
    fn two_unlabelled_windows_keep_the_legacy_slot_mapping() {
        let data = usage_from_json(
            r#"{
                "rate_limit": {
                    "primary_window": {"used_percent": 20, "reset_at": 1787100000},
                    "secondary_window": {"used_percent": 80, "reset_at": 1787198224}
                }
            }"#,
        );

        assert_eq!(data.session.percentage, 20.0);
        assert_eq!(data.weekly.percentage, 80.0);
    }

    #[test]
    fn an_individual_spend_limit_fills_the_monthly_and_long_windows() {
        let data = usage_from_json(
            r#"{
                "rate_limit": null,
                "spend_control": {
                    "individual_limit": {
                        "used_percent": 24,
                        "reset_at": 1793491201
                    }
                }
            }"#,
        );

        assert_eq!(data.weekly.percentage, 24.0);
        assert!(!data.session.available);
        assert_eq!(
            data.weekly.resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(1793491201))
        );
        assert_eq!(data.weekly_label.as_deref(), Some("30d"));
        assert_eq!(data.monthly.as_ref(), Some(&data.weekly));
    }

    #[test]
    fn a_legacy_weekly_window_remains_the_selected_long_window() {
        let data = usage_from_json(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 20,
                        "limit_window_seconds": 604800,
                        "reset_at": 1787198224
                    }
                },
                "spend_control": {
                    "individual_limit": {
                        "used_percent": 24,
                        "reset_at": 1793491201
                    }
                }
            }"#,
        );

        assert_eq!(data.weekly.percentage, 20.0);
        assert_eq!(data.weekly_label, None);
        assert_eq!(data.monthly.unwrap().percentage, 24.0);
    }

    #[test]
    fn an_incomplete_individual_limit_does_not_hide_legacy_windows() {
        let data = usage_from_json(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 42,
                        "limit_window_seconds": 604800,
                        "reset_at": 1787198224
                    }
                },
                "spend_control": {"individual_limit": {}}
            }"#,
        );

        assert_eq!(data.weekly.percentage, 42.0);
        assert_eq!(data.monthly, None);
    }

    #[test]
    fn missing_or_null_individual_limits_do_not_fabricate_usage() {
        for json in [
            r#"{}"#,
            r#"{"rate_limit":null,"spend_control":null}"#,
            r#"{"spend_control":{}}"#,
            r#"{"spend_control":{"individual_limit":null}}"#,
            r#"{"spend_control":{"individual_limit":{}}}"#,
            r#"{"spend_control":{"individual_limit":{"used_percent":null,"reset_at":1793491201}}}"#,
            r#"{"spend_control":{"individual_limit":{"reset_at":1793491201}}}"#,
        ] {
            let response = serde_json::from_str(json).expect("fixture should deserialize");
            assert!(
                codex_usage_from_response(response, None).is_none(),
                "{json}"
            );
        }
    }

    #[test]
    fn a_monthly_fallback_keeps_the_legacy_session_window() {
        let data = usage_from_json(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 42,
                        "limit_window_seconds": 18000,
                        "reset_at": 1787100000
                    },
                    "secondary_window": null
                },
                "spend_control": {
                    "individual_limit": {"used_percent": 24, "reset_at": 1793491201}
                }
            }"#,
        );

        assert!(data.session.available);
        assert_eq!(data.session.percentage, 42.0);
        assert_eq!(
            data.session.resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(1787100000))
        );
        assert_eq!(data.weekly.percentage, 24.0);
        assert_eq!(data.weekly_label.as_deref(), Some("30d"));
        assert_eq!(data.monthly.as_ref(), Some(&data.weekly));
    }

    #[test]
    fn an_idle_individual_limit_is_available_without_a_reset_time() {
        for limit in [
            r#"{"used_percent":0}"#,
            r#"{"used_percent":0,"reset_at":null}"#,
            r#"{"used_percent":0,"reset_at":-1}"#,
        ] {
            let data = usage_from_json(&format!(
                r#"{{"spend_control":{{"individual_limit":{limit}}}}}"#
            ));

            assert!(data.weekly.available);
            assert_eq!(data.weekly.percentage, 0.0);
            assert!(data.weekly.resets_at.is_none());
            assert_eq!(data.weekly_label.as_deref(), Some("30d"));
            assert_eq!(data.monthly.as_ref(), Some(&data.weekly));
        }
    }

    #[test]
    fn a_window_without_a_reset_keeps_its_percentage() {
        let data = usage_from_json(
            r#"{"rate_limit":{"primary_window":{"used_percent":12,"limit_window_seconds":18000},"secondary_window":{"used_percent":40,"reset_at":null,"limit_window_seconds":604800}}}"#,
        );

        assert!(data.session.available);
        assert_eq!(data.session.percentage, 12.0);
        assert!(data.session.resets_at.is_none());
        assert_eq!(data.weekly.percentage, 40.0);
        assert!(data.weekly.resets_at.is_none());
    }

    fn session_line(timestamp: &str, primary: &str, secondary: &str) -> String {
        format!(
            r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"limit_id":"codex","primary":{primary},"secondary":{secondary}}}}}}}"#
        )
    }

    #[test]
    fn session_reader_uses_latest_event_timestamp_across_files() {
        let root = tempfile::tempdir().unwrap();
        let older_dir = root.path().join("2026").join("03").join("24");
        let newer_dir = root.path().join("2026").join("03").join("25");
        std::fs::create_dir_all(&older_dir).unwrap();
        std::fs::create_dir_all(&newer_dir).unwrap();
        // the file in the older directory holds the newer event
        std::fs::write(
            older_dir.join("older.jsonl"),
            session_line(
                "2026-03-25T12:34:34.363Z",
                r#"{"used_percent":0.0,"resets_at":1774460043}"#,
                r#"{"used_percent":7.0,"resets_at":1774532923}"#,
            ),
        )
        .unwrap();
        std::fs::write(
            newer_dir.join("newer.jsonl"),
            session_line(
                "2026-03-25T11:00:00.000Z",
                r#"{"used_percent":15.0,"resets_at":1774453200}"#,
                r#"{"used_percent":7.0,"resets_at":1774532923}"#,
            ),
        )
        .unwrap();

        let data = read_codex_rate_limits_from_dir(root.path()).unwrap();

        assert_eq!(data.session.percentage, 0.0);
        assert_eq!(data.weekly.percentage, 7.0);
        assert!(data.session.available && data.weekly.available);
    }

    #[test]
    fn session_windows_route_by_length_then_position() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("a.jsonl"),
            [
                "not json".to_string(),
                session_line(
                    "2026-03-25T12:00:00Z",
                    r#"{"used_percent":30.0,"resets_at":1774532923,"window_minutes":10080}"#,
                    r#"{"used_percent":0.0,"resets_at":1774460043,"window_minutes":300}"#,
                ),
            ]
            .join("\n"),
        )
        .unwrap();

        let data = read_codex_rate_limits_from_dir(root.path()).unwrap();

        assert_eq!(data.weekly.percentage, 30.0);
        assert_eq!(data.session.percentage, 0.0);
    }

    #[test]
    fn missing_session_directory_has_no_snapshot() {
        let root = tempfile::tempdir().unwrap();
        assert!(read_codex_rate_limits_from_dir(&root.path().join("sessions")).is_none());
    }

    #[test]
    fn roll_over_zeros_a_stale_window_and_advances_its_reset() {
        let mut section = UsageSection {
            available: true,
            percentage: 100.0,
            resets_at: Some(SystemTime::now() - Duration::from_secs(60)),
        };

        roll_over_section(&mut section, CODEX_SESSION_WINDOW);

        assert_eq!(section.percentage, 0.0);
        let remaining = section
            .resets_at
            .unwrap()
            .duration_since(SystemTime::now())
            .unwrap();
        assert!(remaining <= CODEX_SESSION_WINDOW);
        assert!(remaining > CODEX_SESSION_WINDOW - Duration::from_secs(120));
    }

    #[test]
    fn roll_over_leaves_a_live_reading_untouched() {
        let future = SystemTime::now() + Duration::from_secs(1800);
        let mut section = UsageSection {
            available: true,
            percentage: 73.0,
            resets_at: Some(future),
        };

        roll_over_section(&mut section, CODEX_SESSION_WINDOW);

        assert_eq!(section.percentage, 73.0);
        assert_eq!(section.resets_at, Some(future));
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20621), (2026, 6, 17));
        assert_eq!(civil_from_days(18321), (2020, 2, 29));
    }

    #[test]
    fn persisting_refreshed_tokens_keeps_unrelated_auth_fields() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"OPENAI_API_KEY":null,"tokens":{"access_token":"old","refresh_token":"r1","account_id":"acct","id_token":"i1"},"last_refresh":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let tokens = CodexTokenData {
            access_token: "new".into(),
            account_id: Some("acct".into()),
            refresh_token: Some("r2".into()),
            id_token: Some("i1".into()),
        };

        persist_codex_auth(&path, &tokens).unwrap();

        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["tokens"]["access_token"], "new");
        assert_eq!(saved["tokens"]["refresh_token"], "r2");
        assert_eq!(saved["tokens"]["account_id"], "acct");
        assert!(saved["OPENAI_API_KEY"].is_null());
        assert_ne!(saved["last_refresh"], "2026-01-01T00:00:00Z");
        let reread = read_codex_credentials_at(&path).unwrap();
        assert_eq!(reread.access_token, "new");
    }
}
