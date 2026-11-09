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
