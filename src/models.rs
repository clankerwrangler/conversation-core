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

    /// The model's context window in tokens, if the file knows it.
    pub fn window(&self, model: &str) -> Option<u64> {
        self.models
            .get(model)?
            .get("contextWindow")?
            .as_u64()
            .filter(|w| *w > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
