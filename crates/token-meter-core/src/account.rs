use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexRateLimitWindow {
    pub used_percent: i64,
    pub window_duration_minutes: i64,
    pub resets_at: DateTime<Utc>,
}

impl CodexRateLimitWindow {
    pub const fn remaining_percent(&self) -> i64 {
        100 - self.used_percent
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexResetCreditSummary {
    pub available_count: i64,
    pub expirations: Vec<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAccountUsage {
    pub five_hour_window: Option<CodexRateLimitWindow>,
    pub seven_day_window: Option<CodexRateLimitWindow>,
    pub reset_credits: Option<CodexResetCreditSummary>,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CodexAccountUsageError {
    #[error("Codex returned an account status this version cannot read.")]
    InvalidResponse,
    #[error("{0}")]
    Server(String),
}

pub fn parse_rate_limits_response(
    response: &[u8],
    fetched_at: DateTime<Utc>,
) -> Result<CodexAccountUsage, CodexAccountUsageError> {
    let response: RpcResponse =
        serde_json::from_slice(response).map_err(|_| CodexAccountUsageError::InvalidResponse)?;
    if let Some(error) = response.error {
        return Err(CodexAccountUsageError::Server(error.message));
    }
    let result = response
        .result
        .ok_or(CodexAccountUsageError::InvalidResponse)?;

    let windows = [result.rate_limits.primary, result.rate_limits.secondary]
        .into_iter()
        .flatten()
        .map(|window| {
            Ok(CodexRateLimitWindow {
                used_percent: (window.used_percent.round() as i64).clamp(0, 100),
                window_duration_minutes: window.window_duration_mins,
                resets_at: unix_date(window.resets_at)
                    .ok_or(CodexAccountUsageError::InvalidResponse)?,
            })
        })
        .collect::<Result<Vec<_>, CodexAccountUsageError>>()?;

    let reset_credits = result.rate_limit_reset_credits.map(|credits| {
        let mut expirations = credits
            .credits
            .unwrap_or_default()
            .into_iter()
            .filter_map(|credit| credit.expires_at.and_then(unix_date))
            .collect::<Vec<_>>();
        expirations.sort_unstable();
        CodexResetCreditSummary {
            available_count: credits.available_count.max(0),
            expirations,
        }
    });

    Ok(CodexAccountUsage {
        five_hour_window: windows
            .iter()
            .find(|window| window.window_duration_minutes == 300)
            .cloned(),
        seven_day_window: windows
            .iter()
            .find(|window| window.window_duration_minutes == 10_080)
            .cloned(),
        reset_credits,
        fetched_at,
    })
}

fn unix_date(value: f64) -> Option<DateTime<Utc>> {
    if !value.is_finite() {
        return None;
    }
    let whole = value.floor();
    let mut seconds = whole as i64;
    let mut nanos = ((value - whole) * 1_000_000_000.0).round() as u32;
    if nanos == 1_000_000_000 {
        seconds = seconds.checked_add(1)?;
        nanos = 0;
    }
    DateTime::from_timestamp(seconds, nanos)
}

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<RateLimitsResult>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateLimitsResult {
    rate_limits: RateLimitSnapshot,
    rate_limit_reset_credits: Option<ResetCredits>,
}

#[derive(Deserialize)]
struct RateLimitSnapshot {
    primary: Option<RawRateLimitWindow>,
    secondary: Option<RawRateLimitWindow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRateLimitWindow {
    used_percent: f64,
    window_duration_mins: i64,
    resets_at: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResetCredits {
    available_count: i64,
    credits: Option<Vec<ResetCredit>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResetCredit {
    expires_at: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    #[test]
    fn parses_windows_and_authoritative_reset_credit_count() {
        let response = br#"{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":150,"windowDurationMins":300,"resetsAt":1783666131},"secondary":{"usedPercent":-4,"windowDurationMins":10080,"resetsAt":1784252931}},"rateLimitResetCredits":{"availableCount":4,"credits":[{"expiresAt":1785524779},{"expiresAt":1785109644}]}}}"#;
        let usage = parse_rate_limits_response(response, at(1_700_000_000)).unwrap();

        assert_eq!(usage.five_hour_window.unwrap().used_percent, 100);
        assert_eq!(usage.seven_day_window.unwrap().remaining_percent(), 100);
        assert_eq!(usage.reset_credits.as_ref().unwrap().available_count, 4);
        assert_eq!(usage.reset_credits.unwrap().expirations.len(), 2);
        assert_eq!(usage.fetched_at, at(1_700_000_000));
    }

    #[test]
    fn keeps_missing_values_explicit_and_reports_rpc_failures() {
        let missing = br#"{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":15,"resetsAt":1783666131},"secondary":null},"rateLimitResetCredits":null}}"#;
        let usage = parse_rate_limits_response(missing, Utc::now()).unwrap();
        assert!(usage.five_hour_window.is_none());
        assert!(usage.seven_day_window.is_none());
        assert!(usage.reset_credits.is_none());

        assert_eq!(
            parse_rate_limits_response(
                br#"{"id":2,"error":{"code":-32000,"message":"Login required"}}"#,
                Utc::now()
            ),
            Err(CodexAccountUsageError::Server("Login required".into()))
        );
        assert_eq!(
            parse_rate_limits_response(br#"{"id":2}"#, Utc::now()),
            Err(CodexAccountUsageError::InvalidResponse)
        );
        assert_eq!(
            parse_rate_limits_response(b"not-json", Utc::now()),
            Err(CodexAccountUsageError::InvalidResponse)
        );
    }
}
