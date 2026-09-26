//! Proofreading through a CLI Provider, end to end from the public entry point.
//! Separate test binary: it sets process-wide environment variables (serialised by ENV_LOCK).
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use course2md::llm::{LlmProvider, LlmSettings, polish_sections_report};
use course2md::timeline::{Section, TranscriptEvent};

/// Both tests point COURSE2MD_CLAUDE_BIN at their own fake program; run them one at a time.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn event(start: f64, text: &str) -> TranscriptEvent {
    TranscriptEvent { start, end: start + 1.0, text: text.into(), raw: None }
}

#[test]
fn claude_code_provider_rewrites_segments() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    std::fs::write(
        &out,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"```json\n{\"segments\":[{\"id\":0,\"text\":\"hello\"},{\"id\":1,\"text\":\"world\"}]}\n```"}"#,
    )
    .unwrap();
    let script = dir.path().join("claude");
    std::fs::write(&script, format!("#!/bin/sh\ncat > /dev/null\ncat '{}'\n", out.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // SAFETY: environment changes are serialised by ENV_LOCK.
    unsafe { std::env::set_var("COURSE2MD_CLAUDE_BIN", &script) };

    let mut sections = vec![Section { t: 0.0, end: 2.0, image: String::new(), speech: vec![event(0.0, "helo"), event(1.0, "wrld")] }];
    let settings = LlmSettings {
        enabled: true,
        provider: LlmProvider::ClaudeCode,
        model: "sonnet".into(),
        ..LlmSettings::default()
    };

    let report = polish_sections_report(&mut sections, dir.path(), &settings).unwrap();

    assert_eq!(report.succeeded, 2, "{report:?}");
    let texts: Vec<&str> = sections[0].speech.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(texts, ["hello", "world"]);
}

#[test]
fn connection_test_runs_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    std::fs::write(&out, r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#).unwrap();
    let script = dir.path().join("claude-ok");
    std::fs::write(&script, format!("#!/bin/sh\ncat > /dev/null\ncat '{}'\n", out.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // SAFETY: environment changes are serialised by ENV_LOCK.
    unsafe { std::env::set_var("COURSE2MD_CLAUDE_BIN", &script) };
    let settings = LlmSettings { enabled: true, provider: LlmProvider::ClaudeCode, model: "sonnet".into(), ..LlmSettings::default() };
    course2md::llm::test_connection(&settings).unwrap();
}
