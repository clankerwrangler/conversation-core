//! Where the core keeps things by default, by the usual Linux convention:
//! configuration (models file, AGENTS file, skills) in `~/.config/conversation-core`,
//! data (sessions, sign-ins) in `~/.local/share/conversation-core`, and what it can fetch
//! again (model lists) in `~/.cache/conversation-core`. `XDG_CONFIG_HOME`, `XDG_DATA_HOME`
//! and `XDG_CACHE_HOME` move them, as they do for other programs.

use std::ffi::OsString;
use std::path::PathBuf;

const NAME: &str = "conversation-core";

fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

fn base(xdg: &str, fallback: &str) -> PathBuf {
    absolute(std::env::var_os(xdg))
        .unwrap_or_else(|| match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(fallback),
            None => PathBuf::from(fallback),
        })
        .join(NAME)
}

pub fn config() -> PathBuf {
    base("XDG_CONFIG_HOME", ".config")
}

pub fn data() -> Option<PathBuf> {
    absolute(std::env::var_os("XDG_DATA_HOME"))
        .or_else(|| absolute(std::env::var_os("HOME")).map(|home| home.join(".local/share")))
        .map(|base| base.join(NAME))
}

pub fn cache() -> PathBuf {
    base("XDG_CACHE_HOME", ".cache")
}
