//! CLI Provider seam: run a (fake) `claude` / `codex` program and get the model's text back.
//! The fake program records its argv and stdin, then replays recorded CLI output.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use course2md::cli_provider::{CliRunner, CliKind};
use course2md::llm::LlmProvider;

/// Writes an executable shell script that saves argv/stdin next to itself and prints `stdout`.
fn fake_cli(dir: &Path, stdout: &str, exit_code: i32) -> PathBuf {
    let out = dir.join("out.txt");
    std::fs::write(&out, stdout).unwrap();
    let script = dir.join("fake-cli");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{d}/argv.txt'\ncat > '{d}/stdin.txt'\ncat '{out}'\nexit {exit_code}\n",
            d = dir.display(),
            out = out.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn argv(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("argv.txt")).unwrap().lines().map(str::to_owned).collect()
}

fn chat_body(system: &str, user: serde_json::Value) -> serde_json::Value {
    course2md::llm::chat_body("sonnet", system, user, 16384)
}

const CLAUDE_OK: &str = r#"{"type":"system","subtype":"init","session_id":"s1"}
{"type":"assistant","message":{"content":[{"type":"text","text":"{\"segments\":[{\"id\":\"0\",\"text\":\"fixed\"}]}"}]}}
{"type":"result","subtype":"success","is_error":false,"result":"{\"segments\":[{\"id\":\"0\",\"text\":\"fixed\"}]}","session_id":"s1"}
"#;

#[test]
fn claude_code_returns_the_final_result_text() {
    let dir = tempfile::tempdir().unwrap();
    let runner = CliRunner::new(CliKind::ClaudeCode, fake_cli(dir.path(), CLAUDE_OK, 0));

    let body = chat_body("You proofread.", serde_json::json!([{"type": "text", "text": "[{\"id\":\"0\",\"text\":\"fixd\"}]"}]));
    let text = runner.complete("sonnet", &body, None, &|| false).unwrap();

    assert_eq!(text, r#"{"segments":[{"id":"0","text":"fixed"}]}"#);
    let args = argv(dir.path());
    for expected in ["-p", "--verbose", "--strict-mcp-config", "--input-format", "--output-format", "stream-json", "--max-turns", "1", "--tools", "--model", "sonnet", "--system-prompt", "You proofread."] {
        assert!(args.iter().any(|a| a == expected), "missing {expected} in {args:?}");
    }
    let stdin = std::fs::read_to_string(dir.path().join("stdin.txt")).unwrap();
    let msg: serde_json::Value = serde_json::from_str(stdin.lines().next().unwrap()).unwrap();
    assert_eq!(msg["type"], "user");
    assert_eq!(msg["message"]["content"][0]["text"], "[{\"id\":\"0\",\"text\":\"fixd\"}]");
}

#[test]
fn provider_maps_to_cli_kind() {
    assert_eq!(CliKind::of(LlmProvider::ClaudeCode), Some(CliKind::ClaudeCode));
    assert_eq!(CliKind::of(LlmProvider::OpenAiCompatible), None);
}

fn text_body() -> serde_json::Value {
    chat_body("sys", serde_json::json!([{"type": "text", "text": "hi"}]))
}

#[test]
fn claude_code_error_result_is_reported_not_returned() {
    let dir = tempfile::tempdir().unwrap();
    let out = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Credit balance is too low"}"#;
    let runner = CliRunner::new(CliKind::ClaudeCode, fake_cli(dir.path(), out, 1));
    let err = runner.complete("sonnet", &text_body(), None, &|| false).unwrap_err();
    assert!(err.message.contains("Credit balance is too low"), "{}", err.message);
    assert!(!err.not_started);
}

#[test]
fn nonzero_exit_without_output_is_an_error_with_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let runner = CliRunner::new(CliKind::ClaudeCode, fake_cli(dir.path(), "", 3));
    let err = runner.complete("sonnet", &text_body(), None, &|| false).unwrap_err();
    assert!(err.message.contains("exit 3"), "{}", err.message);
}

#[test]
fn missing_binary_means_the_request_was_never_sent() {
    let runner = CliRunner::new(CliKind::ClaudeCode, "/nonexistent/claude");
    let err = runner.complete("sonnet", &text_body(), None, &|| false).unwrap_err();
    assert!(err.not_started);
}

/// A CLI that never answers.
fn hanging_cli(dir: &Path) -> PathBuf {
    let script = dir.join("hang");
    std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[test]
fn hanging_cli_is_stopped_at_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let runner = CliRunner::new(CliKind::ClaudeCode, hanging_cli(dir.path()));
    let started = std::time::Instant::now();
    let err = runner
        .complete("sonnet", &text_body(), Some(std::time::Duration::from_millis(300)), &|| false)
        .unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(err.message.contains("stopped"), "{}", err.message);
}

#[test]
fn cancelling_stops_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let runner = CliRunner::new(CliKind::ClaudeCode, hanging_cli(dir.path()));
    let started = std::time::Instant::now();
    let err = runner.complete("sonnet", &text_body(), None, &|| started.elapsed().as_millis() > 200).unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(err.message.contains("cancelled"), "{}", err.message);
}

#[test]
fn timeout_still_applies_when_the_cli_never_reads_a_large_input() {
    let dir = tempfile::tempdir().unwrap();
    let runner = CliRunner::new(CliKind::ClaudeCode, hanging_cli(dir.path()));
    let big = "A".repeat(2 * 1024 * 1024);
    let body = chat_body("sys", serde_json::json!([
        {"type": "text", "text": "hi"},
        {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{big}")}},
    ]));
    let started = std::time::Instant::now();
    let err = runner.complete("sonnet", &body, Some(std::time::Duration::from_millis(300)), &|| false).unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "blocked for {:?}", started.elapsed());
    assert!(err.message.contains("stopped"), "{}", err.message);
}

fn exe(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn binary_on_path_wins_over_install_dirs() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    exe(&root.path().join("bin/claude"));
    exe(&home.join(".local/bin/claude"));
    let path = std::ffi::OsString::from(root.path().join("bin"));
    let found = course2md::cli_provider::find_binary("claude", &path, Some(&home)).unwrap();
    assert_eq!(found, root.path().join("bin/claude"));
}

#[test]
fn finder_launched_app_still_finds_cli_in_home_install_dirs() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    exe(&home.join("Library/pnpm/codex"));
    let found = course2md::cli_provider::find_binary("codex", std::ffi::OsStr::new("/usr/bin:/bin"), Some(&home)).unwrap();
    assert_eq!(found, home.join("Library/pnpm/codex"));
}

#[test]
fn non_executable_files_are_skipped() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("claude"), "not a program").unwrap();
    let path = std::ffi::OsString::from(root.path());
    assert!(course2md::cli_provider::find_binary("claude", &path, None).filter(|p| p.starts_with(root.path())).is_none());
}

fn cli_settings(concurrency: Option<usize>) -> course2md::llm::LlmSettings {
    let mut s = course2md::llm::LlmSettings { enabled: true, provider: LlmProvider::ClaudeCode, model: "sonnet".into(), ..Default::default() };
    if let Some(c) = concurrency {
        s.concurrency = c;
    }
    s
}

#[test]
fn cli_providers_default_to_two_parallel_calls_and_cap_at_four() {
    use course2md::llm::effective_concurrency;
    assert_eq!(effective_concurrency(&cli_settings(None)), 2);
    assert_eq!(effective_concurrency(&cli_settings(Some(3))), 3);
    assert_eq!(effective_concurrency(&cli_settings(Some(16))), 4);
    let http = course2md::llm::LlmSettings { concurrency: 12, ..Default::default() };
    assert_eq!(effective_concurrency(&http), 12);
}

#[test]
fn cli_provider_config_needs_a_model_but_no_address() {
    let s = cli_settings(None);
    assert!(course2md::llm::validate(&s).is_ok());
    let no_model = course2md::llm::LlmSettings { model: String::new(), ..s };
    assert!(course2md::llm::validate(&no_model).is_err());
}

#[test]
fn cli_provider_is_saved_by_its_kebab_case_name() {
    let s = cli_settings(None);
    let text = toml::to_string(&s).unwrap();
    assert!(text.contains("provider = \"claude-code\""), "{text}");
    let back: course2md::llm::LlmSettings = toml::from_str(&text).unwrap();
    assert_eq!(back.provider, LlmProvider::ClaudeCode);
}
