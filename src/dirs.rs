//! Where the core keeps things by default, by the usual Linux convention:
//! configuration (models file, AGENTS file, skills) in `~/.config/conversation-core`,
//! data (sessions, sign-ins) in `~/.local/share/conversation-core`, and what it can fetch
//! again (model lists) in `~/.cache/conversation-core`. `XDG_CONFIG_HOME`, `XDG_DATA_HOME`
//! and `XDG_CACHE_HOME` move them, as they do for other programs.

use std::path::PathBuf;

const NAME: &str = "conversation-core";

fn base(xdg: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(xdg) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(fallback),
            None => PathBuf::from(fallback),
        },
    }
    .join(NAME)
}

pub fn config() -> PathBuf {
    base("XDG_CONFIG_HOME", ".config")
}

pub fn data() -> PathBuf {
    base("XDG_DATA_HOME", ".local/share")
}

pub fn cache() -> PathBuf {
    base("XDG_CACHE_HOME", ".cache")
}
