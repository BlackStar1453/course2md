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
    let text = runner.complete("sonnet", &body, None, None, &|| false).unwrap();

    assert_eq!(text, r#"{"segments":[{"id":"0","text":"fixed"}]}"#);
    let args = argv(dir.path());
    for expected in ["-p", "--verbose", "--strict-mcp-config", "--input-format", "--output-format", "stream-json", "--max-turns", "1", "--tools", "--model", "sonnet", "--system-prompt", "You proofread.", "--setting-sources", "project", "--no-session-persistence"] {
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
    let err = runner.complete("sonnet", &text_body(), None, None, &|| false).unwrap_err();
    assert!(err.message.contains("Credit balance is too low"), "{}", err.message);
    assert!(!err.not_started);
}

#[test]
fn nonzero_exit_without_output_is_an_error_with_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let runner = CliRunner::new(CliKind::ClaudeCode, fake_cli(dir.path(), "", 3));
    let err = runner.complete("sonnet", &text_body(), None, None, &|| false).unwrap_err();
    assert!(err.message.contains("exit 3"), "{}", err.message);
}

#[test]
fn missing_binary_means_the_request_was_never_sent() {
    let runner = CliRunner::new(CliKind::ClaudeCode, "/nonexistent/claude");
    let err = runner.complete("sonnet", &text_body(), None, None, &|| false).unwrap_err();
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
        .complete("sonnet", &text_body(), None, Some(std::time::Duration::from_millis(300)), &|| false)
        .unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(err.message.contains("stopped"), "{}", err.message);
}

#[test]
fn cancelling_stops_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let runner = CliRunner::new(CliKind::ClaudeCode, hanging_cli(dir.path()));
    let started = std::time::Instant::now();
    let err = runner.complete("sonnet", &text_body(), None, None, &|| started.elapsed().as_millis() > 200).unwrap_err();
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
    let err = runner.complete("sonnet", &body, None, Some(std::time::Duration::from_millis(300)), &|| false).unwrap_err();
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

const CODEX_OK: &str = r#"{"type":"thread.started","thread_id":"t1"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"{\"segments\":[{\"id\":0,\"text\":\"fixed\"}]}"}}
{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":5}}
"#;

#[test]
fn codex_cli_returns_the_agent_message_and_reads_the_prompt_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let runner = CliRunner::new(CliKind::CodexCli, fake_cli(dir.path(), CODEX_OK, 0));
    let schema = serde_json::json!({"type": "object"});
    let body = chat_body("You proofread.", serde_json::json!([{"type": "text", "text": "[{\"id\":0,\"text\":\"fixd\"}]"}]));

    let text = runner.complete("gpt-5.5", &body, Some(&schema), None, &|| false).unwrap();

    assert_eq!(text, r#"{"segments":[{"id":0,"text":"fixed"}]}"#);
    let args = argv(dir.path());
    assert_eq!(args.first().map(String::as_str), Some("exec"));
    assert_eq!(args.last().map(String::as_str), Some("-"), "prompt must come from stdin: {args:?}");
    for expected in ["--json", "--ignore-user-config", "--skip-git-repo-check", "--ephemeral", "read-only", "gpt-5.5", "--output-schema"] {
        assert!(args.iter().any(|a| a == expected), "missing {expected} in {args:?}");
    }
    let stdin = std::fs::read_to_string(dir.path().join("stdin.txt")).unwrap();
    assert!(stdin.contains("You proofread.") && stdin.contains("fixd"), "{stdin}");
}

#[test]
fn codex_cli_failed_turn_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let out = r#"{"type":"thread.started","thread_id":"t1"}
{"type":"turn.failed","error":{"message":"You've hit your usage limit"}}
"#;
    let runner = CliRunner::new(CliKind::CodexCli, fake_cli(dir.path(), out, 1));
    let err = runner.complete("gpt-5.5", &text_body(), None, None, &|| false).unwrap_err();
    assert!(err.message.contains("usage limit"), "{}", err.message);
}

/// A fake Codex that also keeps a copy of every `--image=` file it was given.
fn fake_codex_keeping_images(dir: &Path) -> PathBuf {
    let out = dir.join("out.txt");
    std::fs::write(&out, CODEX_OK).unwrap();
    let script = dir.join("fake-codex");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{d}/argv.txt'\nn=0\nfor a in \"$@\"; do case \"$a\" in --image=*) cp \"${{a#--image=}}\" '{d}'/seen-$n; n=$((n+1));; esac; done\ncat > /dev/null\ncat '{out}'\n",
            d = dir.display(),
            out = out.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn screenshot_body(bytes: &[u8]) -> serde_json::Value {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    chat_body("sys", serde_json::json!([
        {"type": "text", "text": "[{\"id\":0,\"text\":\"x\"}]"},
        {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{b64}")}},
    ]))
}

#[test]
fn claude_code_receives_the_screenshot_as_an_image_block() {
    let dir = tempfile::tempdir().unwrap();
    let runner = CliRunner::new(CliKind::ClaudeCode, fake_cli(dir.path(), CLAUDE_OK, 0));
    runner.complete("sonnet", &screenshot_body(b"\xFF\xD8jpeg-bytes"), None, None, &|| false).unwrap();

    let stdin = std::fs::read_to_string(dir.path().join("stdin.txt")).unwrap();
    let msg: serde_json::Value = serde_json::from_str(stdin.lines().next().unwrap()).unwrap();
    let image = &msg["message"]["content"][1];
    assert_eq!(image["type"], "image");
    assert_eq!(image["source"]["media_type"], "image/jpeg");
    assert_eq!(image["source"]["data"], "/9hqcGVnLWJ5dGVz");
}

#[test]
fn codex_cli_gets_the_screenshot_as_a_file_that_is_removed_afterwards() {
    let dir = tempfile::tempdir().unwrap();
    let runner = CliRunner::new(CliKind::CodexCli, fake_codex_keeping_images(dir.path()));
    runner.complete("gpt-5.5", &screenshot_body(b"\xFF\xD8jpeg-bytes"), None, None, &|| false).unwrap();

    assert_eq!(std::fs::read(dir.path().join("seen-0")).unwrap(), b"\xFF\xD8jpeg-bytes");
    let args = argv(dir.path());
    let image_arg = args.iter().find(|a| a.starts_with("--image=")).unwrap();
    assert!(!Path::new(image_arg.trim_start_matches("--image=")).exists(), "temp screenshot left behind");
    assert_eq!(args.last().map(String::as_str), Some("-"));
}

#[test]
fn claude_code_uses_structured_output_when_a_schema_is_given() {
    let dir = tempfile::tempdir().unwrap();
    // `result` holds prose; the schema-validated answer is in `structured_output`.
    let out = r#"{"type":"result","subtype":"success","is_error":false,"result":"Here you go","structured_output":{"segments":[{"id":0,"text":"he said \"hello\""}]}}"#;
    let runner = CliRunner::new(CliKind::ClaudeCode, fake_cli(dir.path(), out, 0));
    let schema = serde_json::json!({"type": "object", "required": ["segments"]});

    let text = runner.complete("sonnet", &text_body(), Some(&schema), None, &|| false).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["segments"][0]["text"], "he said \"hello\"");
    let args = argv(dir.path());
    // StructuredOutput is a tool call on its own turn; one turn is not enough.
    let turns = args.iter().position(|a| a == "--max-turns").unwrap();
    assert_eq!(args[turns + 1], "3");
    let at = args.iter().position(|a| a == "--json-schema").expect("--json-schema passed");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&args[at + 1]).unwrap(), schema);
}

#[test]
fn setup_for_a_cli_provider_needs_no_address_or_key_and_picks_a_default_model() {
    let cfg = course2md::llm::setup_interactive(Default::default(), Some(LlmProvider::ClaudeCode), None, None, None, false).unwrap();
    assert_eq!(cfg.llm.provider, LlmProvider::ClaudeCode);
    assert_eq!(cfg.llm.model, "sonnet");
    assert!(cfg.llm.enabled && cfg.llm.base_url.is_empty() && cfg.llm.api_key.is_empty());

    let cfg = course2md::llm::setup_interactive(Default::default(), Some(LlmProvider::CodexCli), None, None, Some("gpt-5.4".into()), false).unwrap();
    assert_eq!(cfg.llm.model, "gpt-5.4");
}

#[test]
fn a_failure_without_result_text_still_says_what_went_wrong() {
    let dir = tempfile::tempdir().unwrap();
    let out = r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#;
    let runner = CliRunner::new(CliKind::ClaudeCode, fake_cli(dir.path(), out, 1));
    let err = runner.complete("sonnet", &text_body(), None, None, &|| false).unwrap_err();
    assert!(err.message.contains("error_max_turns"), "{}", err.message);
}

fn pid_alive(pid: i32) -> bool {
    std::process::Command::new("kill").args(["-0", &pid.to_string()]).status().is_ok_and(|s| s.success())
}

/// npm-installed `claude` / `codex` are wrappers that start the real worker as a child;
/// stopping only the wrapper would leave the worker running on the subscription.
#[test]
fn stopping_the_cli_also_stops_programs_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("grandchild.pid");
    let script = dir.path().join("wrapper");
    std::fs::write(&script, format!("#!/bin/sh\nsleep 30 &\necho $! > '{}'\nwait\n", pidfile.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let runner = CliRunner::new(CliKind::ClaudeCode, script);

    // Stop once the worker is known to be running (timeout and cancel share the same stop path).
    let started = || std::fs::read_to_string(&pidfile).is_ok_and(|p| !p.trim().is_empty());
    runner.complete("sonnet", &text_body(), None, Some(std::time::Duration::from_secs(20)), &started).unwrap_err();

    let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!pid_alive(pid), "grandchild {pid} still running");
}

/// A fake CLI that dumps its environment, then prints `stdout`.
fn env_dumping_cli(dir: &Path, stdout: &str) -> PathBuf {
    let out = dir.join("out.txt");
    std::fs::write(&out, stdout).unwrap();
    let script = dir.join("env-cli");
    std::fs::write(&script, format!("#!/bin/sh\nenv > '{d}/env.txt'\ncat > /dev/null\ncat '{out}'\n", d = dir.display(), out = out.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Variables that would move the call off the user's subscription (API keys, other endpoints).
const OFF_SUBSCRIPTION_VARS: [&str; 8] = [
    "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX", "OPENAI_API_KEY", "OPENAI_BASE_URL", "CODEX_API_KEY",
];

#[test]
fn cli_calls_do_not_inherit_variables_that_bypass_the_subscription() {
    for var in OFF_SUBSCRIPTION_VARS {
        // SAFETY: only these otherwise-unused variables are set; no test here reads them.
        unsafe { std::env::set_var(var, "set-by-test") };
    }
    for (kind, out) in [(CliKind::ClaudeCode, CLAUDE_OK), (CliKind::CodexCli, CODEX_OK)] {
        let dir = tempfile::tempdir().unwrap();
        CliRunner::new(kind, env_dumping_cli(dir.path(), out)).complete("m", &text_body(), None, None, &|| false).unwrap();
        let env = std::fs::read_to_string(dir.path().join("env.txt")).unwrap();
        for var in OFF_SUBSCRIPTION_VARS {
            assert!(!env.lines().any(|l| l.starts_with(&format!("{var}="))), "{kind:?} inherited {var}");
        }
    }
}

#[test]
fn codex_cli_recovers_from_a_transient_error_event() {
    let dir = tempfile::tempdir().unwrap();
    let out = r#"{"type":"thread.started","thread_id":"t1"}
{"type":"error","message":"Reconnecting... 1/5"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"{\"segments\":[]}"}}
{"type":"turn.completed","usage":{}}
"#;
    let runner = CliRunner::new(CliKind::CodexCli, fake_cli(dir.path(), out, 0));
    assert_eq!(runner.complete("gpt-5.5", &text_body(), None, None, &|| false).unwrap(), r#"{"segments":[]}"#);
}

/// Finder-launched apps get a minimal PATH; npm's `codex`/`claude` wrappers then run
/// `node`, which lives next to them or in a common install directory.
#[test]
fn the_cli_can_find_programs_installed_next_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("pnpm");
    std::fs::create_dir(&bin).unwrap();
    let out = dir.path().join("out.txt");
    std::fs::write(&out, CLAUDE_OK).unwrap();
    let script = bin.join("claude");
    std::fs::write(&script, format!("#!/bin/sh\necho \"$PATH\" > '{d}/path.txt'\ncat > /dev/null\ncat '{o}'\n", d = dir.path().display(), o = out.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    CliRunner::new(CliKind::ClaudeCode, &script).complete("m", &text_body(), None, None, &|| false).unwrap();

    let path = std::fs::read_to_string(dir.path().join("path.txt")).unwrap();
    let entries: Vec<&str> = path.trim().split(':').collect();
    assert!(entries.contains(&bin.to_str().unwrap()), "{path}");
    assert!(entries.contains(&"/opt/homebrew/bin"), "{path}");
}

#[test]
fn locating_without_the_override_only_searches_the_given_places() {
    let root = tempfile::tempdir().unwrap();
    exe(&root.path().join("bin/codex"));
    let path = std::ffi::OsString::from(root.path().join("bin"));
    let runner = CliRunner::locate_in(CliKind::CodexCli, &path, None).unwrap();
    assert_eq!(runner.binary(), root.path().join("bin/codex"));
}
