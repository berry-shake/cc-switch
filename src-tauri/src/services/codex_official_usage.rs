//! Account-consistent Codex quota and official analytics snapshots.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::services::codex_auth_credentials::{CodexCredentialSnapshot, CodexCredentialSource};
use crate::services::codex_usage_analytics::{
    query_codex_usage_analytics_with_credentials, CodexAnalyticsUsage,
};
use crate::services::subscription::{
    query_codex_quota_with_metadata, read_codex_request_credentials, CodexQuotaQueryResult,
    QuotaTier, SubscriptionQuota,
};

const LONG_CYCLE_MIN_SECONDS: i64 = 6 * 24 * 60 * 60;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexQuotaSnapshot {
    pub quota: SubscriptionQuota,
    pub email: Option<String>,
    pub credential_source: CodexCredentialSource,
    pub credential_scope: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexOfficialUsageSnapshot {
    pub quota: SubscriptionQuota,
    pub analytics: Option<CodexAnalyticsUsage>,
    pub email: Option<String>,
    pub credential_source: CodexCredentialSource,
    pub credential_scope: String,
    pub queried_at: i64,
}

async fn query_quota(
    credentials: &CodexCredentialSnapshot,
) -> Result<CodexQuotaQueryResult, String> {
    query_codex_quota_with_metadata(
        &credentials.access_token,
        credentials.account_id.as_deref(),
        "codex",
        "Authentication failed. Please re-login with Codex CLI.",
    )
    .await
}

fn long_cycle_tier(quota: &SubscriptionQuota) -> Option<&QuotaTier> {
    let queried_at = quota.queried_at?;
    quota
        .tiers
        .iter()
        .filter(|tier| {
            let Some(window_seconds) = tier.window_seconds else {
                return false;
            };
            let Some(resets_at) = tier
                .resets_at
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.timestamp_millis())
            else {
                return false;
            };
            window_seconds >= LONG_CYCLE_MIN_SECONDS
                && tier.utilization > 0.0
                && tier.utilization <= 100.0
                && resets_at > queried_at
                && resets_at.saturating_sub(window_seconds.saturating_mul(1000)) <= queried_at
        })
        .max_by_key(|tier| tier.window_seconds.unwrap_or_default())
}

fn analytics_range(quota: &SubscriptionQuota) -> Option<(String, String)> {
    if !quota.success {
        return None;
    }
    let queried_at = quota.queried_at?;
    let tier = long_cycle_tier(quota)?;
    let window_seconds = tier.window_seconds?;
    let reset_at = DateTime::parse_from_rfc3339(tier.resets_at.as_deref()?)
        .ok()?
        .with_timezone(&Utc);
    let cycle_start = reset_at - Duration::seconds(window_seconds);
    let query_end = DateTime::from_timestamp_millis(queried_at)?;
    Some((
        (cycle_start - Duration::days(1))
            .date_naive()
            .format("%Y-%m-%d")
            .to_string(),
        (query_end + Duration::days(1))
            .date_naive()
            .format("%Y-%m-%d")
            .to_string(),
    ))
}

pub async fn query_codex_quota_snapshot() -> Result<CodexQuotaSnapshot, String> {
    let credentials = read_codex_request_credentials()?;
    let result = query_quota(&credentials).await?;
    Ok(CodexQuotaSnapshot {
        quota: result.quota,
        email: result.email,
        credential_source: credentials.source,
        credential_scope: credentials.account_scope,
    })
}

pub async fn query_codex_official_usage_snapshot() -> Result<CodexOfficialUsageSnapshot, String> {
    // Resolve once: both HTTP phases borrow this immutable snapshot even if the
    // user switches the CLI account while the refresh is in flight.
    let credentials = read_codex_request_credentials()?;
    let result = query_quota(&credentials).await?;
    let quota = result.quota;
    let analytics = match analytics_range(&quota) {
        Some((start_date, end_date)) => Some(
            query_codex_usage_analytics_with_credentials(&start_date, &end_date, &credentials)
                .await?,
        ),
        None => None,
    };
    let queried_at = quota
        .queried_at
        .unwrap_or_else(|| Utc::now().timestamp_millis());

    Ok(CodexOfficialUsageSnapshot {
        quota,
        analytics,
        email: result.email,
        credential_source: credentials.source,
        credential_scope: credentials.account_scope,
        queried_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::subscription::{CredentialStatus, QuotaTier};

    fn quota() -> SubscriptionQuota {
        SubscriptionQuota {
            tool: "codex".to_string(),
            credential_status: CredentialStatus::Valid,
            credential_message: None,
            success: true,
            tiers: vec![QuotaTier {
                name: "seven_day".to_string(),
                window_seconds: Some(604_800),
                utilization: 30.0,
                resets_at: Some("2026-08-28T00:00:00Z".to_string()),
                used_value_usd: None,
                max_value_usd: None,
            }],
            extra_usage: None,
            error: None,
            queried_at: Some(1_777_075_200_000), // 2026-04-24; replaced below
        }
    }

    #[test]
    fn derives_expanded_utc_range_from_the_same_quota_snapshot() {
        let mut value = quota();
        value.queried_at = Some(
            DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
                .unwrap()
                .timestamp_millis(),
        );
        assert_eq!(
            analytics_range(&value),
            Some(("2026-08-20".to_string(), "2026-08-25".to_string()))
        );
    }

    #[test]
    fn refuses_analytics_when_quota_has_no_current_long_cycle() {
        let mut value = quota();
        value.tiers[0].window_seconds = Some(18_000);
        assert_eq!(analytics_range(&value), None);
    }
}
