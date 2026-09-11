//! Account-consistent Codex quota and official analytics snapshots.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::services::codex_auth_credentials::{CodexCredentialSnapshot, CodexCredentialSource};
use crate::services::codex_usage_analytics::{
    query_codex_usage_analytics_with_credentials, CodexAnalyticsUsage,
};
use crate::services::subscription::{
    query_codex_quota_with_metadata, read_codex_request_credentials, CodexQuotaQueryResult,
    CodexQuotaWindow, QuotaTier, SubscriptionQuota,
};

const LONG_CYCLE_MIN_SECONDS: i64 = 6 * 24 * 60 * 60;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexQuotaSnapshot {
    pub quota: SubscriptionQuota,
    pub quota_windows: Vec<CodexQuotaWindow>,
    pub email: Option<String>,
    pub credential_source: CodexCredentialSource,
    pub credential_scope: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexOfficialUsageSnapshot {
    pub quota: SubscriptionQuota,
    pub quota_windows: Vec<CodexQuotaWindow>,
    pub analytics: Option<CodexAnalyticsUsage>,
    /// Analytics failed after quota succeeded. No HTTP body or credential crosses IPC.
    pub analytics_unavailable: bool,
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
        quota_windows: result.quota_windows,
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
    let analytics_result = match analytics_range(&quota) {
        Some((start_date, end_date)) => {
            query_codex_usage_analytics_with_credentials(&start_date, &end_date, &credentials)
                .await
                .map(Some)
        }
        None => Ok(None),
    };
    Ok(assemble_official_snapshot(
        result.quota_windows,
        result.email,
        quota,
        credentials,
        analytics_result,
    ))
}

fn assemble_official_snapshot(
    quota_windows: Vec<CodexQuotaWindow>,
    email: Option<String>,
    quota: SubscriptionQuota,
    credentials: CodexCredentialSnapshot,
    analytics_result: Result<Option<CodexAnalyticsUsage>, String>,
) -> CodexOfficialUsageSnapshot {
    let analytics_unavailable = analytics_result.is_err();
    let analytics = analytics_result.ok().flatten();
    let queried_at = quota
        .queried_at
        .unwrap_or_else(|| Utc::now().timestamp_millis());

    CodexOfficialUsageSnapshot {
        quota,
        quota_windows,
        analytics,
        analytics_unavailable,
        email,
        credential_source: credentials.source,
        credential_scope: credentials.account_scope,
        queried_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::subscription::{CredentialStatus, QuotaTier};

    #[tokio::test]
    #[ignore = "Manual read-only contract probe using the current Codex login; no secrets or amounts printed"]
    async fn live_personal_credit_contract() {
        let result = query_codex_official_usage_snapshot().await;
        assert!(
            result.is_ok(),
            "Current login could not query the official snapshot"
        );
        let snapshot = result.unwrap();
        println!(
            "quota_success={} analytics_unavailable={}",
            snapshot.quota.success, snapshot.analytics_unavailable
        );
        assert!(snapshot.quota.success, "Quota query did not succeed");
        let Some(analytics) = snapshot.analytics else {
            assert!(!snapshot.analytics_unavailable, "Analytics request failed");
            println!("No active nonzero long cycle; daily contract probe not applicable");
            return;
        };
        let Some(credits) = analytics.personal_credits else {
            println!("Workspace account; personal Credits contract not applicable");
            return;
        };
        println!("totals_status={:?} breakdown_status={:?} days={} known_credit_days={} known_token_days={}",
            credits.totals_status, credits.breakdown_status, credits.days.len(),
            credits.days.iter().filter(|day| day.credits.is_some()).count(),
            credits.days.iter().filter(|day| day.tokens.total_tokens.is_some()).count());
        assert_eq!(
            credits.totals_status,
            crate::services::codex_personal_credits::CreditDataStatus::Available
        );
    }

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
    fn analytics_failure_preserves_quota_identity_and_hides_error_content() {
        let credentials = CodexCredentialSnapshot {
            access_token: "secret-token".into(),
            account_id: Some("secret-account".into()),
            account_scope: "anonymous-scope".into(),
            source: CodexCredentialSource::File,
            last_refresh: None,
        };
        let snapshot = assemble_official_snapshot(
            vec![],
            Some("fixture@example.test".into()),
            quota(),
            credentials,
            Err("secret-http-body".into()),
        );
        assert!(snapshot.quota.success);
        assert_eq!(snapshot.quota.tiers[0].utilization, 30.0);
        assert!(snapshot.analytics_unavailable);
        assert!(snapshot.analytics.is_none());
        assert_eq!(snapshot.credential_scope, "anonymous-scope");
        assert_eq!(snapshot.email.as_deref(), Some("fixture@example.test"));
        assert!(!serde_json::to_string(&snapshot).unwrap().contains("secret"));
    }

    #[test]
    fn refuses_analytics_when_quota_has_no_current_long_cycle() {
        let mut value = quota();
        value.tiers[0].window_seconds = Some(18_000);
        assert_eq!(analytics_range(&value), None);
    }

    #[test]
    fn refuses_analytics_for_a_fresh_zero_usage_cycle() {
        let mut value = quota();
        value.queried_at = Some(
            DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
                .unwrap()
                .timestamp_millis(),
        );
        value.tiers[0].utilization = 0.0;

        assert_eq!(analytics_range(&value), None);
    }
}
