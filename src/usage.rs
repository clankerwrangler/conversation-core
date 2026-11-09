//! A provider's remaining usage, read with the core's own sign-in (renewed first when it is
//! about to expire), so nothing outside the core needs its credentials. The answer is
//! `{provider, model: null, status, remaining}` with `status` one of `ready`,
//! `unavailable` (the provider did not answer usefully), `sign_in_required` (no sign-in,
//! or the provider refused it) and `unsupported`.
//!
//! - `openai-codex`: ChatGPT's usage windows; `remaining` is the most-used window
//!   (`kind: "rate_limits"`) with every window listed.
//! - `grok-cli`: the weekly credit allowance (`kind: "weekly_allowance"`), per-product use
//!   and the subscription tier.
use crate::auth;
use crate::store::iso_from_ms;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(8);
const CODEX_USAGE: &str = "https://chatgpt.com/backend-api/wham/usage";
const MAX_BODY: u64 = 256 * 1024;
const MAX_PRODUCTS: usize = 8;

pub fn usage(auth_path: &Path, provider: &str, now_ms: u64) -> Value {
    match provider {
        auth::CODEX => codex(auth_path, now_ms),
        auth::GROK => grok(auth_path, now_ms),
        _ => answer(provider, "unsupported", Value::Null),
    }
}

fn answer(provider: &str, status: &str, remaining: Value) -> Value {
    json!({"provider": provider, "model": null, "status": status, "remaining": remaining})
}

enum Got {
    Json(Value),
    Refused,
    Failed,
}

fn get(url: &str, headers: &[(&str, &str)]) -> Got {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .max_redirects(0)
        .build()
        .into();
    let mut call = agent.get(url).header("accept", "application/json");
    for (k, v) in headers {
        call = call.header(*k, *v);
    }
    let Ok(mut response) = call.call() else {
        return Got::Failed;
    };
    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Got::Refused,
        _ => return Got::Failed,
    }
    let Ok(body) = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
    else {
        return Got::Failed;
    };
    match serde_json::from_str::<Value>(&body) {
        Ok(value) if value.is_object() => Got::Json(value),
        _ => Got::Failed,
    }
}

/// The stored sign-in for `provider`, or the status to answer with.
fn signed_in(auth_path: &Path, provider: &str) -> Result<Value, &'static str> {
    match auth::credentials(auth_path, provider) {
        Ok(Some(c))
            if c.get("token")
                .and_then(Value::as_str)
                .is_some_and(|t| !t.is_empty()) =>
        {
            Ok(c)
        }
        Ok(_) => Err("sign_in_required"),
        // Renewal failed: possibly offline, so not necessarily a lost sign-in.
        Err(_) => Err("unavailable"),
    }
}

fn codex(auth_path: &Path, now_ms: u64) -> Value {
    let creds = match signed_in(auth_path, auth::CODEX) {
        Ok(c) => c,
        Err(status) => return answer(auth::CODEX, status, Value::Null),
    };
    let token = creds["token"].as_str().unwrap_or("");
    let Some(account) = creds
        .get("accountId")
        .and_then(Value::as_str)
        .filter(|a| !a.is_empty())
    else {
        return answer(auth::CODEX, "sign_in_required", Value::Null);
    };
    let bearer = format!("Bearer {token}");
    match get(
        CODEX_USAGE,
        &[("authorization", &bearer), ("chatgpt-account-id", account)],
    ) {
        Got::Json(payload) => parse_codex(&payload, now_ms),
        Got::Refused => answer(auth::CODEX, "sign_in_required", Value::Null),
        Got::Failed => answer(auth::CODEX, "unavailable", Value::Null),
    }
}

/// ChatGPT's usage windows; the most-used one is shown, not a made-up request count.
pub fn parse_codex(payload: &Value, now_ms: u64) -> Value {
    let unavailable = || answer(auth::CODEX, "unavailable", Value::Null);
    let Some(limits) = payload.get("rate_limit").filter(|v| v.is_object()) else {
        return unavailable();
    };
    let mut windows = Vec::new();
    for id in ["primary", "secondary"] {
        let raw = &limits[format!("{id}_window")];
        if raw.is_null() {
            continue;
        }
        let used = raw
            .get("used_percent")
            .and_then(Value::as_f64)
            .filter(|u| (0.0..=100.0).contains(u));
        let seconds = raw
            .get("limit_window_seconds")
            .and_then(Value::as_i64)
            .filter(|s| *s > 0);
        let reset = raw
            .get("reset_at")
            .and_then(Value::as_i64)
            .filter(|r| *r > 0);
        let (Some(used), Some(seconds), Some(reset)) = (used, seconds, reset) else {
            return unavailable();
        };
        windows.push(
            json!({"id": id, "windowSeconds": seconds, "percentUsed": used,
            "percentRemaining": 100.0 - used, "resetsAt": iso_from_ms(reset.saturating_mul(1000))}),
        );
    }
    let Some(limiting) = windows.iter().fold(None::<&Value>, |most, w| match most {
        Some(m) if m["percentUsed"].as_f64() >= w["percentUsed"].as_f64() => Some(m),
        _ => Some(w),
    }) else {
        return unavailable();
    };
    let remaining = json!({"kind": "rate_limits", "percentUsed": limiting["percentUsed"],
        "percentRemaining": limiting["percentRemaining"], "resetsAt": limiting["resetsAt"],
        "limitingWindow": limiting["id"], "windows": windows,
        "updatedAt": iso_from_ms(now_ms as i64), "fresh": true, "source": "live"});
    answer(auth::CODEX, "ready", remaining)
}

fn grok(auth_path: &Path, now_ms: u64) -> Value {
    let creds = match signed_in(auth_path, auth::GROK) {
        Ok(c) => c,
        Err(status) => return answer(auth::GROK, status, Value::Null),
    };
    let token = creds["token"].as_str().unwrap_or("");
    let base = creds
        .get("baseUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end_matches('/');
    if !base.starts_with("https://") {
        return answer(auth::GROK, "unavailable", Value::Null);
    }
    let bearer = format!("Bearer {token}");
    let headers = [
        ("authorization", bearer.as_str()),
        ("x-xai-token-auth", "xai-grok-cli"),
    ];
    let credits = match get(&format!("{base}/billing?format=credits"), &headers) {
        Got::Json(v) => v,
        Got::Refused => return answer(auth::GROK, "sign_in_required", Value::Null),
        Got::Failed => return answer(auth::GROK, "unavailable", Value::Null),
    };
    // The tier is optional: a failed settings read leaves it out.
    let tier = match get(&format!("{base}/settings"), &headers) {
        Got::Json(v) => v
            .get("subscription_tier_display")
            .and_then(Value::as_str)
            .filter(|t| plain(t, 64))
            .map(str::to_string),
        _ => None,
    };
    parse_grok(&credits, tier.as_deref(), now_ms)
}

fn plain(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}

fn percent(value: Option<f64>) -> Option<f64> {
    value
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(0.0, 100.0).round())
}

/// The weekly credit allowance from `/billing?format=credits`.
pub fn parse_grok(payload: &Value, tier: Option<&str>, now_ms: u64) -> Value {
    let unavailable = || answer(auth::GROK, "unavailable", Value::Null);
    let Some(config) = payload.get("config").filter(|v| v.is_object()) else {
        return unavailable();
    };
    let Some(resets) = config
        .get("billingPeriodEnd")
        .and_then(Value::as_str)
        .filter(|t| plain(t, 64) && t.as_bytes().get(4) == Some(&b'-'))
    else {
        return unavailable();
    };
    let used = match config.get("creditUsagePercent") {
        Some(v) => percent(v.as_f64()),
        None => {
            let cap = config.pointer("/onDemandCap/val").and_then(Value::as_f64);
            let spent = config
                .pointer("/onDemandUsed/val")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            match cap {
                Some(cap) if cap > 0.0 => percent(Some(spent / cap * 100.0)),
                _ => Some(0.0),
            }
        }
    };
    let Some(used) = used else {
        return unavailable();
    };
    let products: Vec<Value> = config
        .get("productUsage")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item
                .get("product")
                .and_then(Value::as_str)
                .filter(|p| plain(p, 64))?;
            let p = percent(item.get("usagePercent").and_then(Value::as_f64))?;
            Some(json!({"id": id, "percentUsed": p, "percentRemaining": 100.0 - p}))
        })
        .take(MAX_PRODUCTS)
        .collect();
    let remaining = json!({"kind": "weekly_allowance", "percentRemaining": 100.0 - used, "percentUsed": used,
        "resetsAt": resets, "updatedAt": iso_from_ms(now_ms as i64), "fresh": true, "source": "live",
        "tier": tier, "products": products});
    answer(auth::GROK, "ready", remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_shows_the_most_used_window_with_all_windows() {
        let payload = json!({"rate_limit": {
            "primary_window": {"used_percent": 20.0, "limit_window_seconds": 18_000, "reset_at": 1_800_000_000},
            "secondary_window": {"used_percent": 75.5, "limit_window_seconds": 604_800, "reset_at": 1_800_500_000}}});
        let got = parse_codex(&payload, 1_799_999_000_000);
        assert_eq!(got["status"], "ready");
        let r = &got["remaining"];
        assert_eq!(r["limitingWindow"], "secondary");
        assert_eq!(r["percentUsed"], 75.5);
        assert_eq!(r["percentRemaining"], 24.5);
        assert_eq!(r["resetsAt"], iso_from_ms(1_800_500_000_000));
        assert_eq!(r["windows"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn codex_answers_unavailable_for_anything_malformed() {
        for payload in [
            json!({}),
            json!({"rate_limit": {}}),
            json!({"rate_limit": {"primary_window": {"used_percent": 120.0, "limit_window_seconds": 1, "reset_at": 1}}}),
            json!({"rate_limit": {"primary_window": {"used_percent": 10.0, "limit_window_seconds": 0, "reset_at": 1}}}),
        ] {
            assert_eq!(
                parse_codex(&payload, 0)["status"],
                "unavailable",
                "{payload}"
            );
        }
    }

    #[test]
    fn grok_reads_the_weekly_allowance_products_and_tier() {
        let payload = json!({"config": {"creditUsagePercent": 33.4, "billingPeriodEnd": "2026-11-16T00:00:00Z",
            "productUsage": [{"product": "grok-code", "usagePercent": 40}, {"product": "bad\nname", "usagePercent": 1}]}});
        let got = parse_grok(&payload, Some("SuperGrok"), 0);
        let r = &got["remaining"];
        assert_eq!(got["status"], "ready");
        assert_eq!(r["percentUsed"], 33.0);
        assert_eq!(r["percentRemaining"], 67.0);
        assert_eq!(r["resetsAt"], "2026-11-16T00:00:00Z");
        assert_eq!(r["tier"], "SuperGrok");
        assert_eq!(
            r["products"],
            json!([{"id": "grok-code", "percentUsed": 40.0, "percentRemaining": 60.0}])
        );
        let cap = json!({"config": {"onDemandCap": {"val": 200.0}, "onDemandUsed": {"val": 50.0}, "billingPeriodEnd": "2026-11-16T00:00:00Z"}});
        assert_eq!(parse_grok(&cap, None, 0)["remaining"]["percentUsed"], 25.0);
        assert_eq!(
            parse_grok(&json!({"config": {}}), None, 0)["status"],
            "unavailable"
        );
    }

    #[test]
    fn no_sign_in_asks_for_one_and_other_providers_are_unsupported() {
        let dir = std::env::temp_dir().join(format!("core-usage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        assert_eq!(usage(&path, auth::CODEX, 0)["status"], "sign_in_required");
        assert_eq!(usage(&path, auth::GROK, 0)["status"], "sign_in_required");
        assert_eq!(usage(&path, "openrouter", 0)["status"], "unsupported");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
