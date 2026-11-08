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

/// A provider's cached list, if any: (fetched at, models, etag).
fn read_cache(dir: &Path, provider: &str) -> Option<(u64, Vec<Value>, Option<String>)> {
    let text = std::fs::read_to_string(dir.join(format!("{provider}.json"))).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let models = value.get("models")?.as_array()?.clone();
    let etag = value
        .get("etag")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some((
        value.get("checkedAt").and_then(Value::as_u64).unwrap_or(0),
        models,
        etag,
    ))
}

fn write_cache(
    dir: &Path,
    provider: &str,
    models: &[Value],
    etag: Option<&str>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{provider}.json.{}", std::process::id()));
    std::fs::write(
        &tmp,
        json!({"checkedAt": now_ms(), "etag": etag, "models": models}).to_string(),
    )?;
    std::fs::rename(tmp, dir.join(format!("{provider}.json")))
}

/// GET a JSON list: (status, body, etag). A 304 (unchanged) has no body.
fn get(url: &str, headers: &[(&str, String)]) -> Result<(u16, Value, Option<String>), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();
    let mut call = agent.get(url).header("accept", "application/json");
    for (k, v) in headers {
        call = call.header(*k, v);
    }
    let mut response = call
        .call()
        .map_err(|_| "models_list_unreachable".to_string())?;
    let status = response.status().as_u16();
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if status != 200 {
        return Ok((status, Value::Null, etag));
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .map_err(|_| "models_list_unreadable")?;
    serde_json::from_str(&body)
        .map(|v| (status, v, etag))
        .map_err(|_| "models_list_invalid".to_string())
}

pub const CODEX: &str = "openai-codex";
const CODEX_BASE: &str = "https://chatgpt.com/backend-api";
/// OpenAI lists a model only to clients at least as new as its `minimal_client_version`.
const CODEX_CLIENT_VERSION: &str = "1.5.0";

/// One entry of OpenAI's Codex list as a model description in pi's shape. The reasoning
/// levels it lists become pi's `thinkingLevelMap` (a level it does not list is null, so
/// the session's level is fitted to the nearest one it has). OpenAI gives no prices.
pub fn codex_model(entry: &Value, base_url: &str) -> Option<Value> {
    let id = entry.get("slug")?.as_str()?;
    if entry
        .get("visibility")
        .and_then(Value::as_str)
        .is_some_and(|v| v != "list")
    {
        return None;
    }
    let efforts: Vec<&str> = entry
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|l| l.get("effort").and_then(Value::as_str))
        .collect();
    let mut map = Map::new();
    // OpenAI's list never offers "none", but most Codex models accept it. "off" is sent as
    // "none"; a model that refuses it is learned (see learn_reasoning_mandatory).
    map.insert("off".into(), json!("none"));
    for level in ["minimal", "low", "medium", "high", "xhigh", "max"] {
        map.insert(
            level.into(),
            if efforts.contains(&level) {
                json!(level)
            } else {
                Value::Null
            },
        );
    }
    let inputs: Vec<&str> = entry
        .get("input_modalities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|m| matches!(*m, "text" | "image"))
        .collect();
    let mut model = json!({"id": id, "name": entry.get("display_name").cloned().unwrap_or(json!(id)),
        "api": "openai-codex-responses", "provider": CODEX, "baseUrl": base_url,
        "reasoning": !efforts.is_empty(), "input": if inputs.is_empty() { json!(["text"]) } else { json!(inputs) }});
    if !efforts.is_empty() {
        model["thinkingLevelMap"] = Value::Object(map);
    }
    if let Some(w) = entry
        .get("context_window")
        .and_then(Value::as_u64)
        .filter(|w| *w > 0)
    {
        model["contextWindow"] = json!(w);
    }
    Some(model)
}

/// Fetch OpenAI's Codex list, as the Codex CLI does (`<base>/codex/models` with the
/// account's sign-in), and cache it. An unchanged list (304) keeps the cached one.
pub fn refresh_codex(
    dir: &Path,
    base_url: &str,
    token: Option<&str>,
) -> Result<Vec<Value>, String> {
    let token = token
        .filter(|t| !t.is_empty())
        .ok_or("codex_models_need_sign_in")?;
    let base = base_url.trim_end_matches('/');
    let cached = read_cache(dir, CODEX);
    let mut headers = vec![
        ("authorization", format!("Bearer {token}")),
        ("originator", "pi".to_string()),
    ];
    if let Some(account) = crate::provider::codex_account(token) {
        headers.push(("chatgpt-account-id", account));
    }
    if let Some((_, models, Some(etag))) = &cached
        && !models.is_empty()
    {
        headers.push(("if-none-match", etag.clone()));
    }
    let (status, body, etag) = get(
        &format!("{base}/codex/models?client_version={CODEX_CLIENT_VERSION}"),
        &headers,
    )?;
    if status == 304
        && let Some((_, models, old)) = cached
    {
        write_cache(dir, CODEX, &models, old.as_deref()).map_err(|_| "codex_models_not_saved")?;
        return Ok(models);
    }
    if status != 200 {
        return Err(format!("codex_models_http_{status}"));
    }
    let entries = body
        .get("models")
        .and_then(Value::as_array)
        .ok_or("codex_models_invalid")?;
    let models: Vec<Value> = entries
        .iter()
        .filter_map(|e| codex_model(e, base_url))
        .collect();
    write_cache(dir, CODEX, &models, etag.as_deref()).map_err(|_| "codex_models_not_saved")?;
    Ok(models)
}

/// Fetches a provider's list now and caches it (cache folder, endpoint, key).
type Refresh = fn(&Path, &str, Option<&str>) -> Result<Vec<Value>, String>;

/// A provider's fetched list: the cache, fetched first when there is none, refreshed in the
/// background when it is older than 4 hours.
fn fetched(
    dir: &Path,
    provider: &'static str,
    base_url: &str,
    key: Option<&str>,
    refresh: Refresh,
) -> Vec<Value> {
    match read_cache(dir, provider) {
        Some((checked, models, _)) => {
            if now_ms().saturating_sub(checked) > REFRESH.as_millis() as u64 {
                let (dir, base, key) = (
                    dir.to_path_buf(),
                    base_url.to_string(),
                    key.map(str::to_string),
                );
                std::thread::spawn(move || {
                    let _ = refresh(&dir, &base, key.as_deref());
                });
            }
            models
        }
        None => refresh(dir, base_url, key).unwrap_or_default(),
    }
}

pub fn codex_models(dir: &Path, base_url: &str, key: Option<&str>) -> Vec<Value> {
    fetched(dir, CODEX, base_url, key, refresh_codex)
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
        let (status, value, _) = get(
            &format!("{base}/models/user"),
            &[("authorization", format!("Bearer {key}"))],
        )?;
        if status == 200 {
            body = value;
        } else if ![401, 403, 404].contains(&status) {
            return Err(format!("openrouter_models_http_{status}"));
        }
    }
    if body.is_null() {
        let (status, value, _) = get(&format!("{base}/models"), &[])?;
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
    write_cache(dir, OPENROUTER, &models, None).map_err(|_| "openrouter_models_not_saved")?;
    Ok(models)
}

pub fn openrouter_models(dir: &Path, base_url: &str, key: Option<&str>) -> Vec<Value> {
    fetched(dir, OPENROUTER, base_url, key, refresh_openrouter)
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

/// The providers whose lists the core discovers, with their default endpoints.
const DISCOVERED: [(&str, &str); 2] = [(OPENROUTER, OPENROUTER_BASE), (CODEX, CODEX_BASE)];

/// A provider's discovered list (empty for providers the core cannot ask).
fn discovered(dir: &Path, provider: &str, base_url: Option<&str>, key: Option<&str>) -> Vec<Value> {
    match provider {
        OPENROUTER => openrouter_models(dir, base_url.unwrap_or(OPENROUTER_BASE), key),
        CODEX => codex_models(dir, base_url.unwrap_or(CODEX_BASE), key),
        _ => Vec::new(),
    }
}

/// `over`'s fields over `base`'s.
fn overlay(base: Option<Value>, over: Option<&Value>) -> Option<Value> {
    match (base, over) {
        (Some(Value::Object(mut b)), Some(Value::Object(o))) => {
            for (k, v) in o {
                b.insert(k.clone(), v.clone());
            }
            Some(Value::Object(b))
        }
        (b, None) => b,
        (_, Some(o)) => Some(o.clone()),
    }
}

/// A provider's list: the shipped one (the models file), updated by what was discovered:
/// a discovered model's fields replace the shipped ones, and new models are added (as pi
/// merges its built-in list with downloaded ones).
fn provider_models(
    models: &Models,
    dir: &Path,
    provider: &str,
    base_url: Option<&str>,
    key: Option<&str>,
) -> Vec<Value> {
    let found = discovered(dir, provider, base_url, key);
    let mut out: Vec<Value> = models
        .all()
        .filter(|m| m.get("provider").and_then(Value::as_str) == Some(provider))
        .map(|m| {
            let newer = found.iter().find(|f| f["id"] == m["id"]);
            overlay(Some(m), newer).unwrap_or(Value::Null)
        })
        .collect();
    for model in found {
        if !out.iter().any(|m| m["id"] == model["id"]) {
            out.push(model);
        }
    }
    out
}

fn resolve_listed(models: &Models, dir: &Path, requested: &Value, key: Option<&str>) -> Value {
    let provider = requested
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("");
    let id = requested.get("id").and_then(Value::as_str).unwrap_or("");
    let base_url = requested.get("baseUrl").and_then(Value::as_str);
    let listed = provider_models(models, dir, provider, base_url, key)
        .into_iter()
        .find(|m| m["id"] == id);
    // A shipped entry without a provider applies to any provider.
    let listed = listed.or_else(|| {
        models
            .get(id)
            .filter(|m| m.get("provider").is_none())
            .cloned()
    });
    overlay(listed, Some(requested)).unwrap_or_else(|| requested.clone())
}

/// Every model the core knows for a provider (all providers when None).
pub fn list(models: &Models, dir: &Path, provider: Option<&str>, key: Option<&str>) -> Vec<Value> {
    let mut providers: Vec<String> = models
        .all()
        .filter_map(|m| {
            m.get("provider")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .chain(DISCOVERED.iter().map(|(p, _)| p.to_string()))
        .collect();
    providers.dedup();
    let mut seen = std::collections::HashSet::new();
    providers
        .into_iter()
        .filter(|p| provider.is_none_or(|w| w == p) && seen.insert(p.clone()))
        .flat_map(|p| provider_models(models, dir, &p, None, key))
        .collect()
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
    fn a_codex_entry_becomes_a_model() {
        let entry = json!({"slug": "gpt-6.1-sol", "display_name": "GPT-6.1-Sol", "context_window": 500000, "visibility": "list",
            "input_modalities": ["text", "image"], "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"},
            {"effort": "high"}, {"effort": "xhigh"}, {"effort": "max"}, {"effort": "ultra"}]});
        let m = codex_model(&entry, CODEX_BASE).unwrap();
        assert_eq!(
            m,
            json!({"id": "gpt-6.1-sol", "name": "GPT-6.1-Sol", "api": "openai-codex-responses", "provider": "openai-codex",
            "baseUrl": CODEX_BASE, "reasoning": true, "input": ["text", "image"],
            "thinkingLevelMap": {"off": "none", "minimal": null, "low": "low", "medium": "medium", "high": "high", "xhigh": "xhigh", "max": "max"},
            "contextWindow": 500000})
        );
        // Fitted as pi fits it: minimal goes up to low.
        let r = |l: Option<&str>| crate::models::reasoning(&m, None, l).effort;
        assert_eq!(
            (r(None), r(Some("minimal"))),
            (Some("none".into()), Some("low".into()))
        );
        assert!(
            codex_model(&json!({"slug": "hidden", "visibility": "hide"}), CODEX_BASE).is_none()
        );
    }

    #[test]
    fn discovery_updates_the_shipped_list() {
        let dir = std::env::temp_dir().join(format!("core-catalog-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shipped = Models::parse(
            r#"{"models": {
            "a": {"provider": "openai-codex", "contextWindow": 1, "maxTokens": 5},
            "gone": {"provider": "openai-codex", "contextWindow": 2}}}"#,
        )
        .unwrap();
        let found = [
            json!({"id": "a", "provider": "openai-codex", "contextWindow": 10}),
            json!({"id": "new", "provider": "openai-codex"}),
        ];
        write_cache(&dir, CODEX, &found, None).unwrap();
        let ids = |list: &[Value]| {
            list.iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let listed = list(&shipped, &dir, Some(CODEX), None);
        assert_eq!(ids(&listed), ["a", "gone", "new"]);
        assert_eq!(listed[0]["contextWindow"], 10, "the discovered value wins");
        assert_eq!(
            listed[0]["maxTokens"], 5,
            "a shipped value nothing replaced is kept"
        );
        let got = resolve(
            &shipped,
            &dir,
            &json!({"provider": "openai-codex", "id": "a"}),
            None,
        );
        assert_eq!(
            (got["contextWindow"].clone(), got["maxTokens"].clone()),
            (json!(10), json!(5))
        );
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
