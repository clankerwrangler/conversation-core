//! The command line as other programs use it.
use serde_json::Value;
use std::process::Command;

fn core(args: &[&str]) -> (bool, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_core"))
        .args(args)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    (
        out.status.success(),
        serde_json::from_str(text.trim()).unwrap_or(Value::Null),
    )
}

#[test]
fn usage_answers_in_json_without_a_session() {
    let dir = std::env::temp_dir().join(format!("core-cli-usage-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let auth = dir.join("auth.json");
    let auth = auth.to_str().unwrap();
    let (ok, codex) = core(&["usage", "openai-codex", "--auth", auth]);
    assert!(ok);
    assert_eq!(codex["provider"], "openai-codex");
    assert_eq!(codex["status"], "sign_in_required", "no sign-in yet");
    assert_eq!(codex["remaining"], Value::Null);
    let (_, other) = core(&["usage", "openrouter", "--auth", auth]);
    assert_eq!(other["status"], "unsupported");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn authentication_defaults_require_an_absolute_data_or_home_directory() {
    let root = std::env::temp_dir().join(format!(
        "core-cli-auth-paths-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_core"));
        cmd.current_dir(&root)
            .env_remove("HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_CACHE_HOME");
        cmd
    };
    let home = root.join("home");
    let data = root.join("data");
    std::fs::create_dir_all(&home).unwrap();

    // Absolute XDG data takes precedence, even without HOME.
    let out = command()
        .env("XDG_DATA_HOME", &data)
        .args(["logout", "openai-codex"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(data.join("conversation-core/auth.json.lock").exists());

    // An absolute HOME supplies the usual default when XDG_DATA_HOME is unset.
    let default_home = root.join("default-home");
    std::fs::create_dir_all(&default_home).unwrap();
    let out = command()
        .env("HOME", &default_home)
        .args(["logout", "openai-codex"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        default_home
            .join(".local/share/conversation-core/auth.json.lock")
            .exists()
    );

    // Relative XDG data is ignored; an absolute HOME supplies the fallback.
    let out = command()
        .env("HOME", &home)
        .env("XDG_DATA_HOME", "relative-data")
        .args(["logout", "openai-codex"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        home.join(".local/share/conversation-core/auth.json.lock")
            .exists()
    );
    assert!(!root.join("relative-data").exists());

    // Missing or relative paths fail before a session or credential file is created.
    for args in [
        vec!["logout", "openai-codex"],
        vec!["usage", "openrouter"],
        vec!["run", "session"],
    ] {
        let out = command().args(args).output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("cannot determine credentials path"));
    }
    let out = command()
        .env("HOME", "relative-home")
        .env("XDG_DATA_HOME", "relative-data")
        .args(["logout", "openai-codex"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!root.join("relative-home").exists());
    assert!(!root.join(".local").exists());
    assert!(!root.join("session").exists());

    // JSON login reports the failure as JSON instead of waiting for a sign-in.
    let out = command()
        .args(["login", "openai-codex", "device", "--json"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("cannot determine credentials path")
    );

    // An explicit path remains valid, including a path relative to the working directory.
    let out = command()
        .args(["logout", "openai-codex", "--auth", "explicit/auth.json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(root.join("explicit/auth.json.lock").exists());
    let out = command()
        .args(["run", "explicit-session", "--auth", "explicit/auth.json"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());

    // Other XDG locations also ignore relative overrides.
    let config = home.join(".config/conversation-core");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("AGENTS.md"), "configuration fallback marker").unwrap();
    let out = command()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", "relative-config")
        .args(["prompt"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("configuration fallback marker"));
    assert!(!root.join("relative-config").exists());

    // Commands that do not use credentials do not need either environment variable.
    let out = command().args(["tool", "ls", "{}"]).output().unwrap();
    assert!(out.status.success());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn mapped_specifications_name_existing_source_files() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for entry in std::fs::read_dir(root.join("specification")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|x| x.to_str()) != Some("tla") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let Some((_, after_header)) = text.split_once("\\* Sources:\n") else {
            assert_ne!(
                path.file_name().unwrap(),
                "CoreProviderFailover.tla",
                "missing failover source mapping"
            );
            continue; // Older models without a source mapping.
        };
        let sources: Vec<_> = after_header
            .lines()
            .map_while(|line| line.strip_prefix("\\*   "))
            .collect();
        assert!(!sources.is_empty(), "empty source mapping: {path:?}");
        for source in sources {
            assert!(
                root.join(source).is_file(),
                "{path:?} names missing source {source}"
            );
        }
    }
}
