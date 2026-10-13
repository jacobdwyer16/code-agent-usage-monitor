use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Deserialize;

use super::{build_agent, unix_to_system_time, PollError};
use crate::diagnose;
use crate::models::{UsageData, UsageSection};

const MUSE_KEY_URL: &str = "https://api.meta.ai/muse-code/key";
const MUSE_API_VERSION: &str = "1.0.0";
// `muse login` (device flow) issues these; `muse auth set` stores a Model API
// key instead, which the subscription endpoint rejects
const DEVICE_TOKEN_PREFIX: &str = "dca:";

#[derive(Deserialize)]
struct MuseAuthFile {
    #[serde(default)]
    providers: Option<MuseAuthProviders>,
}

#[derive(Deserialize)]
struct MuseAuthProviders {
    meta: Option<MuseAuthEntry>,
}

#[derive(Deserialize)]
struct MuseAuthEntry {
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct MuseKeyResponse {
    #[serde(default)]
    require_payment: bool,
    #[serde(default)]
    is_subs_active: bool,
    subs_tier_name: Option<String>,
    subs_usage: Option<MuseSubscriptionUsage>,
}

#[derive(Deserialize)]
struct MuseSubscriptionUsage {
    window: MuseWindow,
    weekly: MuseWindow,
}

#[derive(Deserialize)]
struct MuseWindow {
    used_percent: f64,
    resets_at: Option<i64>,
}

pub(super) fn poll_muse() -> Result<UsageData, PollError> {
    let path = muse_auth_path().ok_or(PollError::NoCredentials)?;
    let Some(token) = read_muse_device_token(&path) else {
        diagnose::log(
            "Muse usage poll failed: no 'muse login' device token found (run 'muse login' to sign in)",
        );
        return Err(PollError::NoCredentials);
    };
    fetch_muse_usage(&token)
}

pub(super) fn credential_watch_snapshot(_all_sources: bool) -> Vec<String> {
    let Some(path) = muse_auth_path() else {
        return vec!["muse:auth-path-missing".into()];
    };
    let key = format!("muse:{}", path.display());
    let signature = match std::fs::metadata(&path) {
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

/// Mirrors the CLI: `MUSE_AUTH_PATH`, then `MUSE_CONFIG_DIR`, then
/// `XDG_CONFIG_HOME/muse`, then `~/.config/muse`, on Windows too.
fn muse_auth_path() -> Option<PathBuf> {
    let environment = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    if let Some(path) = environment("MUSE_AUTH_PATH") {
        return Some(PathBuf::from(path));
    }
    if let Some(directory) = environment("MUSE_CONFIG_DIR") {
        return Some(PathBuf::from(directory).join("auth.json"));
    }
    if let Some(directory) = environment("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(directory).join("muse").join("auth.json"));
    }
    Some(
        crate::known_folders::home_dir()?
            .join(".config")
            .join("muse")
            .join("auth.json"),
    )
}

fn read_muse_device_token(path: &Path) -> Option<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            diagnose::log_error(
                &format!("unable to read Muse credentials at {}", path.display()),
                error,
            );
            return None;
        }
    };
    device_token_from_auth(&content)
}

fn device_token_from_auth(content: &str) -> Option<String> {
    let auth: MuseAuthFile = serde_json::from_str(content).ok()?;
    auth.providers?
        .meta?
        .access_token
        .map(|token| token.trim().to_string())
        .filter(|token| token.starts_with(DEVICE_TOKEN_PREFIX))
}

fn fetch_muse_usage(token: &str) -> Result<UsageData, PollError> {
    let mut response = match build_agent()?
        .post(MUSE_KEY_URL)
        .header("Authorization", &format!("Bearer {token}"))
        .header("x-api-version", MUSE_API_VERSION)
        .send_json(serde_json::json!({}))
        .and_then(super::check_http_status)
    {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(code @ (401 | 403))) => {
            diagnose::log(format!(
                "Muse key endpoint returned auth error status {code}; run 'muse login'"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(ureq::Error::StatusCode(code)) => {
            diagnose::log(format!("Muse key endpoint returned status {code}"));
            return Err(PollError::HttpStatus(code));
        }
        Err(error) => {
            diagnose::log_error("Muse key endpoint request failed", error);
            return Err(PollError::NetworkError);
        }
    };
    let response: MuseKeyResponse = response.body_mut().read_json().map_err(|error| {
        diagnose::log_error("unable to parse Muse key response", error);
        PollError::UnexpectedResponse
    })?;
    muse_usage_from_response(response)
}

fn muse_usage_from_response(response: MuseKeyResponse) -> Result<UsageData, PollError> {
    if response.require_payment {
        diagnose::log("Muse Code requires a payment method; finish billing at dev.meta.ai");
        return Err(PollError::UnexpectedResponse);
    }
    if !response.is_subs_active {
        diagnose::log("no Muse Code subscription is active on this login");
        return Err(PollError::UnexpectedResponse);
    }
    if let Some(tier) = response.subs_tier_name.as_deref() {
        diagnose::log(format!("Muse subscription tier: {tier}"));
    }
    let Some(usage) = response.subs_usage else {
        // the endpoint omits usage while the 5-hour window is idle, and with
        // it the weekly figure, which stays unknown rather than reading 0%
        return Ok(UsageData {
            session: UsageSection {
                available: true,
                percentage: 0.0,
                resets_at: None,
            },
            ..UsageData::default()
        });
    };
    Ok(UsageData {
        session: muse_section(&usage.window),
        weekly: muse_section(&usage.weekly),
        ..UsageData::default()
    })
}

fn muse_section(window: &MuseWindow) -> UsageSection {
    UsageSection {
        available: true,
        percentage: window.used_percent.clamp(0.0, 100.0),
        resets_at: unix_to_system_time(window.resets_at.filter(|secs| *secs > 0)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn usage_from_json(json: &str) -> Result<UsageData, PollError> {
        muse_usage_from_response(serde_json::from_str(json).unwrap())
    }

    #[test]
    fn active_windows_map_to_session_and_weekly() {
        let data = usage_from_json(
            r#"{"is_subs_active":true,"subs_tier_name":"Muse Code Everyday Usage","subs_usage":{"tier":"1","window":{"used_percent":6,"resets_at":1787198224,"window_duration_mins":300},"weekly":{"used_percent":117,"resets_at":1787600000}}}"#,
        )
        .unwrap();

        assert!(data.session.available && data.weekly.available);
        assert_eq!(data.session.percentage, 6.0);
        assert_eq!(data.weekly.percentage, 100.0);
        assert_eq!(
            data.session.resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(1787198224))
        );
        assert_eq!(
            data.weekly.resets_at,
            Some(UNIX_EPOCH + Duration::from_secs(1787600000))
        );
    }

    #[test]
    fn an_idle_window_reads_zero_and_leaves_weekly_unknown() {
        let data =
            usage_from_json(r#"{"is_subs_active":true,"subs_tier_name":"Muse Code High Usage"}"#)
                .unwrap();

        assert!(data.session.available);
        assert_eq!(data.session.percentage, 0.0);
        assert!(data.session.resets_at.is_none());
        assert!(!data.weekly.available);
    }

    #[test]
    fn missing_or_zero_resets_have_no_countdown() {
        let data = usage_from_json(
            r#"{"is_subs_active":true,"subs_usage":{"window":{"used_percent":0,"resets_at":0},"weekly":{"used_percent":12}}}"#,
        )
        .unwrap();

        assert!(data.session.resets_at.is_none());
        assert!(data.weekly.resets_at.is_none());
        assert_eq!(data.weekly.percentage, 12.0);
    }

    #[test]
    fn inactive_or_unpaid_subscriptions_are_not_usage() {
        for json in [
            r#"{"is_subs_active":false}"#,
            r#"{}"#,
            r#"{"is_subs_active":true,"require_payment":true}"#,
        ] {
            assert_eq!(usage_from_json(json), Err(PollError::UnexpectedResponse));
        }
    }

    #[test]
    fn only_device_login_tokens_are_used() {
        assert_eq!(
            device_token_from_auth(
                r#"{"providers":{"meta":{"access_token":" dca:abc ","api_key":"mk-1"}}}"#
            )
            .as_deref(),
            Some("dca:abc")
        );
        for content in [
            r#"{"providers":{"meta":{"api_key":"mk-1"}}}"#,
            r#"{"providers":{"meta":{"access_token":"mk-1"}}}"#,
            r#"{"providers":{}}"#,
            r#"{}"#,
            "not json",
        ] {
            assert!(device_token_from_auth(content).is_none(), "{content}");
        }
    }

    #[test]
    fn the_device_token_is_read_from_the_auth_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        assert!(read_muse_device_token(&path).is_none());

        std::fs::write(&path, r#"{"providers":{"meta":{"access_token":"dca:x"}}}"#).unwrap();

        assert_eq!(read_muse_device_token(&path).as_deref(), Some("dca:x"));
    }
}
