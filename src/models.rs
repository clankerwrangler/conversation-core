//! The models file: facts about models (for now the context window), kept out of the code.
//! `~/.config/conversation-core/models.json` by default, or `core run ... --models <file>`.
//!
//! ```json
//! {"models": {"gpt-6.1-sol": {"contextWindow": 500000}}}
//! ```

use crate::store::Error;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Models {
    models: serde_json::Map<String, Value>,
    fallback: std::collections::HashMap<String, Value>,
}

pub fn default_path() -> Option<PathBuf> {
    Some(crate::dirs::config().join("models.json"))
}

impl Models {
    /// Load the given file (it must exist), or the default file if there is one.
    pub fn load(explicit: Option<&Path>) -> Result<Models, Error> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => match default_path().filter(|p| p.exists()) {
                Some(p) => p,
                None => return Ok(Models::default()),
            },
        };
        let text = std::fs::read_to_string(&path).map_err(|e| Error::Io(path.clone(), e))?;
        Models::parse(&text).map_err(|e| Error::Format(format!("{}: {e}", path.display())))
    }

    pub fn parse(text: &str) -> Result<Models, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let Some(models) = value.get("models").and_then(Value::as_object) else {
            return Err("expected {\"models\": {...}}".into());
        };
        let mut fallback = std::collections::HashMap::new();
        if let Some(routes) = value.get("fallback") {
            let routes = routes
                .as_object()
                .ok_or("expected fallback provider-to-model map")?;
            for (provider, target) in routes {
                let id = target
                    .as_str()
                    .filter(|id| !id.trim().is_empty())
                    .ok_or("fallback target must be a model key")?;
                let model = models
                    .get(id)
                    .filter(|m| m.is_object())
                    .cloned()
                    .ok_or("fallback target must be defined in models")?;
                let target_provider = model.get("provider").and_then(Value::as_str);
                if provider.trim().is_empty()
                    || target_provider.is_none_or(|p| p.trim().is_empty() || p == provider)
                    || model.get("id").is_some_and(|model_id| model_id != id)
                    || model["api"] != crate::completions::API
                    || !model["baseUrl"].as_str().is_some_and(loopback_url)
                    || !model["contextWindow"].as_u64().is_some_and(|w| w > 0)
                {
                    return Err("fallback needs a distinct provider and a local Chat Completions model with a known context window".into());
                }
                let mut model = model;
                model["id"] = Value::String(id.into());
                if model.get("maxTokens").is_none() {
                    model["maxTokens"] = Value::from(1024);
                }
                if !model["maxTokens"]
                    .as_u64()
                    .is_some_and(|n| n > 0 && n < model["contextWindow"].as_u64().unwrap())
                {
                    return Err(
                        "fallback maxTokens must be positive and smaller than contextWindow".into(),
                    );
                }
                fallback.insert(provider.clone(), model);
            }
        }
        Ok(Models {
            models: models.clone(),
            fallback,
        })
    }

    pub fn fallback(&self, primary_provider: &str) -> Option<&Value> {
        self.fallback.get(primary_provider)
    }

    /// Everything the file says about a model.
    pub fn get(&self, model: &str) -> Option<&Value> {
        self.models.get(model)
    }

    /// Every model in the file, each with its `id`.
    pub fn all(&self) -> impl Iterator<Item = Value> + '_ {
        self.models.iter().map(|(id, m)| {
            let mut m = m.clone();
            if m.get("id").is_none() && m.is_object() {
                m["id"] = Value::String(id.clone());
            }
            m
        })
    }

    /// The model's context window in tokens, if the file knows it.
    pub fn window(&self, model: &str) -> Option<u64> {
        self.models
            .get(model)?
            .get("contextWindow")?
            .as_u64()
            .filter(|w| *w > 0)
    }
}

fn loopback_url(url: &str) -> bool {
    url.parse::<ureq::http::Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https"))
            && uri.query().is_none()
            && uri.host().is_some_and(|host| {
                let host = host.trim_start_matches('[').trim_end_matches(']');
                host.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            })
    })
}

/// What a reply cost, using the model's `cost` rates (dollars per
/// million tokens, with optional `tiers` above an input size), then the harness's
/// service-tier pricing (flex half, priority double).
pub struct Pricing {
    rates: Option<Value>,
    model: String,
    requested_tier: Option<String>,
    codex: bool,
}

impl Pricing {
    /// Rates from the model object, else from the models file.
    pub fn new(model: &Value, file: Option<&Value>, requested_tier: Option<&str>) -> Pricing {
        let rates = model
            .get("cost")
            .or_else(|| file.and_then(|f| f.get("cost")))
            .filter(|c| c.is_object())
            .cloned();
        Pricing {
            rates,
            model: model
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            requested_tier: requested_tier.map(str::to_string),
            codex: model.get("api").and_then(Value::as_str) == Some("openai-codex-responses"),
        }
    }

    /// Fill `usage.cost`. `response_tier` is the tier the provider says it used.
    pub fn apply(&self, usage: &mut Value, response_tier: Option<&str>) {
        // OpenRouter's billed cost, which replaces the total worked out from the rates.
        let reported = usage.as_object_mut().and_then(|u| u.remove("_openrouter"));
        self.apply_rates(usage, response_tier);
        if let Some(reported) = reported {
            apply_openrouter_cost(usage, &reported);
        }
    }

    fn apply_rates(&self, usage: &mut Value, response_tier: Option<&str>) {
        let Some(rates) = &self.rates else { return };
        let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0) as f64;
        let (input, output, read, write) =
            (n("input"), n("output"), n("cacheRead"), n("cacheWrite"));
        let prompt = input + read + write;
        let mut chosen = rates;
        let mut matched = -1.0;
        for tier in rates
            .get("tiers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let above = tier
                .get("inputTokensAbove")
                .and_then(Value::as_f64)
                .unwrap_or(f64::INFINITY);
            if prompt > above && above > matched {
                chosen = tier;
                matched = above;
            }
        }
        let rate = |k: &str| chosen.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        let mut cost = [
            (rate("input") / 1_000_000.0) * input,
            (rate("output") / 1_000_000.0) * output,
            (rate("cacheRead") / 1_000_000.0) * read,
            (rate("cacheWrite") * write + rate("input") * 2.0 * 0.0) / 1_000_000.0,
            0.0,
        ];
        cost[4] = cost[0] + cost[1] + cost[2] + cost[3];
        let tier = response_tier.or(self.requested_tier.as_deref());
        let tier = if self.codex && tier == Some("default") {
            self.requested_tier.as_deref()
        } else {
            tier
        };
        let multiplier = match tier {
            Some("flex") => 0.5,
            Some("priority") if self.model == "gpt-5.5" => 2.5,
            Some("priority") => 2.0,
            _ => 1.0,
        };
        for c in &mut cost {
            *c *= multiplier;
        }
        usage["cost"] = serde_json::json!({"input": cost[0], "output": cost[1], "cacheRead": cost[2],
            "cacheWrite": cost[3], "total": cost[4], "totalFromProvider": false});
    }
}

/// Apply the cost reported by OpenRouter.
fn apply_openrouter_cost(usage: &mut Value, reported: &Value) {
    let valid = |v: Option<&Value>| {
        v.and_then(Value::as_f64)
            .filter(|c| c.is_finite() && *c >= 0.0)
    };
    let Some(cost) = valid(reported.get("cost")) else {
        return;
    };
    let total = match reported.get("is_byok").and_then(Value::as_bool) {
        Some(false) => cost,
        Some(true) => {
            let Some(details) = reported.get("cost_details").filter(|d| d.is_object()) else {
                return;
            };
            let upstream = match details.get("upstream_inference_cost") {
                Some(Value::Null) => {
                    match (
                        valid(details.get("upstream_inference_prompt_cost")),
                        valid(details.get("upstream_inference_completions_cost")),
                    ) {
                        (Some(p), Some(c)) => p + c,
                        _ => return,
                    }
                }
                other => match valid(other) {
                    Some(u) => u,
                    None => return,
                },
            };
            cost + upstream
        }
        None => return,
    };
    if !usage["cost"].is_object() {
        usage["cost"] = serde_json::json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0});
    }
    usage["cost"]["total"] = serde_json::json!(total);
    usage["cost"]["totalFromProvider"] = serde_json::json!(true);
}

const LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Get the supported thinking levels.
fn supported_levels(reasoning: bool, map: Option<&Value>) -> Vec<&'static str> {
    if !reasoning {
        return vec!["off"];
    }
    LEVELS
        .into_iter()
        .filter(|level| match map.and_then(|m| m.get(*level)) {
            Some(Value::Null) => false,
            mapped => !matches!(*level, "xhigh" | "max") || mapped.is_some(),
        })
        .collect()
}

/// Fit the reasoning level: the requested level if supported, else the nearest higher
/// one, else the nearest lower one.
fn clamp(reasoning: bool, map: Option<&Value>, level: &str) -> &'static str {
    let available = supported_levels(reasoning, map);
    let first = available.first().copied().unwrap_or("off");
    let Some(index) = LEVELS.iter().position(|l| *l == level) else {
        return first;
    };
    if available.contains(&LEVELS[index]) {
        return LEVELS[index];
    }
    LEVELS[index..]
        .iter()
        .chain(LEVELS[..index].iter().rev())
        .find(|l| available.contains(l))
        .copied()
        .unwrap_or(first)
}

/// What the request says about reasoning, as the harness's native provider builds it.
#[derive(Debug, PartialEq)]
pub struct Reasoning {
    /// Ask for encrypted reasoning back.
    pub include: bool,
    /// `reasoning.effort`, when the request carries a reasoning setting at all.
    pub effort: Option<String>,
    /// The level after fitting it to the model ("off" when none was asked for).
    pub level: String,
}

/// `requested` is the session's level (None = off). `model` is the runner's model object
/// (in Pi-compatible format); missing `reasoning` and `thinkingLevelMap` fields come from
/// the models file.
/// A model nobody describes keeps the old behaviour: the level is sent as given.
pub fn reasoning(model: &Value, file: Option<&Value>, requested: Option<&str>) -> Reasoning {
    let field = |key: &str| model.get(key).or_else(|| file.and_then(|f| f.get(key)));
    let Some(supports) = field("reasoning").and_then(Value::as_bool) else {
        return Reasoning {
            include: requested.is_some(),
            effort: requested.map(str::to_string),
            level: requested.unwrap_or("off").to_string(),
        };
    };
    if !supports {
        return Reasoning {
            include: false,
            effort: None,
            level: "off".into(),
        };
    }
    let map = field("thinkingLevelMap").filter(|m| m.is_object());
    let level = requested.map_or("off", |r| clamp(true, map, r));
    let mapped = map.and_then(|m| m.get(level));
    let effort = match mapped {
        Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        _ => Some(if level == "off" {
            "none".to_string()
        } else {
            level.to_string()
        }),
    };
    Reasoning {
        include: true,
        effort,
        level: level.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reasoning_levels_like_pi() {
        let sol = json!({"reasoning": true, "thinkingLevelMap": {"off": null, "minimal": "low", "low": "low", "medium": "medium", "high": "high", "xhigh": "xhigh", "max": "max"}});
        let old = json!({"reasoning": true, "thinkingLevelMap": {"xhigh": "xhigh", "max": "max", "minimal": "low"}});
        let r = |m: &Value, l: Option<&str>| reasoning(&json!({}), Some(m), l).effort;
        // Off is not offered by this model: no reasoning setting is sent.
        assert_eq!(r(&sol, None), None);
        assert_eq!(r(&sol, Some("minimal")), Some("low".into()));
        assert_eq!(r(&sol, Some("max")), Some("max".into()));
        assert_eq!(r(&old, None), Some("none".into()));
        assert_eq!(r(&old, Some("high")), Some("high".into()));
        let grok = json!({"reasoning": true, "thinkingLevelMap": {"off": null, "minimal": null, "low": "low", "medium": "medium", "high": "high", "xhigh": null, "max": null}});
        assert_eq!(
            r(&grok, Some("max")),
            Some("high".into()),
            "clamped down to the nearest supported level"
        );
        assert_eq!(
            r(&grok, Some("minimal")),
            Some("low".into()),
            "clamped up first"
        );
        assert_eq!(
            reasoning(&json!({"reasoning": false}), None, Some("high")),
            Reasoning {
                include: false,
                effort: None,
                level: "off".into()
            }
        );
        assert_eq!(
            reasoning(&json!({}), None, Some("high")).effort,
            Some("high".into()),
            "unknown model: as given"
        );
    }

    #[test]
    fn cost_like_pi_and_the_harness() {
        let file = json!({"cost": {"input": 2, "output": 10, "cacheRead": 0.1, "cacheWrite": 2.5,
            "tiers": [{"inputTokensAbove": 272000, "input": 4, "output": 15, "cacheRead": 0.2, "cacheWrite": 5}]}});
        let codex = json!({"id": "gpt-6.1-sol", "api": "openai-codex-responses"});
        let usage = |input: u64| json!({"input": input, "output": 500, "cacheRead": 1000, "cacheWrite": 0, "totalTokens": input + 1500});
        let cost = |p: &Pricing, input: u64, tier: Option<&str>| {
            let mut u = usage(input);
            p.apply(&mut u, tier);
            u["cost"].clone()
        };
        let plain = Pricing::new(&codex, Some(&file), None);
        let c = cost(&plain, 1000, None);
        // Preserve operation order for Pi-compatible floating-point results.
        assert_eq!(c["input"], json!((2.0 / 1_000_000.0) * 1000.0));
        assert_eq!(c["output"], json!((10.0 / 1_000_000.0) * 500.0));
        assert_eq!(c["cacheRead"], json!((0.1 / 1_000_000.0) * 1000.0));
        assert_eq!(
            c["total"],
            json!(
                (2.0 / 1_000_000.0) * 1000.0
                    + (10.0 / 1_000_000.0) * 500.0
                    + (0.1 / 1_000_000.0) * 1000.0
                    + 0.0
            )
        );
        assert_eq!(c["totalFromProvider"], false);
        // Above 272k prompt tokens the higher tier applies.
        assert_eq!(
            cost(&plain, 300000, None)["input"],
            json!((4.0 / 1_000_000.0) * 300000.0)
        );
        // Codex reports "default" for a priority request: priced as priority (double).
        let fast = Pricing::new(&codex, Some(&file), Some("priority"));
        assert_eq!(
            cost(&fast, 1000, Some("default"))["input"],
            json!((2.0 / 1_000_000.0) * 1000.0 * 2.0)
        );
        assert_eq!(
            cost(&plain, 1000, Some("flex"))["output"],
            json!((10.0 / 1_000_000.0) * 500.0 * 0.5)
        );
        // Rates on the model object win; with none at all, the cost stays as it was.
        let own = Pricing::new(
            &json!({"id": "x", "cost": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0}}),
            Some(&file),
            None,
        );
        assert_eq!(
            cost(&own, 1000, None)["input"],
            json!((1.0 / 1_000_000.0) * 1000.0)
        );
        let mut u = usage(1);
        Pricing::new(&json!({"id": "x"}), None, None).apply(&mut u, None);
        assert!(u.get("cost").is_none());
    }

    #[test]
    fn reads_windows() {
        let m = Models::parse(r#"{"models": {"a": {"contextWindow": 500000}, "b": {}}}"#)
            .unwrap_or_default();
        assert_eq!(
            (m.window("a"), m.window("b"), m.window("c")),
            (Some(500000), None, None)
        );
        assert!(Models::parse("{}").is_err());
    }
}
