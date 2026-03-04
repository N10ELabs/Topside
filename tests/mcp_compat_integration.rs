use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

struct McpHarness {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpHarness {
    fn start(workspace: &Path) -> Result<Self> {
        let bin = env!("CARGO_BIN_EXE_topside");
        let mut child = Command::new(bin)
            .arg("--workspace")
            .arg(workspace)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed spawning topside mcp subprocess")?;

        let stdin = child.stdin.take().context("missing child stdin")?;
        let stdout = child.stdout.take().context("missing child stdout")?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    fn call(&mut self, id: i64, method: &str, params: Value) -> Result<Value> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        writeln!(self.stdin, "{}", req)?;
        self.stdin.flush()?;

        let mut line = String::new();
        self.stdout.read_line(&mut line)?;
        if line.trim().is_empty() {
            anyhow::bail!("received empty response line for method {method}");
        }

        let response: Value = serde_json::from_str(line.trim())
            .with_context(|| format!("invalid JSON response for method {method}: {line}"))?;
        Ok(response)
    }

    fn call_framed(&mut self, id: i64, method: &str, params: Value) -> Result<Value> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let body = req.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
        self.stdin.flush()?;

        let mut content_length = None;
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line)?;
            if line.is_empty() {
                anyhow::bail!("received EOF while reading framed response for method {method}");
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }

            if let Some((name, value)) = trimmed.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    content_length = Some(value.trim().parse::<usize>().with_context(|| {
                        format!("invalid Content-Length header for method {method}")
                    })?);
                }
            }
        }

        let content_length = content_length
            .with_context(|| format!("missing Content-Length header for method {method}"))?;
        let mut body = vec![0u8; content_length];
        self.stdout.read_exact(&mut body)?;

        let response: Value = serde_json::from_slice(&body)
            .with_context(|| format!("invalid framed JSON response for method {method}"))?;
        Ok(response)
    }
}

impl Drop for McpHarness {
    fn drop(&mut self) {
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ensure_workspace_initialized(workspace: &Path) -> Result<()> {
    let bin = env!("CARGO_BIN_EXE_topside");
    let status = Command::new(bin)
        .arg("init")
        .arg(workspace)
        .status()
        .context("failed to run topside init")?;
    if !status.success() {
        anyhow::bail!("topside init failed with status {status}");
    }
    Ok(())
}

#[test]
fn mcp_initialize_and_discovery_are_protocol_only() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    ensure_workspace_initialized(temp.path())?;

    let mut mcp = McpHarness::start(temp.path())?;

    let init = mcp.call(1, "initialize", json!({}))?;
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "Topside");

    let tools = mcp.call(2, "tools/list", json!({}))?;
    let tool_list = tools["result"]["tools"]
        .as_array()
        .context("missing tools array")?;
    assert!(tool_list.is_empty());

    let resources = mcp.call(3, "resources/list", json!({}))?;
    let resource_list = resources["result"]["resources"]
        .as_array()
        .context("missing resources array")?;
    assert!(resource_list.is_empty());

    let templates = mcp.call(4, "resources/templates/list", json!({}))?;
    let template_list = templates["result"]["resourceTemplates"]
        .as_array()
        .context("missing resourceTemplates array")?;
    assert!(template_list.is_empty());

    Ok(())
}

#[test]
fn mcp_rejects_tool_calls_and_direct_methods() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    ensure_workspace_initialized(temp.path())?;

    let mut mcp = McpHarness::start(temp.path())?;
    let _ = mcp.call(1, "initialize", json!({}))?;

    let tool_call = mcp.call_framed(
        2,
        "tools/call",
        json!({
            "name": "list_projects",
            "arguments": {}
        }),
    )?;
    assert_eq!(tool_call["error"]["code"], -32601);
    assert_eq!(tool_call["error"]["message"], "unknown tool name");
    assert_eq!(tool_call["error"]["data"]["tool"], "list_projects");

    let direct_method = mcp.call(3, "list_projects", json!({ "limit": 20 }))?;
    assert_eq!(direct_method["error"]["code"], -32601);
    assert_eq!(direct_method["error"]["message"], "method not found");
    assert_eq!(direct_method["error"]["data"]["method"], "list_projects");

    Ok(())
}

#[test]
fn mcp_framed_initialize_and_ping_work() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    ensure_workspace_initialized(temp.path())?;

    let mut mcp = McpHarness::start(temp.path())?;

    let init = mcp.call_framed(1, "initialize", json!({}))?;
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");

    let ping = mcp.call_framed(2, "ping", json!({}))?;
    assert_eq!(ping["result"], json!({}));

    Ok(())
}

#[test]
fn init_migrates_legacy_workspace_identity() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    fs::create_dir_all(temp.path().join(".n10e"))?;
    fs::write(
        temp.path().join("n10e.toml"),
        r#"codename = "n10e-01"
workspace_root = "."

[dirs]
projects = "projects"
tasks = "tasks"
notes = "notes"
agents = "agents"
archive = "archive"

[server]
host = "127.0.0.1"
port = 8123

[index]
debounce_ms = 350
startup_full_scan = true

[search]
default_limit = 20
bm25_k1 = 1.2
bm25_b = 0.75
"#,
    )?;

    ensure_workspace_initialized(temp.path())?;

    assert!(temp.path().join("topside.toml").exists());
    assert!(!temp.path().join("n10e.toml").exists());
    assert!(temp.path().join(".topside").exists());
    assert!(!temp.path().join(".n10e").exists());

    let config = fs::read_to_string(temp.path().join("topside.toml"))?;
    assert!(config.contains("codename = \"n10e-01\""));
    assert!(config.contains("port = 8123"));

    let bin = env!("CARGO_BIN_EXE_topside");
    let status = Command::new(bin)
        .arg("--workspace")
        .arg(temp.path())
        .arg("doctor")
        .status()
        .context("failed to run topside doctor")?;
    if !status.success() {
        anyhow::bail!("topside doctor failed with status {status}");
    }

    Ok(())
}
