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

/// A CLI error is a definite failure: that batch keeps its original text and the
/// other batches still run (it must not freeze the task as "result uncertain").
#[test]
fn a_failed_batch_does_not_stop_the_other_batches() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let ok = dir.path().join("ok.txt");
    std::fs::write(&ok, r#"{"type":"result","subtype":"success","is_error":false,"structured_output":{"segments":[{"id":0,"text":"fixed"}]}}"#).unwrap();
    let bad = dir.path().join("bad.txt");
    std::fs::write(&bad, r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#).unwrap();
    let script = dir.path().join("claude-flaky");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif grep -q BAD; then cat '{bad}'; exit 1; fi\ncat '{ok}'\n",
            bad = bad.display(),
            ok = ok.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: environment changes are serialised by ENV_LOCK.
    unsafe { std::env::set_var("COURSE2MD_CLAUDE_BIN", &script) };
    std::fs::write(dir.path().join("s.jpg"), b"\xFF\xD8jpeg").unwrap();
    let section = |t: f64, text: &str| Section { t, end: t + 1.0, image: "s.jpg".into(), speech: vec![event(t, text)] };
    let mut sections = vec![section(0.0, "BAD"), section(1.0, "helo"), section(2.0, "wrld")];
    let settings = LlmSettings { enabled: true, provider: LlmProvider::ClaudeCode, model: "sonnet".into(), vision: true, ..LlmSettings::default() };
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let _ledger = course2md::dispatch::install(&work, None, &Default::default()).unwrap();

    let report = polish_sections_report(&mut sections, dir.path(), &settings).unwrap();

    assert_eq!((report.succeeded, report.failed), (2, 1), "{report:?}");
    assert_eq!(sections[0].speech[0].text, "BAD");
    assert_eq!(sections[1].speech[0].text, "fixed");
    assert_eq!(sections[2].speech[0].text, "fixed");
    assert!(report.note.as_deref().unwrap_or_default().contains("error_max_turns"), "{report:?}");
}
