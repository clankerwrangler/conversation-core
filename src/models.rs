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
}

pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".config/conversation-core/models.json"))
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
        match value.get("models") {
            Some(Value::Object(models)) => Ok(Models {
                models: models.clone(),
            }),
            _ => Err("expected {\"models\": {...}}".into()),
        }
    }

    /// Everything the file says about a model.
    pub fn get(&self, model: &str) -> Option<&Value> {
        self.models.get(model)
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

const LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// pi's `getSupportedThinkingLevels`.
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

/// pi's `clampThinkingLevel`: the requested level if supported, else the nearest higher
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
}

/// `requested` is the session's level (None = off). `model` is the runner's model object
/// (pi's shape); `reasoning` and `thinkingLevelMap` missing there come from the models file.
/// A model nobody describes keeps the old behaviour: the level is sent as given.
pub fn reasoning(model: &Value, file: Option<&Value>, requested: Option<&str>) -> Reasoning {
    let field = |key: &str| model.get(key).or_else(|| file.and_then(|f| f.get(key)));
    let Some(supports) = field("reasoning").and_then(Value::as_bool) else {
        return Reasoning {
            include: requested.is_some(),
            effort: requested.map(str::to_string),
        };
    };
    if !supports {
        return Reasoning {
            include: false,
            effort: None,
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
                effort: None
            }
        );
        assert_eq!(
            reasoning(&json!({}), None, Some("high")).effort,
            Some("high".into()),
            "unknown model: as given"
        );
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
