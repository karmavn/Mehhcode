use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempEnv {
    root: PathBuf,
    cwd: PathBuf,
    home: PathBuf,
}

impl TempEnv {
    fn new(prefix: &str) -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "mehh_e2e_{prefix}_{}_{}",
            std::process::id(),
            seq
        ));
        let cwd = root.join("workspace");
        let home = root.join("home");
        fs::create_dir_all(&cwd).expect("create temp workspace");
        fs::create_dir_all(&home).expect("create temp home");
        Self { root, cwd, home }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mehh"));
        cmd.current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env_remove("MEHH_FAUX_SCRIPT")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("GEMINI_API_KEY")
            .env_remove("OPENROUTER_API_KEY");
        cmd
    }
}

impl Drop for TempEnv {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run_with_stdin(mut cmd: Command, stdin_payload: &str) -> std::process::Output {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn mehh binary");
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(stdin_payload.as_bytes())
            .expect("write stdin payload");
    }
    child.wait_with_output().expect("wait for mehh binary")
}

#[test]
fn e2e_cli_version_and_list_models_work_offline() {
    let env = TempEnv::new("version_models");

    let ver = env.cmd().arg("--version").output().expect("run --version");
    assert!(
        ver.status.success(),
        "--version failed: {}",
        String::from_utf8_lossy(&ver.stderr)
    );
    let ver_out = String::from_utf8_lossy(&ver.stdout);
    assert!(ver_out.contains("mehh"), "unexpected --version: {ver_out}");

    let models = env
        .cmd()
        .arg("--list-models")
        .output()
        .expect("run --list-models");
    assert!(
        models.status.success(),
        "--list-models failed: {}",
        String::from_utf8_lossy(&models.stderr)
    );
    let models_out = String::from_utf8_lossy(&models.stdout);
    assert!(
        models_out.lines().count() > 5,
        "expected built-in model catalog lines, got: {models_out}"
    );
}

#[test]
fn e2e_cli_print_mode_text_and_json_with_faux_provider() {
    let env = TempEnv::new("print_modes");

    let text_out = env
        .cmd()
        .args(["--provider", "faux", "--no-mcp", "--no-session", "-p", "ping"])
        .output()
        .expect("run text print mode");
    assert!(
        text_out.status.success(),
        "text mode failed: {}",
        String::from_utf8_lossy(&text_out.stderr)
    );
    let stdout = String::from_utf8_lossy(&text_out.stdout);
    assert!(
        stdout.contains("Hello from mehh Rust faux provider!"),
        "missing faux text output: {stdout}"
    );

    let json_out = env
        .cmd()
        .args([
            "--provider",
            "faux",
            "--no-mcp",
            "--no-session",
            "--mode",
            "json",
            "-p",
            "ping",
        ])
        .output()
        .expect("run json print mode");
    assert!(
        json_out.status.success(),
        "json mode failed: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    let json_stdout = String::from_utf8_lossy(&json_out.stdout);
    let mut seen_agent_end = false;
    let mut line_count = 0usize;
    for line in json_stdout.lines().filter(|l| !l.trim().is_empty()) {
        line_count += 1;
        let val: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid JSON event line '{line}': {e}"));
        let ev_type = val
            .get("type")
            .and_then(|v| v.as_str())
            .expect("JSON event must carry string 'type'");
        if ev_type == "agent_end" {
            seen_agent_end = true;
        }
    }
    assert!(line_count >= 2, "expected multiple JSON events, got {line_count}");
    assert!(seen_agent_end, "expected agent_end event in: {json_stdout}");
}

#[test]
fn e2e_cli_print_mode_combines_piped_stdin_with_prompt() {
    let env = TempEnv::new("piped_stdin");
    let mut cmd = env.cmd();
    cmd.args([
        "--provider",
        "faux",
        "--no-mcp",
        "--no-session",
        "--mode",
        "json",
        "-p",
        "summarize input",
    ])
    .env(
        "MEHH_FAUX_SCRIPT",
        r#"[{"text":"PIPED_AND_PROMPT_HANDLED"}]"#,
    );

    let out = run_with_stdin(cmd, "PIPED_DATA_LINE_42\n");
    assert!(
        out.status.success(),
        "piped stdin run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut saw_merged_user = false;
    let mut saw_assistant = false;
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let val: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid JSON line '{line}': {e}"));
        let msg = &val["message"];
        if msg["role"] == "user" {
            let serialized = serde_json::to_string(msg).expect("serialize user message");
            if serialized.contains("summarize input") && serialized.contains("PIPED_DATA_LINE_42") {
                saw_merged_user = true;
            }
        }
        if msg["role"] == "assistant" {
            let serialized = serde_json::to_string(msg).expect("serialize assistant message");
            if serialized.contains("PIPED_AND_PROMPT_HANDLED") {
                saw_assistant = true;
            }
        }
    }
    assert!(
        saw_merged_user,
        "expected emitted user message with both prompt and piped stdin, got: {stdout}"
    );
    assert!(
        saw_assistant,
        "expected faux assistant reply in JSON stream, got: {stdout}"
    );
}

#[test]
fn e2e_cli_rpc_mode_handles_jsonl_commands_over_stdio() {
    let env = TempEnv::new("rpc_stdio");
    let mut cmd = env.cmd();
    cmd.args(["--provider", "faux", "--no-mcp", "--no-session", "--mode", "rpc"])
        .env("MEHH_FAUX_SCRIPT", r#"[{"text":"RPC_REPLY_OK"}]"#);

    let script = concat!(
        "{\"id\":\"s1\",\"type\":\"get_state\"}\n",
        "{\"id\":\"t1\",\"type\":\"set_thinking_level\",\"level\":\"medium\"}\n",
        "{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"hello rpc\"}\n",
        "{\"id\":\"m1\",\"type\":\"get_messages\"}\n",
    );
    let out = run_with_stdin(cmd, script);
    assert!(
        out.status.success(),
        "rpc mode failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut responses: HashMap<String, serde_json::Value> = HashMap::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let val: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid RPC JSONL line '{line}': {e}"));
        if val.get("type").and_then(|v| v.as_str()) == Some("response") {
            if let Some(id) = val.get("id").and_then(|v| v.as_str()) {
                responses.insert(id.to_string(), val);
            }
        }
    }

    assert_eq!(responses["s1"]["success"], true);
    assert_eq!(responses["s1"]["command"], "get_state");
    assert!(responses["s1"]["data"].get("thinkingLevel").is_some());

    assert_eq!(responses["t1"]["success"], true);
    assert_eq!(responses["t1"]["command"], "set_thinking_level");

    assert_eq!(responses["p1"]["success"], true);
    assert_eq!(responses["p1"]["command"], "prompt");

    assert_eq!(responses["m1"]["success"], true);
    let messages = responses["m1"]["data"]["messages"]
        .as_array()
        .expect("get_messages must return array");
    assert!(
        messages.len() >= 2,
        "expected user + assistant messages, got {}",
        messages.len()
    );
    let serialized = serde_json::to_string(messages).expect("serialize messages");
    assert!(serialized.contains("hello rpc"));
    assert!(serialized.contains("RPC_REPLY_OK"));
}

#[test]
fn e2e_cli_session_persists_and_reloads_across_invocations() {
    let env = TempEnv::new("session_reload");
    let session_file = env.cwd.join("persisted.jsonl");
    let session_arg = session_file.to_string_lossy().into_owned();

    let first = env
        .cmd()
        .args([
            "--provider",
            "faux",
            "--no-mcp",
            "--session",
            &session_arg,
            "-p",
            "first turn question",
        ])
        .env("MEHH_FAUX_SCRIPT", r#"[{"text":"FIRST_TURN_REPLY"}]"#)
        .output()
        .expect("run first session turn");
    assert!(
        first.status.success(),
        "first turn failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(session_file.exists(), "session JSONL file must be created");

    let mut reload_cmd = env.cmd();
    reload_cmd.args([
        "--provider",
        "faux",
        "--no-mcp",
        "--session",
        &session_arg,
        "--mode",
        "rpc",
    ]);
    let out = run_with_stdin(reload_cmd, "{\"id\":\"g1\",\"type\":\"get_messages\"}\n");
    assert!(
        out.status.success(),
        "session reload failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let g1 = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("id").and_then(|x| x.as_str()) == Some("g1"))
        .expect("g1 response must exist");
    assert_eq!(g1["success"], true);
    let messages_json = serde_json::to_string(&g1["data"]["messages"]).expect("serialize messages");
    assert!(
        messages_json.contains("first turn question"),
        "reloaded session missing prompt: {messages_json}"
    );
    assert!(
        messages_json.contains("FIRST_TURN_REPLY"),
        "reloaded session missing reply: {messages_json}"
    );
}

#[test]
fn e2e_cli_permission_mode_blocks_write_in_chat_and_allows_in_auto() {
    let env = TempEnv::new("permissions");
    let target_file = env.cwd.join("e2e_output.txt");
    let script = r#"[
        {"tool_calls":[{"id":"w1","name":"write","arguments":{"path":"e2e_output.txt","content":"written_by_e2e"}}]},
        {"text":"finished"}
    ]"#;

    let chat_out = env
        .cmd()
        .args([
            "--provider",
            "faux",
            "--no-mcp",
            "--no-session",
            "--permission-mode",
            "chat",
            "-p",
            "write e2e_output.txt",
        ])
        .env("MEHH_FAUX_SCRIPT", script)
        .output()
        .expect("run in chat permission mode");
    assert!(
        chat_out.status.success(),
        "chat mode exited unexpectedly: {}",
        String::from_utf8_lossy(&chat_out.stderr)
    );
    assert!(
        !target_file.exists(),
        "chat permission mode must block write tool"
    );
    let chat_stdout = String::from_utf8_lossy(&chat_out.stdout);
    assert!(
        chat_stdout.contains("finished"),
        "chat mode should block turn 1 write and complete turn 2: {chat_stdout}"
    );

    let auto_out = env
        .cmd()
        .args([
            "--provider",
            "faux",
            "--no-mcp",
            "--no-session",
            "--permission-mode",
            "auto",
            "-p",
            "write e2e_output.txt",
        ])
        .env("MEHH_FAUX_SCRIPT", script)
        .output()
        .expect("run in auto permission mode");
    assert!(
        auto_out.status.success(),
        "auto mode failed: {}",
        String::from_utf8_lossy(&auto_out.stderr)
    );
    let content = fs::read_to_string(&target_file)
        .expect("auto permission mode must create e2e_output.txt");
    assert_eq!(content, "written_by_e2e");
    let stdout = String::from_utf8_lossy(&auto_out.stdout);
    assert!(
        stdout.contains("finished"),
        "expected final assistant turn after tool execution: {stdout}"
    );
}

#[test]
fn e2e_cli_invalid_faux_script_exits_nonzero() {
    let env = TempEnv::new("invalid_faux_script");
    let out = env
        .cmd()
        .args(["--provider", "faux", "--no-mcp", "--no-session", "-p", "hi"])
        .env("MEHH_FAUX_SCRIPT", "{invalid json")
        .output()
        .expect("run with invalid MEHH_FAUX_SCRIPT");
    assert!(
        !out.status.success(),
        "invalid MEHH_FAUX_SCRIPT must fail closed"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("MEHH_FAUX_SCRIPT"),
        "expected MEHH_FAUX_SCRIPT in stderr, got: {stderr}"
    );
}
