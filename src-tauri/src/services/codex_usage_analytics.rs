//! Codex official usage API reader.
//!
//! The ChatGPT Codex analytics endpoints expose model-level token totals for
//! workspaces. Personal accounts expose daily total tokens separately from
//! model/speed credit shares; the frontend applies the same rate-adjusted
//! allocation used by the reference userscript without ever receiving OAuth
//! credentials.

use chrono::NaiveDate;
use reqwest::{RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const WORKSPACE_USAGE_URL: &str =
    "https://chatgpt.com/backend-api/wham/usage/daily-workspace-user-token-usage-breakdown";
const PERSONAL_MODEL_USAGE_URL: &str =
    "https://chatgpt.com/backend-api/wham/usage/daily-token-usage-breakdown";
const PERSONAL_TOTALS_URL: &str =
    "https://chatgpt.com/backend-api/wham/analytics/daily-workspace-usage-counts";
const REQUEST_TIMEOUT_SECS: u64 = 15;
const ERROR_BODY_MAX_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexAnalyticsAccountMode {
    Workspace,
    Personal,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAnalyticsTokenCounts {
    pub uncached_input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl CodexAnalyticsTokenCounts {
    fn from_raw(raw: &RawTokenCounts) -> Self {
        Self {
            uncached_input_tokens: raw.uncached_text_input_tokens,
            cached_input_tokens: raw.cached_text_input_tokens,
            cache_write_input_tokens: raw.cache_write_input_tokens,
            output_tokens: raw.text_output_tokens,
            total_tokens: raw.text_total_tokens,
        }
    }

    fn add_model(&mut self, model: &RawModelUsage) {
        self.uncached_input_tokens = self
            .uncached_input_tokens
            .saturating_add(model.tokens.uncached_text_input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(model.tokens.cached_text_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(model.tokens.cache_write_input_tokens);
        self.output_tokens = self
            .output_tokens
            .saturating_add(model.tokens.text_output_tokens);
        self.total_tokens = self
            .total_tokens
            .saturating_add(model.tokens.effective_total());
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAnalyticsModelUsage {
    pub model: String,
    pub speed: String,
    pub credits: f64,
    pub tokens: CodexAnalyticsTokenCounts,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAnalyticsDailyUsage {
    pub date: String,
    pub totals: CodexAnalyticsTokenCounts,
    pub models: Vec<CodexAnalyticsModelUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAnalyticsUsage {
    pub account_mode: CodexAnalyticsAccountMode,
    pub days: Vec<CodexAnalyticsDailyUsage>,
    pub queried_at: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawTokenCounts {
    #[serde(default)]
    uncached_text_input_tokens: u64,
    #[serde(default)]
    cached_text_input_tokens: u64,
    #[serde(
        default,
        alias = "cache_creation_input_tokens",
        alias = "cache_creation_text_input_tokens",
        alias = "cache_write_text_input_tokens"
    )]
    cache_write_input_tokens: u64,
    #[serde(default)]
    text_output_tokens: u64,
    #[serde(default)]
    text_total_tokens: u64,
}

impl RawTokenCounts {
    fn effective_total(&self) -> u64 {
        if self.text_total_tokens > 0 {
            self.text_total_tokens
        } else {
            self.uncached_text_input_tokens
                .saturating_add(self.cached_text_input_tokens)
                .saturating_add(self.cache_write_input_tokens)
                .saturating_add(self.text_output_tokens)
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawModelUsage {
    #[serde(
        default,
        alias = "model_id",
        alias = "model_name",
        alias = "name",
        alias = "id"
    )]
    model: String,
    #[serde(default)]
    speed: String,
    #[serde(default)]
    credits: f64,
    #[serde(flatten)]
    tokens: RawTokenCounts,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawDailyUsage {
    #[serde(default)]
    date: String,
    #[serde(default)]
    totals: RawTokenCounts,
    #[serde(default)]
    models: Vec<RawModelUsage>,
}

fn normalize_model(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        "unknown".to_string()
    } else {
        value
    }
}

fn normalize_speed(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        "standard".to_string()
    } else {
        value
    }
}

fn extract_daily_rows(body: &str) -> Result<Vec<RawDailyUsage>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| format!("Failed to parse Codex analytics JSON: {error}"))?;
    let rows = value.as_array().or_else(|| {
        [
            "data",
            "items",
            "results",
            "daily",
            "daily_usage",
            "dailyWorkspaceUsageCounts",
            "daily_workspace_usage_counts",
            "workspace_usage_counts",
        ]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_array))
    });

    match rows {
        Some(rows) => serde_json::from_value(Value::Array(rows.clone()))
            .map_err(|error| format!("Failed to parse Codex analytics rows: {error}")),
        None => Ok(Vec::new()),
    }
}

fn to_public_model(raw: RawModelUsage) -> CodexAnalyticsModelUsage {
    CodexAnalyticsModelUsage {
        model: normalize_model(&raw.model),
        speed: normalize_speed(&raw.speed),
        credits: raw.credits,
        tokens: CodexAnalyticsTokenCounts::from_raw(&raw.tokens),
    }
}

fn build_workspace_days(body: &str) -> Result<Vec<CodexAnalyticsDailyUsage>, String> {
    extract_daily_rows(body).map(|rows| {
        rows.into_iter()
            .filter(|row| !row.date.trim().is_empty())
            .map(|row| {
                let mut totals = CodexAnalyticsTokenCounts::default();
                for model in &row.models {
                    totals.add_model(model);
                }
                CodexAnalyticsDailyUsage {
                    date: row.date.chars().take(10).collect(),
                    totals,
                    models: row.models.into_iter().map(to_public_model).collect(),
                }
            })
            .collect()
    })
}

fn build_personal_days(
    totals_body: &str,
    breakdown_body: &str,
) -> Result<Vec<CodexAnalyticsDailyUsage>, String> {
    let breakdown_by_date: HashMap<String, Vec<RawModelUsage>> =
        extract_daily_rows(breakdown_body)?
            .into_iter()
            .map(|row| (row.date.chars().take(10).collect(), row.models))
            .collect();

    extract_daily_rows(totals_body).map(|rows| {
        rows.into_iter()
            .filter(|row| !row.date.trim().is_empty())
            .map(|row| {
                let date: String = row.date.chars().take(10).collect();
                let models = breakdown_by_date
                    .get(&date)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(to_public_model)
                    .collect();
                CodexAnalyticsDailyUsage {
                    date,
                    totals: CodexAnalyticsTokenCounts::from_raw(&row.totals),
                    models,
                }
            })
            .collect()
    })
}

fn with_auth_headers(
    request: RequestBuilder,
    token: &str,
    account_id: Option<&str>,
) -> RequestBuilder {
    let request = request
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "codex-cli")
        .header("Accept", "application/json");
    match account_id {
        Some(account_id) if !account_id.trim().is_empty() => {
            request.header("ChatGPT-Account-Id", account_id)
        }
        _ => request,
    }
}

async fn fetch_daily_body(
    url: &str,
    token: &str,
    account_id: Option<&str>,
    start_date: &str,
    end_date: &str,
    workspace_user: bool,
) -> Result<(StatusCode, String), String> {
    let client = crate::proxy::http_client::get();
    let mut query = vec![
        ("start_date", start_date),
        ("end_date", end_date),
        ("group_by", "day"),
    ];
    if workspace_user {
        query.push(("workspace_user", "true"));
    }
    let request = with_auth_headers(client.get(url).query(&query), token, account_id);
    let response = request
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|error| format!("Codex analytics network error: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Failed to read Codex analytics response: {error}"))?;
    Ok((status, body))
}

fn truncate_body(body: &str) -> String {
    if body.chars().count() <= ERROR_BODY_MAX_CHARS {
        body.to_string()
    } else {
        let mut value: String = body.chars().take(ERROR_BODY_MAX_CHARS).collect();
        value.push_str("...");
        value
    }
}

fn ensure_success(status: StatusCode, body: &str, label: &str) -> Result<(), String> {
    if status.is_success() {
        Ok(())
    } else {
        Err(format!(
            "{label} returned HTTP {status}: {}",
            truncate_body(body)
        ))
    }
}

fn validate_date_range(start_date: &str, end_date: &str) -> Result<(), String> {
    let start = NaiveDate::parse_from_str(start_date, "%Y-%m-%d")
        .map_err(|_| "Codex analytics startDate must use YYYY-MM-DD".to_string())?;
    let end = NaiveDate::parse_from_str(end_date, "%Y-%m-%d")
        .map_err(|_| "Codex analytics endDate must use YYYY-MM-DD".to_string())?;
    if start > end {
        return Err("Codex analytics startDate must not be after endDate".to_string());
    }
    Ok(())
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub async fn query_codex_usage_analytics(
    start_date: &str,
    end_date: &str,
) -> Result<CodexAnalyticsUsage, String> {
    validate_date_range(start_date, end_date)?;
    let (token, account_id) = crate::services::subscription::read_codex_request_credentials()?;
    let account_id = account_id.as_deref();

    let (workspace_status, workspace_body) = fetch_daily_body(
        WORKSPACE_USAGE_URL,
        &token,
        account_id,
        start_date,
        end_date,
        false,
    )
    .await?;

    if workspace_status.is_success() {
        return Ok(CodexAnalyticsUsage {
            account_mode: CodexAnalyticsAccountMode::Workspace,
            days: build_workspace_days(&workspace_body)?,
            queried_at: now_millis(),
        });
    }

    let no_active_workspace = workspace_status == StatusCode::BAD_REQUEST
        && workspace_body
            .to_ascii_lowercase()
            .contains("no active workspace");
    if !no_active_workspace {
        ensure_success(
            workspace_status,
            &workspace_body,
            "Codex workspace analytics",
        )?;
    }

    let breakdown_request = fetch_daily_body(
        PERSONAL_MODEL_USAGE_URL,
        &token,
        account_id,
        start_date,
        end_date,
        false,
    );
    let totals_request = fetch_daily_body(
        PERSONAL_TOTALS_URL,
        &token,
        account_id,
        start_date,
        end_date,
        true,
    );
    let ((breakdown_status, breakdown_body), (totals_status, totals_body)) =
        tokio::try_join!(breakdown_request, totals_request)?;
    ensure_success(
        breakdown_status,
        &breakdown_body,
        "Codex personal model analytics",
    )?;
    ensure_success(
        totals_status,
        &totals_body,
        "Codex personal token analytics",
    )?;

    Ok(CodexAnalyticsUsage {
        account_mode: CodexAnalyticsAccountMode::Personal,
        days: build_personal_days(&totals_body, &breakdown_body)?,
        queried_at: now_millis(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_payload_preserves_models_speeds_and_token_buckets() {
        let days = build_workspace_days(
            r#"{
                "data": [{
                    "date": "2026-08-22",
                    "models": [{
                        "model": "GPT-5.6-Sol",
                        "speed": "FAST",
                        "uncached_text_input_tokens": 100,
                        "cached_text_input_tokens": 200,
                        "cache_creation_input_tokens": 30,
                        "text_output_tokens": 40,
                        "text_total_tokens": 370
                    }]
                }]
            }"#,
        )
        .expect("parse workspace payload");

        assert_eq!(days.len(), 1);
        assert_eq!(days[0].models[0].model, "gpt-5.6-sol");
        assert_eq!(days[0].models[0].speed, "fast");
        assert_eq!(days[0].totals.uncached_input_tokens, 100);
        assert_eq!(days[0].totals.cached_input_tokens, 200);
        assert_eq!(days[0].totals.cache_write_input_tokens, 30);
        assert_eq!(days[0].totals.output_tokens, 40);
        assert_eq!(days[0].totals.total_tokens, 370);
    }

    #[test]
    fn personal_payload_joins_daily_totals_with_model_credit_shares() {
        let days = build_personal_days(
            r#"{
                "data": [{
                    "date": "2026-08-22",
                    "totals": {
                        "uncached_text_input_tokens": 1000,
                        "cached_text_input_tokens": 2000,
                        "text_output_tokens": 300,
                        "text_total_tokens": 3300
                    }
                }]
            }"#,
            r#"{
                "data": [{
                    "date": "2026-08-22",
                    "models": [
                        {"model": "gpt-5.6-sol", "speed": "fast", "credits": 8},
                        {"model": "gpt-5.6-luna", "speed": "standard", "credits": 2}
                    ]
                }]
            }"#,
        )
        .expect("parse personal payloads");

        assert_eq!(days.len(), 1);
        assert_eq!(days[0].totals.total_tokens, 3300);
        assert_eq!(days[0].models.len(), 2);
        assert_eq!(days[0].models[0].credits, 8.0);
        assert_eq!(days[0].models[1].speed, "standard");
    }

    #[test]
    fn date_range_validation_rejects_invalid_and_reversed_dates() {
        assert!(validate_date_range("2026/08/22", "2026-08-23").is_err());
        assert!(validate_date_range("2026-08-24", "2026-08-23").is_err());
        assert!(validate_date_range("2026-08-22", "2026-08-23").is_ok());
    }
}
