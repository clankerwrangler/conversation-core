//! Where a model's details come from. The runner names a model (`provider`, `id`, and any
//! details it wants to set itself); the core fills in the rest:
//!
//! - OpenRouter: OpenRouter's own model list, fetched from OpenRouter (`/models/user` with
//!   the account's key, the models and prices that key may use, as pi does; `/models`
//!   without one), cached in `<catalog-dir>/openrouter.json` and refreshed every 4 hours.
//! - Every other provider (Codex, xAI): the models file, whose entries are full model
//!   descriptions in pi's shape.

use crate::models::Models;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const OPENROUTER: &str = "openrouter";
const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
const REFRESH: Duration = Duration::from_secs(4 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(15);

pub fn default_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| Path::new(&h).join(".config/conversation-core/catalog"))
        .unwrap_or_else(|| PathBuf::from(".config/conversation-core/catalog"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// pi's openRouterCost: OpenRouter's dollars per token as dollars per million tokens, with
/// prompt-length overrides as tiers.
fn per_million(value: Option<&Value>, fallback: f64) -> f64 {
    let parsed = match value {
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    };
    match parsed {
        Some(p) if p >= 0.0 => ((p * 1_000_000.0) * 1e6).round() / 1e6,
        _ => fallback,
    }
}

fn cost(pricing: Option<&Value>) -> Value {
    let p = |k: &str, f: f64| per_million(pricing.and_then(|p| p.get(k)), f);
    let base = (
        p("prompt", 0.0),
        p("completion", 0.0),
        p("input_cache_read", 0.0),
        p("input_cache_write", 0.0),
    );
    let mut out =
        json!({"input": base.0, "output": base.1, "cacheRead": base.2, "cacheWrite": base.3});
    let tiers: Vec<Value> = pricing
        .and_then(|p| p.get("overrides"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|o| o.get("min_prompt_tokens").is_some() && ["utc_start", "utc_end", "utc_days"].iter().all(|k| o.get(*k).is_none()))
        .map(|o| {
            let q = |k: &str, f: f64| per_million(o.get(k), f);
            json!({"inputTokensAbove": o["min_prompt_tokens"], "input": q("prompt", base.0), "output": q("completion", base.1),
                "cacheRead": q("input_cache_read", base.2), "cacheWrite": q("input_cache_write", base.3)})
        })
        .collect();
    if !tiers.is_empty() {
        out["tiers"] = Value::Array(tiers);
    }
    out
}

/// One entry of OpenRouter's list as a model description in pi's shape. None for models
/// that cannot chat.
pub fn openrouter_model(entry: &Value, base_url: &str) -> Option<Value> {
    let id = entry.get("id")?.as_str()?;
    let outputs = entry
        .pointer("/architecture/output_modalities")
        .and_then(Value::as_array);
    if outputs.is_some_and(|o| !o.iter().any(|m| m == "text")) {
        return None;
    }
    if let Some(endpoints) = entry.get("endpoints").and_then(Value::as_array)
        && !endpoints
            .iter()
            .any(|e| e.as_str().is_some_and(|e| e.ends_with("/chat/completions")))
    {
        return None;
    }
    let params: Vec<&str> = entry
        .get("supported_parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let images = entry
        .pointer("/architecture/input_modalities")
        .and_then(Value::as_array)
        .is_some_and(|m| m.iter().any(|m| m == "image"));
    let positive = |v: Option<&Value>| v.and_then(Value::as_u64).filter(|n| *n > 0);
    let window = positive(entry.pointer("/top_provider/context_length"))
        .or(positive(entry.get("context_length")));
    let reasoning = params.contains(&"reasoning");
    let mut compat = Map::new();
    if params.contains(&"structured_outputs") {
        compat.insert("supportsStrictMode".into(), json!(true));
    }
    // DeepSeek's reasoning models want reasoning_content on every assistant message.
    if reasoning && id.trim_start_matches('~').starts_with("deepseek/") {
        compat.insert(
            "requiresReasoningContentOnAssistantMessages".into(),
            json!(true),
        );
    }
    let mut model = json!({"id": id, "name": entry.get("name").cloned().unwrap_or(json!(id)), "api": crate::completions::API,
        "provider": OPENROUTER, "baseUrl": base_url, "reasoning": reasoning,
        "input": if images { json!(["text", "image"]) } else { json!(["text"]) },
        "cost": cost(entry.get("pricing"))});
    if let Some(w) = window {
        model["contextWindow"] = json!(w);
    }
    if let Some(m) = positive(entry.pointer("/top_provider/max_completion_tokens")) {
        model["maxTokens"] = json!(m);
    }
    if !compat.is_empty() {
        model["compat"] = Value::Object(compat);
    }
    Some(model)
}

/// The cached OpenRouter list, if any: (fetched at, models).
fn read_cache(dir: &Path) -> Option<(u64, Vec<Value>)> {
    let text = std::fs::read_to_string(dir.join("openrouter.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let models = value.get("models")?.as_array()?.clone();
    Some((
        value.get("checkedAt").and_then(Value::as_u64).unwrap_or(0),
        models,
    ))
}

fn write_cache(dir: &Path, models: &[Value]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".openrouter.json.{}", std::process::id()));
    std::fs::write(
        &tmp,
        json!({"checkedAt": now_ms(), "models": models}).to_string(),
    )?;
    std::fs::rename(tmp, dir.join("openrouter.json"))
}

fn get(url: &str, key: Option<&str>) -> Result<(u16, Value), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();
    let mut call = agent.get(url).header("accept", "application/json");
    if let Some(key) = key {
        call = call.header("authorization", format!("Bearer {key}"));
    }
    let mut response = call
        .call()
        .map_err(|_| "openrouter_models_unreachable".to_string())?;
    let status = response.status().as_u16();
    if status != 200 {
        return Ok((status, Value::Null));
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .map_err(|_| "openrouter_models_unreadable")?;
    serde_json::from_str(&body)
        .map(|v| (status, v))
        .map_err(|_| "openrouter_models_invalid".to_string())
}

/// Fetch OpenRouter's list now and cache it. With a key, the models that key may use
/// (`/models/user`); when the key is refused there, or without a key, the public list.
pub fn refresh_openrouter(
    dir: &Path,
    base_url: &str,
    key: Option<&str>,
) -> Result<Vec<Value>, String> {
    let base = base_url.trim_end_matches('/');
    let mut body = Value::Null;
    if let Some(key) = key.filter(|k| !k.is_empty()) {
        let (status, value) = get(&format!("{base}/models/user"), Some(key))?;
        if status == 200 {
            body = value;
        } else if ![401, 403, 404].contains(&status) {
            return Err(format!("openrouter_models_http_{status}"));
        }
    }
    if body.is_null() {
        let (status, value) = get(&format!("{base}/models"), None)?;
        if status != 200 {
            return Err(format!("openrouter_models_http_{status}"));
        }
        body = value;
    }
    let entries = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or("openrouter_models_invalid")?;
    let models: Vec<Value> = entries
        .iter()
        .filter_map(|e| openrouter_model(e, base_url))
        .collect();
    write_cache(dir, &models).map_err(|_| "openrouter_models_not_saved")?;
    Ok(models)
}

/// OpenRouter's models: the cache, fetched first when there is none, and refreshed in the
/// background when it is older than 4 hours.
pub fn openrouter_models(dir: &Path, base_url: &str, key: Option<&str>) -> Vec<Value> {
    match read_cache(dir) {
        Some((checked, models)) => {
            if now_ms().saturating_sub(checked) > REFRESH.as_millis() as u64 {
                let (dir, base, key) = (
                    dir.to_path_buf(),
                    base_url.to_string(),
                    key.map(str::to_string),
                );
                std::thread::spawn(move || {
                    let _ = refresh_openrouter(&dir, &base, key.as_deref());
                });
            }
            models
        }
        None => refresh_openrouter(dir, base_url, key).unwrap_or_default(),
    }
}

/// What the core has learned about models from the providers' answers, by `provider/id`.
/// OpenRouter's list does not say which models cannot switch reasoning off; when a provider
/// refuses "off" for one, that is remembered here as pi's `thinkingLevelMap.off = null`.
fn learned_path(dir: &Path) -> PathBuf {
    dir.join("learned.json")
}

fn learned(dir: &Path) -> Map<String, Value> {
    std::fs::read_to_string(learned_path(dir))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Remember that this model's reasoning cannot be switched off.
pub fn learn_reasoning_mandatory(dir: &Path, provider: &str, id: &str) {
    let mut all = learned(dir);
    all.insert(
        format!("{provider}/{id}"),
        json!({"thinkingLevelMap": {"off": null}}),
    );
    let _ = std::fs::create_dir_all(dir);
    let tmp = dir.join(format!(".learned.json.{}", std::process::id()));
    if std::fs::write(&tmp, Value::Object(all).to_string()).is_ok() {
        let _ = std::fs::rename(tmp, learned_path(dir));
    }
}

/// The model the runner named, with the catalog's details under the runner's own. The
/// runner's fields always win.
pub fn resolve(models: &Models, dir: &Path, requested: &Value, key: Option<&str>) -> Value {
    let mut model = resolve_listed(models, dir, requested, key);
    let provider = model
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let id = model
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(Value::Object(lesson)) = learned(dir).get(&format!("{provider}/{id}"))
        && let Some(Value::Object(levels)) = lesson.get("thinkingLevelMap")
        && requested.get("thinkingLevelMap").is_none()
    {
        if !model.get("thinkingLevelMap").is_some_and(Value::is_object) {
            model["thinkingLevelMap"] = json!({});
        }
        for (level, value) in levels {
            model["thinkingLevelMap"][level.as_str()] = value.clone();
        }
    }
    model
}

fn resolve_listed(models: &Models, dir: &Path, requested: &Value, key: Option<&str>) -> Value {
    let provider = requested
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("");
    let id = requested.get("id").and_then(Value::as_str).unwrap_or("");
    let base = if provider == OPENROUTER {
        let base_url = requested
            .get("baseUrl")
            .and_then(Value::as_str)
            .unwrap_or(OPENROUTER_BASE);
        openrouter_models(dir, base_url, key)
            .into_iter()
            .find(|m| m["id"] == id)
    } else {
        models
            .get(id)
            .filter(|m| {
                m.get("provider")
                    .and_then(Value::as_str)
                    .is_none_or(|p| p == provider)
            })
            .cloned()
    };
    let Some(Value::Object(mut merged)) = base else {
        return requested.clone();
    };
    if let Value::Object(own) = requested {
        for (k, v) in own {
            merged.insert(k.clone(), v.clone());
        }
    }
    Value::Object(merged)
}

/// Every model the core knows for a provider (all providers when None).
pub fn list(models: &Models, dir: &Path, provider: Option<&str>, key: Option<&str>) -> Vec<Value> {
    let mut out: Vec<Value> = models
        .all()
        .filter(|m| provider.is_none_or(|p| m.get("provider").and_then(Value::as_str) == Some(p)))
        .collect();
    if provider.is_none_or(|p| p == OPENROUTER) {
        out.extend(openrouter_models(dir, OPENROUTER_BASE, key));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_openrouter_entry_becomes_a_model() {
        let entry = json!({"id": "deepseek/deepseek-v4-flash", "name": "DeepSeek V4 Flash", "context_length": 1048576,
            "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
            "pricing": {"prompt": "0.000000091", "completion": "0.000000182", "input_cache_read": "0.0000000182"},
            "top_provider": {"context_length": 1048575, "max_completion_tokens": 943717},
            "supported_parameters": ["reasoning", "structured_outputs", "tools"], "endpoints": ["/api/v1/chat/completions"]});
        let m = openrouter_model(&entry, OPENROUTER_BASE).unwrap();
        assert_eq!(
            m,
            json!({"id": "deepseek/deepseek-v4-flash", "name": "DeepSeek V4 Flash", "api": "openai-completions",
            "provider": "openrouter", "baseUrl": OPENROUTER_BASE, "reasoning": true, "input": ["text"],
            "cost": {"input": 0.091, "output": 0.182, "cacheRead": 0.0182, "cacheWrite": 0.0},
            "contextWindow": 1048575, "maxTokens": 943717,
            "compat": {"supportsStrictMode": true, "requiresReasoningContentOnAssistantMessages": true}})
        );
        let image_only = json!({"id": "x/img", "architecture": {"output_modalities": ["image"]}});
        assert!(openrouter_model(&image_only, OPENROUTER_BASE).is_none());
    }

    #[test]
    fn the_runners_fields_win_and_other_providers_come_from_the_models_file() {
        let models = Models::parse(r#"{"models": {"gpt-x": {"provider": "openai-codex", "contextWindow": 1, "api": "openai-codex-responses"}}}"#).unwrap();
        let dir = std::env::temp_dir().join("core-catalog-none");
        let got = resolve(
            &models,
            &dir,
            &json!({"provider": "openai-codex", "id": "gpt-x", "contextWindow": 2}),
            None,
        );
        assert_eq!(
            got,
            json!({"provider": "openai-codex", "contextWindow": 2, "api": "openai-codex-responses", "id": "gpt-x"})
        );
        let other = resolve(
            &models,
            &dir,
            &json!({"provider": "grok-cli", "id": "gpt-x"}),
            None,
        );
        assert_eq!(
            other,
            json!({"provider": "grok-cli", "id": "gpt-x"}),
            "another provider's entry is not used"
        );
    }
}
