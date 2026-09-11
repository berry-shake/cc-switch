//! Validated personal-account daily credits. Never reprice or correct API credits.
//! The two daily endpoints may sync/fail independently; unknown is not zero.

use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CreditDataStatus {
    Available,
    Unavailable,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonalCreditModel {
    pub model: String,
    pub speed: String,
    pub credits: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CreditAllocation {
    Reported,
    Allocated,
    Unallocated,
    Mismatched,
    Pending,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonalTokenCounts {
    pub uncached_input_tokens: Option<f64>,
    pub cached_input_tokens: Option<f64>,
    pub cache_write_input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    pub total_tokens: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonalCreditDay {
    pub date: String,
    pub credits: Option<f64>,
    pub tokens: PersonalTokenCounts,
    pub models: Vec<PersonalCreditModel>,
    pub unallocated_credits: Option<f64>,
    pub allocation: CreditAllocation,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexPersonalCredits {
    pub totals_status: CreditDataStatus,
    pub breakdown_status: CreditDataStatus,
    pub days: Vec<PersonalCreditDay>,
}

fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    let parsed = match value {
        Value::Number(value) => value.as_f64()?,
        Value::String(value) if !value.trim().is_empty() => value.trim().parse().ok()?,
        _ => return None,
    };
    (parsed.is_finite() && parsed >= 0.0).then_some(parsed)
}

fn token(value: Option<&Value>) -> Option<f64> {
    number(value).filter(|n| n.fract() == 0.0 && *n <= 9_007_199_254_740_991.0)
}

fn tokens(value: &Value) -> PersonalTokenCounts {
    let uncached = token(value.get("uncached_text_input_tokens"));
    let cached = token(value.get("cached_text_input_tokens"));
    let output = token(value.get("text_output_tokens"));
    let write = [
        "cache_write_input_tokens",
        "cache_creation_input_tokens",
        "cache_creation_text_input_tokens",
        "cache_write_text_input_tokens",
    ]
    .iter()
    .find_map(|key| value.get(key));
    // Older daily endpoints don't report a cache-write bucket. A present invalid
    // value must not be treated as absent or silently included as zero.
    let cache_write = write.map_or(Some(0.0), |value| token(Some(value)));
    let total = if value.get("text_total_tokens").is_some() {
        token(value.get("text_total_tokens"))
    } else {
        uncached
            .zip(cached)
            .zip(output)
            .zip(cache_write)
            .map(|(((u, c), o), w)| u + c + o + w)
            .filter(|n| n.is_finite() && *n <= 9_007_199_254_740_991.0)
    };
    PersonalTokenCounts {
        uncached_input_tokens: uncached,
        cached_input_tokens: cached,
        output_tokens: output,
        cache_write_input_tokens: cache_write,
        total_tokens: total,
    }
}

fn daily_rows(value: &Value) -> Option<&Vec<Value>> {
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
    .iter()
    .find_map(|key| value.get(key).and_then(Value::as_array))
}

fn date(row: &Value) -> Option<String> {
    let raw = row.get("date")?.as_str()?;
    let key = raw.get(..10)?;
    let parsed = NaiveDate::parse_from_str(key, "%Y-%m-%d").ok()?;
    (parsed.format("%Y-%m-%d").to_string() == key).then(|| key.to_string())
}

type IndexedRows = BTreeMap<String, Value>;

fn index_rows(
    body: Option<&str>,
    totals: bool,
) -> (CreditDataStatus, IndexedRows, BTreeSet<String>, bool) {
    let Some(body) = body else {
        return (
            CreditDataStatus::Unavailable,
            BTreeMap::new(),
            BTreeSet::new(),
            false,
        );
    };
    let invalid = || {
        (
            CreditDataStatus::Invalid,
            BTreeMap::new(),
            BTreeSet::new(),
            false,
        )
    };
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return invalid();
    };
    let percent = value.get("units").and_then(Value::as_str) == Some("percent");
    let unit_valid = if totals {
        value.get("balance_unit").and_then(Value::as_str) == Some("credit")
    } else {
        matches!(
            value.get("units").and_then(Value::as_str),
            Some("percent" | "credit" | "credits")
        )
    };
    if !unit_valid || value.get("group_by").and_then(Value::as_str) != Some("day") {
        return invalid();
    }
    let Some(rows) = daily_rows(&value) else {
        return invalid();
    };
    let mut indexed = BTreeMap::new();
    let mut duplicate = BTreeSet::new();
    for row in rows {
        let Some(date) = date(row) else {
            return invalid();
        };
        if indexed.insert(date.clone(), row.clone()).is_some() {
            if totals {
                return invalid();
            }
            duplicate.insert(date);
        }
    }
    (CreditDataStatus::Available, indexed, duplicate, percent)
}

fn model_name(value: &Value) -> String {
    ["model", "model_id", "model_name", "name", "id"]
        .iter()
        .find_map(|key| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or("unknown")
        .trim()
        .to_ascii_lowercase()
}

fn allocate(day: &mut PersonalCreditDay, breakdown: &Value, percent: bool) {
    let Some(credits) = day.credits else {
        return;
    };
    let Some(items) = breakdown.get("models").and_then(Value::as_array) else {
        return;
    };
    let Some(weights) = items
        .iter()
        .map(|item| number(item.get("credits")))
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    let sum: f64 = weights.iter().sum();
    if !sum.is_finite() {
        return;
    }
    if credits == 0.0 && sum > 0.0 {
        day.allocation = CreditAllocation::Mismatched;
        return;
    }
    if !percent && (sum - credits).abs() > 1e-8_f64.max(credits * 1e-8) {
        day.allocation = CreditAllocation::Mismatched;
        return;
    }
    if sum <= 0.0 {
        if credits == 0.0 {
            day.allocation = CreditAllocation::Reported;
        }
        return;
    }
    day.models = items
        .iter()
        .zip(weights)
        .filter(|(_, weight)| *weight > 0.0)
        .map(|(item, weight)| PersonalCreditModel {
            model: model_name(item),
            speed: item
                .get("speed")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("unknown")
                .trim()
                .to_ascii_lowercase(),
            credits: if percent {
                credits * (weight / sum)
            } else {
                weight
            },
        })
        .collect();
    // Correct floating-point residue only, never round before aggregation.
    if percent {
        let assigned: f64 = day.models.iter().map(|model| model.credits).sum();
        if let Some(largest) = day
            .models
            .iter_mut()
            .max_by(|a, b| a.credits.total_cmp(&b.credits))
        {
            largest.credits += credits - assigned;
        }
    }
    day.unallocated_credits = Some(0.0);
    day.allocation = if percent {
        CreditAllocation::Allocated
    } else {
        CreditAllocation::Reported
    };
}

pub fn build_personal_credits(
    totals: Option<&str>,
    breakdown: Option<&str>,
) -> CodexPersonalCredits {
    let (totals_status, totals, _, _) = index_rows(totals, true);
    let (breakdown_status, breakdown, duplicates, percent) = index_rows(breakdown, false);
    let dates: BTreeSet<_> = totals
        .keys()
        .chain(
            breakdown
                .iter()
                .filter(|(_, row)| {
                    row.get("models")
                        .and_then(Value::as_array)
                        .is_some_and(|models| {
                            models
                                .iter()
                                .any(|model| number(model.get("credits")).is_some_and(|n| n > 0.0))
                        })
                })
                .map(|(date, _)| date),
        )
        .cloned()
        .collect();
    let days = dates
        .into_iter()
        .map(|date| {
            let raw = totals
                .get(&date)
                .map(|row| row.get("totals").unwrap_or(row));
            let credits = raw.and_then(|raw| number(raw.get("credits")));
            let mut day = PersonalCreditDay {
                date: date.clone(),
                credits,
                tokens: raw.map(tokens).unwrap_or_default(),
                models: Vec::new(),
                unallocated_credits: credits,
                allocation: if credits.is_some() {
                    CreditAllocation::Unallocated
                } else {
                    CreditAllocation::Pending
                },
            };
            if !duplicates.contains(&date) {
                if let Some(breakdown) = breakdown.get(&date) {
                    allocate(&mut day, breakdown, percent);
                }
            }
            day
        })
        .collect();
    CodexPersonalCredits {
        totals_status,
        breakdown_status,
        days,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn totals() -> Value {
        json!({"balance_unit":"credit","group_by":"day","data":[{
            "date":"2026-09-10","totals":{"credits":2500.25,
            "uncached_text_input_tokens":100,"cached_text_input_tokens":200,
            "text_output_tokens":30}}]})
    }

    fn breakdown() -> Value {
        json!({"units":"percent","group_by":"day","data":[{
        "date":"2026-09-10","models":[
            {"model":"GPT-6-Astra","speed":"FAST","credits":8},
            {"model":"new-unknown-model","speed":"standard","credits":2}
        ]}]})
    }

    fn build(totals: &Value, breakdown: &Value) -> CodexPersonalCredits {
        build_personal_credits(Some(&totals.to_string()), Some(&breakdown.to_string()))
    }

    #[test]
    fn preserves_raw_credits_precision_tokens_and_fast_without_repricing() {
        let data = build(&totals(), &breakdown());
        let day = &data.days[0];
        assert_eq!(day.credits, Some(2500.25));
        assert_eq!(day.tokens.total_tokens, Some(330.0));
        assert_eq!(day.models[0].model, "gpt-6-astra");
        assert_eq!(day.models[0].speed, "fast");
        assert!((day.models[0].credits - 2000.2).abs() < 1e-9);
        assert!((day.models.iter().map(|m| m.credits).sum::<f64>() - 2500.25).abs() < 1e-9);
        assert_eq!(day.allocation, CreditAllocation::Allocated);
    }

    #[test]
    fn absolute_credits_must_match_daily_total() {
        let mut models = breakdown();
        models["units"] = json!("credits");
        let data = build(&totals(), &models);
        assert_eq!(data.days[0].allocation, CreditAllocation::Mismatched);
        assert_eq!(data.days[0].unallocated_credits, Some(2500.25));
        assert!(data.days[0].models.is_empty());
        models["data"][0]["models"][0]["credits"] = json!(2498.25);
        let data = build(&totals(), &models);
        assert_eq!(data.days[0].allocation, CreditAllocation::Reported);
        assert_eq!(data.days[0].models[0].credits, 2498.25);
    }

    #[test]
    fn distinguishes_missing_credits_tokens_and_real_zero() {
        let mut totals = totals();
        totals["data"][0]["totals"] = json!({"credits":0,"text_total_tokens":0});
        let data = build(
            &totals,
            &json!({"units":"percent","group_by":"day","data":[]}),
        );
        assert_eq!(data.days[0].credits, Some(0.0));
        assert_eq!(data.days[0].tokens.total_tokens, Some(0.0));
        assert_eq!(data.days[0].tokens.cached_input_tokens, None);
        totals["data"][0]["totals"] = json!({});
        let data = build(&totals, &breakdown());
        assert_eq!(data.days[0].credits, None);
        assert_eq!(data.days[0].tokens.total_tokens, None);
        assert_eq!(data.days[0].allocation, CreditAllocation::Pending);
    }

    #[test]
    fn rejects_invalid_numeric_values_without_coercing_to_zero() {
        for invalid in [
            json!(null),
            json!(true),
            json!(""),
            json!("NaN"),
            json!("Infinity"),
            json!(-1),
        ] {
            let mut totals = totals();
            totals["data"][0]["totals"]["credits"] = invalid.clone();
            totals["data"][0]["totals"]["text_total_tokens"] = invalid;
            let data = build(&totals, &breakdown());
            assert_eq!(data.days[0].credits, None);
            assert_eq!(data.days[0].tokens.total_tokens, None);
        }
        let mut totals = totals();
        totals["data"][0]["totals"]["credits"] = json!("2500.25");
        assert_eq!(build(&totals, &breakdown()).days[0].credits, Some(2500.25));
    }

    #[test]
    fn rejects_unknown_units_granularity_bad_dates_and_duplicate_totals() {
        let mut cases = Vec::new();
        let mut wrong = totals();
        wrong["balance_unit"] = json!("percent");
        cases.push(wrong);
        let mut wrong = totals();
        wrong["group_by"] = json!("month");
        cases.push(wrong);
        let mut wrong = totals();
        wrong["data"][0]["date"] = json!("2026-02-30");
        cases.push(wrong);
        let mut wrong = totals();
        wrong["data"] = json!([wrong["data"][0], wrong["data"][0]]);
        cases.push(wrong);
        for wrong in cases {
            let data = build(&wrong, &breakdown());
            assert_eq!(data.totals_status, CreditDataStatus::Invalid);
            assert_eq!(data.days[0].credits, None);
        }
    }

    #[test]
    fn duplicate_or_invalid_breakdowns_preserve_unallocated_total() {
        let mut models = breakdown();
        models["data"] = json!([models["data"][0], models["data"][0]]);
        let data = build(&totals(), &models);
        assert_eq!(data.days[0].unallocated_credits, Some(2500.25));
        assert!(data.days[0].models.is_empty());
        for invalid in [json!(null), json!(-1), json!("bad")] {
            let mut models = breakdown();
            models["data"][0]["models"][0]["credits"] = invalid;
            let data = build(&totals(), &models);
            assert_eq!(data.days[0].allocation, CreditAllocation::Unallocated);
        }
    }

    #[test]
    fn unions_delayed_dates_and_survives_either_missing_endpoint() {
        let mut models = breakdown();
        models["data"][0]["date"] = json!("2026-09-11");
        let data = build(&totals(), &models);
        assert_eq!(data.days.len(), 2);
        assert_eq!(data.days[0].credits, Some(2500.25));
        assert_eq!(data.days[1].credits, None);
        let data = build_personal_credits(Some(&totals().to_string()), None);
        assert_eq!(data.days[0].credits, Some(2500.25));
        assert_eq!(data.breakdown_status, CreditDataStatus::Unavailable);
        let data = build_personal_credits(None, Some(&models.to_string()));
        assert_eq!(data.totals_status, CreditDataStatus::Unavailable);
        assert_eq!(data.days[0].date, "2026-09-11");
        assert_eq!(data.days[0].credits, None);
    }

    #[test]
    fn zero_daily_total_with_positive_breakdown_is_not_allocated() {
        let mut totals = totals();
        totals["data"][0]["totals"]["credits"] = json!(0);
        let data = build(&totals, &breakdown());
        assert_eq!(data.days[0].allocation, CreditAllocation::Mismatched);
        assert!(data.days[0].models.is_empty());
    }

    #[test]
    fn preserves_cache_writes_and_refuses_invalid_token_counts() {
        let mut totals = totals();
        totals["data"][0]["totals"]["cache_creation_input_tokens"] = json!(20);
        assert_eq!(
            build(&totals, &breakdown()).days[0].tokens.total_tokens,
            Some(350.0)
        );
        totals["data"][0]["totals"]["cache_creation_input_tokens"] = json!(1.5);
        assert_eq!(
            build(&totals, &breakdown()).days[0].tokens.total_tokens,
            None
        );
    }
}
