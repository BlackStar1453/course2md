//! CLI Provider：用本机已登录的 `claude` / `codex` 程序调用模型（走订阅额度，无 API key）。
//!
//! 输入是 chat/completions 形状的请求体（与 HTTP Provider 共用构造逻辑），输出是模型最终
//! 文本；调用方把文本包成 chat/completions 响应，下游重试、校验、请求记录保持不变。
//! 进程超时或取消时结束子进程；启动失败标记为「请求未发出」，不会被当作结果不确定。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::llm::LlmProvider;

/// 单次调用默认超时（与 HTTP agent 的 300s 一致）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// 等待子进程期间检查取消/超时的间隔。
const POLL: Duration = Duration::from_millis(100);

/// 哪一个 CLI。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliKind {
    ClaudeCode,
    CodexCli,
}

impl CliKind {
    /// Provider 是否由 CLI 承载。
    pub fn of(provider: LlmProvider) -> Option<Self> {
        match provider {
            LlmProvider::ClaudeCode => Some(Self::ClaudeCode),
            LlmProvider::CodexCli => Some(Self::CodexCli),
            _ => None,
        }
    }

    /// 可执行文件名。
    pub fn program(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::CodexCli => "codex",
        }
    }

    /// 手动指定可执行文件路径的环境变量。
    pub fn env_override(self) -> &'static str {
        match self {
            Self::ClaudeCode => "COURSE2MD_CLAUDE_BIN",
            Self::CodexCli => "COURSE2MD_CODEX_BIN",
        }
    }
}

/// 在 PATH 与常见安装目录中找可执行文件（PATH 优先）。
pub fn find_binary(program: &str, path: &std::ffi::OsStr, home: Option<&std::path::Path>) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::split_paths(path).collect();
    if let Some(home) = home {
        for rel in [".local/bin", ".claude/local", "Library/pnpm", ".npm-global/bin", ".bun/bin", ".volta/bin", ".cargo/bin"] {
            dirs.push(home.join(rel));
        }
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs.into_iter().map(|d| d.join(program)).find(|p| is_executable(p))
}

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// CLI 调用失败。
/// - `not_started`：进程没起来，请求内容确定没有发出；
/// - `finished`：进程自己退出并报告了失败，结果是确定的（不是「不知道模型有没有处理」）。
///   超时、取消时由我们结束进程，两者都为 false。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub message: String,
    pub not_started: bool,
    pub finished: bool,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// 进程已退出、明确失败。
fn failed(message: impl Into<String>) -> CliError {
    CliError { message: message.into(), not_started: false, finished: true }
}

/// 进程被我们中止或状态未知：模型可能已处理，结果不确定。
fn interrupted(message: impl Into<String>) -> CliError {
    CliError { message: message.into(), not_started: false, finished: false }
}

/// 进程没有启动：请求确定没有发出。
fn not_started(message: impl Into<String>) -> CliError {
    CliError { message: message.into(), not_started: true, finished: false }
}

/// 一个已定位好的 CLI 程序。
#[derive(Debug, Clone)]
pub struct CliRunner {
    kind: CliKind,
    binary: PathBuf,
}

impl CliRunner {
    pub fn new(kind: CliKind, binary: impl Into<PathBuf>) -> Self {
        Self { kind, binary: binary.into() }
    }

    /// 找到本机的 CLI：环境变量指定 → PATH → 常见安装目录（从 Finder 启动时 PATH 很短）。
    pub fn locate(kind: CliKind) -> Result<Self, CliError> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let path = std::env::var_os("PATH").unwrap_or_default();
        let found = std::env::var_os(kind.env_override())
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| find_binary(kind.program(), &path, home.as_deref()));
        found.map(|binary| Self::new(kind, binary)).ok_or_else(|| not_started(format!(
                "找不到 {program}，请先安装并登录，或用 {var} 指定路径 / {program} not found; install and sign in first, or set {var}",
                program = kind.program(),
                var = kind.env_override(),
            )))
    }

    pub fn binary(&self) -> &std::path::Path {
        &self.binary
    }

    /// 发一次请求，返回模型最终文本。`output_schema` 约束输出结构（Codex 原生支持；
    /// Claude Code 靠提示词里的格式说明）。`cancelled` 在等待期间轮询，返回 true 即结束子进程。
    pub fn complete(
        &self,
        model: &str,
        body: &Value,
        output_schema: Option<&Value>,
        timeout: Option<Duration>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String, CliError> {
        let system = body["messages"][0]["content"].as_str().unwrap_or_default();
        let user = &body["messages"][1]["content"];
        match self.kind {
            CliKind::ClaudeCode => {
                let mut cmd = Command::new(&self.binary);
                cmd.args(claude_args(model, system, output_schema));
                let stdin = claude_stdin(user);
                let out = run(cmd, Some(stdin), timeout.unwrap_or(DEFAULT_TIMEOUT), cancelled)?;
                parse_claude(&out)
            }
            CliKind::CodexCli => {
                // 本次调用的临时文件（schema、截图）；函数返回即删除
                let scratch = tempfile::Builder::new()
                    .prefix("course2md-codex-")
                    .tempdir()
                    .map_err(|e| not_started(format!("无法创建临时目录 / Could not create a temp dir: {e}")))?;
                let (prompt, images) = codex_input(system, user, scratch.path())?;
                let schema = match output_schema {
                    Some(schema) => {
                        let path = scratch.path().join("schema.json");
                        std::fs::write(&path, schema.to_string()).map_err(|e| not_started(format!("无法写入输出结构 / Could not write the output schema: {e}")))?;
                        Some(path)
                    }
                    None => None,
                };
                let mut cmd = Command::new(&self.binary);
                cmd.args(codex_args(model, schema.as_deref(), &images))
                    .env_remove("OPENAI_API_KEY")
                    .env_remove("CODEX_API_KEY");
                let out = run(cmd, Some(prompt), timeout.unwrap_or(DEFAULT_TIMEOUT), cancelled)?;
                parse_codex(&out)
            }
        }
    }
}

fn claude_args(model: &str, system: &str, schema: Option<&Value>) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--verbose",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--max-turns",
        // StructuredOutput 本身占一轮（常见 2–3 轮）；无 schema 时一轮即答完。工具全关，多给的轮次无法做别的事
        if schema.is_some() { "3" } else { "1" },
        "--tools",
        "",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--disable-slash-commands",
        // 只读工作目录（空临时目录）的项目配置：不触发用户全局 hooks / 插件
        "--setting-sources",
        "project",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if !model.trim().is_empty() {
        args.extend(["--model".into(), model.trim().into()]);
    }
    if !system.is_empty() {
        args.extend(["--system-prompt".into(), system.into()]);
    }
    if let Some(schema) = schema {
        // 内置 StructuredOutput：单轮、无工具时同样可用，杜绝未转义引号等无效 JSON
        args.extend(["--json-schema".into(), schema.to_string()]);
    }
    args
}

/// chat/completions 用户内容（字符串或内容块数组）→ Claude stream-json 输入的一行。
fn claude_stdin(user: &Value) -> String {
    let content: Vec<Value> = match user {
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part["type"].as_str() {
                Some("image_url") => claude_image(part["image_url"]["url"].as_str().unwrap_or_default()),
                _ => serde_json::json!({"type": "text", "text": part["text"].as_str().unwrap_or_default()}),
            })
            .collect(),
        Value::String(text) => vec![serde_json::json!({"type": "text", "text": text})],
        other => vec![serde_json::json!({"type": "text", "text": other.to_string()})],
    };
    let line = serde_json::json!({"type": "user", "message": {"role": "user", "content": content}});
    format!("{line}\n")
}

/// `data:image/jpeg;base64,XXX` → Claude base64 图片块。
fn claude_image(data_url: &str) -> Value {
    let (meta, data) = data_url.split_once(',').unwrap_or(("", data_url));
    let media_type = meta
        .strip_prefix("data:")
        .and_then(|m| m.split(';').next())
        .filter(|m| !m.is_empty())
        .unwrap_or("image/jpeg");
    serde_json::json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
}

fn codex_args(model: &str, schema: Option<&std::path::Path>, images: &[PathBuf]) -> Vec<String> {
    let mut args: Vec<String> = [
        "exec",
        "--json",
        // 不加载用户 config.toml 里的插件 / MCP（登录态仍读 CODEX_HOME），冷启动快一倍多
        "--ignore-user-config",
        "--skip-git-repo-check",
        "--ephemeral",
        "--sandbox",
        "read-only",
        "-c",
        "model_reasoning_effort=\"low\"",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if !model.trim().is_empty() {
        args.extend(["--model".into(), model.trim().into()]);
    }
    if let Some(schema) = schema {
        args.extend(["--output-schema".into(), schema.display().to_string()]);
    }
    // `--image=路径` 形式：`-i` 可接多个值，会把末尾代表 stdin 的 `-` 当成图片
    args.extend(images.iter().map(|p| format!("--image={}", p.display())));
    args.push("-".into());
    args
}

/// chat/completions 请求 → Codex 提示文本（系统指令在前）+ 截图临时文件。
fn codex_input(system: &str, user: &Value, dir: &std::path::Path) -> Result<(String, Vec<PathBuf>), CliError> {
    use base64::Engine as _;
    let mut prompt = String::new();
    if !system.is_empty() {
        prompt.push_str(system);
        prompt.push_str("\n\n");
    }
    let mut images = Vec::new();
    let parts: Vec<Value> = match user {
        Value::Array(parts) => parts.clone(),
        Value::String(text) => vec![serde_json::json!({"type": "text", "text": text})],
        other => vec![serde_json::json!({"type": "text", "text": other.to_string()})],
    };
    for part in &parts {
        if part["type"] == "image_url" {
            let url = part["image_url"]["url"].as_str().unwrap_or_default();
            let (meta, data) = url.split_once(',').unwrap_or(("", url));
            let ext = if meta.contains("png") { "png" } else { "jpg" };
            let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| not_started(format!("截图数据无效 / Invalid screenshot data: {e}")))?;
            let path = dir.join(format!("image-{}.{ext}", images.len()));
            std::fs::write(&path, bytes).map_err(|e| not_started(format!("无法写入截图 / Could not write the screenshot: {e}")))?;
            images.push(path);
        } else if let Some(text) = part["text"].as_str() {
            prompt.push_str(text);
            prompt.push('\n');
        }
    }
    Ok((prompt, images))
}

/// Codex `exec --json` 事件流 → 最终文本（最后一条 agent_message）。
fn parse_codex(out: &Output) -> Result<String, CliError> {
    let mut last: Option<String> = None;
    for line in out.stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
        match event["type"].as_str() {
            Some("item.completed") if event["item"]["type"] == "agent_message" => {
                last = event["item"]["text"].as_str().map(str::to_owned);
            }
            Some("turn.failed") | Some("error") => {
                let message = event["error"]["message"].as_str().or_else(|| event["message"].as_str()).unwrap_or_default();
                return Err(failed(format!("Codex 返回错误 / Codex returned an error: {}", excerpt(message))));
            }
            _ => {}
        }
    }
    match last.filter(|t| !t.trim().is_empty()) {
        Some(text) => Ok(text),
        None => Err(failed(format!("Codex 没有返回内容 / Codex returned no content{}", out.describe_exit()))),
    }
}

/// Claude stream-json 输出 → 最终文本。
fn parse_claude(out: &Output) -> Result<String, CliError> {
    let mut fallback = String::new();
    for line in out.stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
        match event["type"].as_str() {
            Some("assistant") => {
                if let Some(parts) = event["message"]["content"].as_array() {
                    for part in parts {
                        if let Some(text) = part["text"].as_str() {
                            fallback.push_str(text);
                        }
                    }
                }
            }
            Some("result") => {
                if event["is_error"].as_bool() != Some(true)
                    && let Some(structured) = event.get("structured_output").filter(|v| !v.is_null())
                {
                    return Ok(structured.to_string());
                }
                let text = event["result"].as_str().unwrap_or_default();
                if event["is_error"].as_bool() == Some(true) || event["subtype"].as_str() != Some("success") {
                    let subtype = event["subtype"].as_str().unwrap_or("error");
                    return Err(failed(format!(
                        "Claude Code 返回错误 / Claude Code returned an error ({subtype}): {}",
                        excerpt(text)
                    )));
                }
                if !text.trim().is_empty() {
                    return Ok(text.to_string());
                }
            }
            _ => {}
        }
    }
    if !fallback.trim().is_empty() && out.success {
        return Ok(fallback);
    }
    Err(failed(format!(
        "Claude Code 没有返回内容 / Claude Code returned no content{}",
        out.describe_exit()
    )))
}

/// 子进程输出。
struct Output {
    stdout: String,
    stderr: String,
    success: bool,
    code: Option<i32>,
}

impl Output {
    fn describe_exit(&self) -> String {
        let code = self.code.map_or_else(|| "signal".to_string(), |c| c.to_string());
        let tail = excerpt(self.stderr.trim());
        if tail.is_empty() {
            format!(" (exit {code})")
        } else {
            format!(" (exit {code}): {tail}")
        }
    }
}

/// 截断到 300 字符，避免把整段输出塞进错误消息。
fn excerpt(text: &str) -> String {
    const MAX: usize = 300;
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(MAX).collect::<String>())
    }
}

/// CLI 的工作目录：放在 $HOME 之外，避免加载用户的 CLAUDE.md / 项目配置。
fn work_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("course2md-cli");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn run(
    mut cmd: Command,
    stdin: Option<String>,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<Output, CliError> {
    cmd.current_dir(work_dir())
        // 订阅额度：不让 API key 抢走计费；嵌套在 Claude Code 里运行时不继承会话标记
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("CLAUDECODE")
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| not_started(format!("无法启动 CLI / Could not start the CLI: {e}")))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // 独立线程写入：大截图超过管道缓冲时，CLI 不读就会阻塞，不能挡住超时/取消检查。
        // 写完即关闭 stdin，CLI 才知道输入结束；子进程被结束后写入自然报错退出。
        std::thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                kill(&mut child);
                return Err(interrupted(format!("等待 CLI 失败 / Failed waiting for the CLI: {e}")));
            }
        }
        if cancelled() {
            kill(&mut child);
            return Err(interrupted("任务已取消，已结束 CLI / Task cancelled; CLI stopped"));
        }
        if started.elapsed() >= timeout {
            kill(&mut child);
            return Err(interrupted(format!(
                "CLI 超过 {} 秒未完成，已结束 / CLI did not finish within {} s; stopped",
                timeout.as_secs(),
                timeout.as_secs()
            )));
        }
        std::thread::sleep(POLL);
    };
    Ok(Output {
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
        success: status.success(),
        code: status.code(),
    })
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(pipe) = pipe {
            let mut reader = BufReader::new(pipe);
            let mut line = String::new();
            while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                text.push_str(&line);
                line.clear();
            }
        }
        text
    })
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
