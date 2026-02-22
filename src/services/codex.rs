use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use std::thread;

/// Cached path to the codex binary.
/// Once resolved, reused for all subsequent calls.
static CODEX_PATH: OnceLock<Option<String>> = OnceLock::new();

/// Resolve the path to the codex binary.
/// First tries `which codex`, then falls back to `bash -lc "which codex"`
/// (for non-interactive SSH sessions where ~/.profile isn't loaded).
fn resolve_codex_path() -> Option<String> {
    // Try direct `which codex` first
    if let Ok(output) = Command::new("which").arg("codex").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }

    // Fallback: use login shell to resolve PATH
    if let Ok(output) = Command::new("bash").args(["-lc", "which codex"]).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }

    None
}

/// Get the cached codex binary path, resolving it on first call.
fn get_codex_path() -> Option<&'static str> {
    CODEX_PATH.get_or_init(resolve_codex_path).as_deref()
}

/// Debug logging helper (only active when COKACDIR_DEBUG=1)
fn debug_log(msg: &str) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    let enabled = ENABLED
        .get_or_init(|| std::env::var("COKACDIR_DEBUG").map(|v| v == "1").unwrap_or(false));
    if !*enabled {
        return;
    }
    if let Some(home) = dirs::home_dir() {
        let debug_dir = home.join(".cokacdir").join("debug");
        let _ = std::fs::create_dir_all(&debug_dir);
        let log_path = debug_dir.join("codex.log");
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
            let timestamp = chrono::Local::now().format("%H:%M:%S%.3f");
            let _ = writeln!(file, "[{}] {}", timestamp, msg);
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodexResponse {
    pub success: bool,
    pub response: Option<String>,
    pub session_id: Option<String>,
    pub error: Option<String>,
}

/// Streaming message types for real-time Codex responses
#[derive(Debug, Clone)]
pub enum StreamMessage {
    /// Initialization - contains session_id
    Init { session_id: String },
    /// Text response chunk
    Text { content: String },
    /// Tool use started
    ToolUse { name: String, input: String },
    /// Tool execution result
    ToolResult { content: String, is_error: bool },
    /// Background task notification
    TaskNotification {
        task_id: String,
        status: String,
        summary: String,
    },
    /// Completion
    Done {
        result: String,
        session_id: Option<String>,
    },
    /// Error
    Error { message: String },
}

/// Token for cooperative cancellation of streaming requests.
/// Holds a flag and the child process PID so the caller can kill it externally.
pub struct CancelToken {
    pub cancelled: std::sync::atomic::AtomicBool,
    pub child_pid: std::sync::Mutex<Option<u32>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            child_pid: std::sync::Mutex::new(None),
        }
    }
}

/// Cached regex pattern for session ID validation
fn session_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[a-zA-Z0-9_-]+$").expect("Invalid session ID regex"))
}

/// Validate session ID format (alphanumeric, dashes, underscores only)
/// Max length reduced to 64 characters for security.
fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty() && session_id.len() <= 64 && session_id_regex().is_match(session_id)
}

/// Default allowed tools (kept for compatibility with existing UI/bot settings).
pub const DEFAULT_ALLOWED_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "Task",
    "TaskOutput",
    "TaskStop",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "Skill",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
];

#[derive(Default)]
struct EventState {
    thread_id: Option<String>,
    accumulated_text: String,
    done_sent: bool,
    last_error: Option<String>,
    started_command_items: HashSet<String>,
}

fn default_system_prompt() -> &'static str {
    r#"You are a terminal file manager assistant. Be concise. Focus on file operations. Respond in the same language as the user.

SECURITY RULES (MUST FOLLOW):
- NEVER execute destructive commands like rm -rf, format, mkfs, dd, etc.
- NEVER modify system files in /etc, /sys, /proc, /boot
- NEVER access or modify files outside the current working directory without explicit user path
- NEVER execute commands that could harm the system or compromise security
- ONLY suggest safe file operations: copy, move, rename, create directory, view, edit
- If a request seems dangerous, explain the risk and suggest a safer alternative

BASH EXECUTION RULES (MUST FOLLOW):
- All commands MUST run non-interactively without user input
- Use -y, --yes, or --non-interactive flags (e.g., apt install -y, npm init -y)
- Use -m flag for commit messages (e.g., git commit -m \"message\")
- Disable pagers with --no-pager or pipe to cat (e.g., git --no-pager log)
- NEVER use commands that open editors (vim, nano, etc.)
- NEVER use commands that wait for stdin without arguments
- NEVER use interactive flags like -i

IMPORTANT: Format your responses using Markdown for better readability:
- Use **bold** for important terms or commands
- Use `code` for file paths, commands, and technical terms
- Use bullet lists (- item) for multiple items
- Use numbered lists (1. item) for sequential steps
- Use code blocks (```language) for multi-line code or command examples
- Use headers (## Title) to organize longer responses
- Keep formatting minimal and terminal-friendly"#
}

fn append_text(accumulated: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !accumulated.is_empty() {
        accumulated.push('\n');
    }
    accumulated.push_str(text);
}

fn compose_prompt(
    user_prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(sp) = system_prompt {
        let trimmed = sp.trim();
        if !trimmed.is_empty() {
            sections.push(format!("SYSTEM INSTRUCTIONS:\n{}", trimmed));
        }
    }

    if let Some(tools) = allowed_tools {
        if !tools.is_empty() {
            sections.push(format!(
                "TOOL POLICY: The user currently allows only these tools: {}. If a task requires any other tool, explain which tool is missing and do not proceed until it is re-enabled.",
                tools.join(", ")
            ));
        }
    }

    sections.push(format!("USER REQUEST:\n{}", user_prompt));
    sections.join("\n\n")
}

fn is_git_repo(working_dir: &str) -> bool {
    match Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(working_dir)
        .output()
    {
        Ok(output) => {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        }
        Err(_) => false,
    }
}

fn build_exec_args(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    include_json: bool,
    sandbox: Option<&str>,
    full_auto: bool,
) -> Vec<String> {
    let mut args = vec!["exec".to_string()];

    if let Some(sid) = session_id {
        args.push("resume".to_string());
        args.push(sid.to_string());
    }

    if include_json {
        args.push("--json".to_string());
    }

    if full_auto {
        args.push("--full-auto".to_string());
    }

    if let Some(mode) = sandbox {
        args.push("--sandbox".to_string());
        args.push(mode.to_string());
    }

    // Auto-add skip flag when the working directory is not a git repo.
    if !is_git_repo(working_dir) {
        args.push("--skip-git-repo-check".to_string());
    }

    args.push(prompt.to_string());
    args
}

fn send_stream_message(sender: Option<&Sender<StreamMessage>>, message: StreamMessage) {
    if let Some(tx) = sender {
        let _ = tx.send(message);
    }
}

fn extract_error_message(value: &Value) -> Option<String> {
    if let Some(s) = value.get("message").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }

    if let Some(err) = value.get("error") {
        if let Some(s) = err.as_str() {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
        if let Some(s) = err.get("message").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }

    None
}

fn extract_agent_text(item: &Value) -> Option<String> {
    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }

    if let Some(text) = item
        .get("message")
        .and_then(|m| m.get("text"))
        .and_then(|v| v.as_str())
    {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }

    let mut chunks = Vec::new();
    if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
        for chunk in content {
            if let Some(text) = chunk.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    chunks.push(text.to_string());
                }
            }
        }
    }

    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join("\n"))
    }
}

fn extract_command(item: &Value) -> Option<String> {
    if let Some(command) = item.get("command").and_then(|v| v.as_str()) {
        if !command.is_empty() {
            return Some(command.to_string());
        }
    }

    if let Some(command) = item
        .get("input")
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
    {
        if !command.is_empty() {
            return Some(command.to_string());
        }
    }

    None
}

fn extract_command_result(item: &Value) -> Option<(String, bool)> {
    let aggregated_output = item
        .get("aggregated_output")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stdout = item.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
    let stderr = item.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
    let exit_code = item.get("exit_code").and_then(|v| v.as_i64());
    let item_error = item
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut parts: Vec<String> = Vec::new();

    if !aggregated_output.is_empty() {
        parts.push(aggregated_output.to_string());
    } else {
        if !stdout.is_empty() {
            parts.push(format!("stdout:\n{}", stdout));
        }
        if !stderr.is_empty() {
            parts.push(format!("stderr:\n{}", stderr));
        }
    }

    if let Some(code) = exit_code {
        parts.push(format!("exit_code: {}", code));
    }

    if parts.is_empty() {
        return None;
    }

    let is_error = item_error || exit_code.map(|code| code != 0).unwrap_or(false);
    Some((parts.join("\n\n"), is_error))
}

fn extract_file_path(item: &Value) -> Option<String> {
    const KEYS: &[&str] = &[
        "file_path",
        "path",
        "target_path",
        "destination_path",
        "new_path",
        "old_path",
    ];

    for key in KEYS {
        if let Some(path) = item.get(*key).and_then(|v| v.as_str()) {
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }

    if let Some(changes) = item.get("changes").and_then(|v| v.as_array()) {
        for change in changes {
            for key in KEYS {
                if let Some(path) = change.get(*key).and_then(|v| v.as_str()) {
                    if !path.is_empty() {
                        return Some(path.to_string());
                    }
                }
            }
        }
    }

    None
}

fn is_file_change_type(item_type: &str) -> bool {
    let t = item_type.to_ascii_lowercase();
    t == "file_change"
        || (t.contains("file")
            && (t.contains("change")
                || t.contains("edit")
                || t.contains("write")
                || t.contains("create")
                || t.contains("delete")))
}

fn handle_item_event(
    event_type: &str,
    item: &Value,
    state: &mut EventState,
    sender: Option<&Sender<StreamMessage>>,
) {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

    if item_type == "agent_message" && event_type == "item.completed" {
        if let Some(text) = extract_agent_text(item) {
            append_text(&mut state.accumulated_text, &text);
            send_stream_message(
                sender,
                StreamMessage::Text {
                    content: state.accumulated_text.clone(),
                },
            );
        }
        return;
    }

    if item_type == "command_execution" {
        let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");

        if event_type == "item.started" {
            if let Some(command) = extract_command(item) {
                send_stream_message(
                    sender,
                    StreamMessage::ToolUse {
                        name: "Bash".to_string(),
                        input: json!({ "command": command }).to_string(),
                    },
                );
            }
            if !item_id.is_empty() {
                state.started_command_items.insert(item_id.to_string());
            }
        }

        if event_type == "item.completed" {
            let already_started = !item_id.is_empty() && state.started_command_items.remove(item_id);
            if !already_started {
                if let Some(command) = extract_command(item) {
                    send_stream_message(
                        sender,
                        StreamMessage::ToolUse {
                            name: "Bash".to_string(),
                            input: json!({ "command": command }).to_string(),
                        },
                    );
                }
            }

            if let Some((content, is_error)) = extract_command_result(item) {
                send_stream_message(sender, StreamMessage::ToolResult { content, is_error });
            }
        }
        return;
    }

    if event_type == "item.completed" && is_file_change_type(item_type) {
        let name = if item_type.to_ascii_lowercase().contains("write")
            || item_type.to_ascii_lowercase().contains("create")
        {
            "Write"
        } else {
            "Edit"
        };

        let file_path = extract_file_path(item).unwrap_or_else(|| "(unknown file)".to_string());
        send_stream_message(
            sender,
            StreamMessage::ToolUse {
                name: name.to_string(),
                input: json!({ "file_path": file_path }).to_string(),
            },
        );
        return;
    }

    if item_type == "task" {
        let task_id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("task")
            .to_string();
        let summary = item
            .get("summary")
            .or_else(|| item.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let status = if event_type == "item.started" {
            "started"
        } else {
            "completed"
        }
        .to_string();
        send_stream_message(
            sender,
            StreamMessage::TaskNotification {
                task_id,
                status,
                summary,
            },
        );
    }
}

fn process_json_event(json: &Value, state: &mut EventState, sender: Option<&Sender<StreamMessage>>) {
    let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    if let Some(thread_id) = json.get("thread_id").and_then(|v| v.as_str()) {
        if state.thread_id.as_deref() != Some(thread_id) {
            state.thread_id = Some(thread_id.to_string());
        }
    }

    match event_type {
        "thread.started" => {
            if let Some(thread_id) = json.get("thread_id").and_then(|v| v.as_str()) {
                state.thread_id = Some(thread_id.to_string());
                send_stream_message(
                    sender,
                    StreamMessage::Init {
                        session_id: thread_id.to_string(),
                    },
                );
            }
        }
        "item.started" | "item.completed" => {
            if let Some(item) = json.get("item") {
                handle_item_event(event_type, item, state, sender);
            }
        }
        "plan.started" | "plan.updated" | "plan.completed" => {
            let summary = json
                .get("summary")
                .or_else(|| json.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("Plan update")
                .to_string();
            let status = event_type.strip_prefix("plan.").unwrap_or("updated").to_string();
            send_stream_message(
                sender,
                StreamMessage::TaskNotification {
                    task_id: "plan".to_string(),
                    status,
                    summary,
                },
            );
        }
        "turn.completed" => {
            state.done_sent = true;
            send_stream_message(
                sender,
                StreamMessage::Done {
                    result: state.accumulated_text.clone(),
                    session_id: state.thread_id.clone(),
                },
            );
        }
        "turn.failed" | "error" => {
            let message = extract_error_message(json)
                .unwrap_or_else(|| format!("Codex returned an error event: {}", event_type));
            state.last_error = Some(message.clone());
            send_stream_message(sender, StreamMessage::Error { message });
        }
        _ => {}
    }
}

fn parse_exec_output(stdout: &str) -> EventState {
    let mut state = EventState::default();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Ok(json) = serde_json::from_str::<Value>(trimmed) {
            process_json_event(&json, &mut state, None);
        } else {
            // Fallback: treat non-JSON line as plain assistant text.
            append_text(&mut state.accumulated_text, trimmed);
        }
    }

    state
}

/// Execute a command using Codex CLI.
/// Safe default for `--prompt`: read-only sandbox.
pub fn execute_command(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    allowed_tools: Option<&[String]>,
) -> CodexResponse {
    if let Some(sid) = session_id {
        if !is_valid_session_id(sid) {
            return CodexResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some("Invalid session ID format".to_string()),
            };
        }
    }

    let codex_bin = match get_codex_path() {
        Some(path) => path,
        None => {
            return CodexResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some("Codex CLI not found. Is Codex CLI installed?".to_string()),
            };
        }
    };

    let composed_prompt = compose_prompt(prompt, Some(default_system_prompt()), allowed_tools);
    let args = build_exec_args(
        &composed_prompt,
        session_id,
        working_dir,
        true,
        Some("read-only"),
        false,
    );

    let output = match Command::new(codex_bin)
        .args(&args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            return CodexResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some(format!(
                    "Failed to start Codex: {}. Is Codex CLI installed?",
                    e
                )),
            };
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    let state = parse_exec_output(&stdout);

    if let Some(error) = state.last_error {
        return CodexResponse {
            success: false,
            response: None,
            session_id: state.thread_id,
            error: Some(error),
        };
    }

    if !output.status.success() {
        return CodexResponse {
            success: false,
            response: None,
            session_id: state.thread_id,
            error: Some(if stderr.is_empty() {
                format!("Process exited with code {:?}", output.status.code())
            } else {
                stderr
            }),
        };
    }

    let response_text = if state.accumulated_text.is_empty() {
        stdout.trim().to_string()
    } else {
        state.accumulated_text
    };

    CodexResponse {
        success: true,
        response: Some(response_text),
        session_id: state.thread_id,
        error: None,
    }
}

/// Check if Codex CLI is available.
pub fn is_codex_available() -> bool {
    #[cfg(not(unix))]
    {
        false
    }

    #[cfg(unix)]
    {
        get_codex_path().is_some()
    }
}

/// Check if platform supports AI features.
pub fn is_ai_supported() -> bool {
    cfg!(unix)
}

/// Execute a command using Codex CLI with streaming JSONL output.
/// If `system_prompt` is None, uses the default file manager system prompt.
/// If `system_prompt` is Some(""), no system prompt is appended.
pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
) -> Result<(), String> {
    debug_log("========================================");
    debug_log("=== execute_command_streaming START ===");
    debug_log("========================================");
    debug_log(&format!("prompt_len: {} chars", prompt.len()));
    debug_log(&format!("session_id: {:?}", session_id));
    debug_log(&format!("working_dir: {}", working_dir));

    if let Some(sid) = session_id {
        if !is_valid_session_id(sid) {
            debug_log("ERROR: Invalid session ID format");
            return Err("Invalid session ID format".to_string());
        }
    }

    let codex_bin = get_codex_path().ok_or_else(|| {
        debug_log("ERROR: Codex CLI not found");
        "Codex CLI not found. Is Codex CLI installed?".to_string()
    })?;

    let effective_system_prompt = match system_prompt {
        None => Some(default_system_prompt()),
        Some("") => None,
        Some(custom) => Some(custom),
    };

    let composed_prompt = compose_prompt(prompt, effective_system_prompt, allowed_tools);
    let args = build_exec_args(
        &composed_prompt,
        session_id,
        working_dir,
        true,
        None,
        true,
    );

    debug_log("--- Spawning codex process ---");
    debug_log(&format!("Command: {}", codex_bin));
    debug_log(&format!("Args count: {}", args.len()));

    let mut child = Command::new(codex_bin)
        .args(&args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Codex: {}. Is Codex CLI installed?", e))?;

    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap() = Some(child.id());
    }

    let mut stderr_handle = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    debug_log(&format!("stderr: {}", line));
                }
            }
        })
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture stdout".to_string())?;
    let reader = BufReader::new(stdout);

    let mut state = EventState::default();

    for line_result in reader.lines() {
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                debug_log("Cancel detected - killing codex process");
                let _ = child.kill();
                let _ = child.wait();
                if let Some(handle) = stderr_handle.take() {
                    let _ = handle.join();
                }
                return Ok(());
            }
        }

        let line = match line_result {
            Ok(line) => line,
            Err(e) => {
                let _ = sender.send(StreamMessage::Error {
                    message: format!("Failed to read output: {}", e),
                });
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Ok(json) = serde_json::from_str::<Value>(trimmed) {
            process_json_event(&json, &mut state, Some(&sender));
        } else {
            append_text(&mut state.accumulated_text, trimmed);
            let _ = sender.send(StreamMessage::Text {
                content: state.accumulated_text.clone(),
            });
        }
    }

    if let Some(ref token) = cancel_token {
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            debug_log("Cancel detected after stdout loop - killing codex process");
            let _ = child.kill();
            let _ = child.wait();
            if let Some(handle) = stderr_handle.take() {
                let _ = handle.join();
            }
            return Ok(());
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("Process wait failed: {}", e))?;

    if let Some(handle) = stderr_handle.take() {
        let _ = handle.join();
    }

    if !state.done_sent {
        let _ = sender.send(StreamMessage::Done {
            result: state.accumulated_text.clone(),
            session_id: state.thread_id.clone(),
        });
    }

    if let Some(error) = state.last_error {
        return Err(error);
    }

    if !status.success() {
        return Err(format!("Codex process exited with code {:?}", status.code()));
    }

    debug_log("========================================");
    debug_log("=== execute_command_streaming END ===");
    debug_log("========================================");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_id_validation() {
        assert!(is_valid_session_id("thread_abc123"));
        assert!(is_valid_session_id("session-1"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("../invalid"));
    }

    #[test]
    fn test_extract_agent_text() {
        let item = serde_json::json!({"type": "agent_message", "text": "hello"});
        assert_eq!(extract_agent_text(&item), Some("hello".to_string()));
    }

    #[test]
    fn test_parse_thread_and_text() {
        let output = r#"{"type":"thread.started","thread_id":"thread_abc"}
{"type":"item.completed","item":{"type":"agent_message","text":"hello"}}
{"type":"turn.completed"}"#;
        let state = parse_exec_output(output);
        assert_eq!(state.thread_id, Some("thread_abc".to_string()));
        assert_eq!(state.accumulated_text, "hello".to_string());
    }
}
