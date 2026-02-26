use std::io::{BufRead, BufReader, Write};
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
        let bin = env!("CARGO_BIN_EXE_n10e");
        let mut child = Command::new(bin)
            .arg("--workspace")
            .arg(workspace)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed spawning n10e mcp subprocess")?;

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
}

impl Drop for McpHarness {
    fn drop(&mut self) {
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ensure_workspace_initialized(workspace: &Path) -> Result<()> {
    let bin = env!("CARGO_BIN_EXE_n10e");
    let status = Command::new(bin)
        .arg("init")
        .arg(workspace)
        .status()
        .context("failed to run n10e init")?;
    if !status.success() {
        anyhow::bail!("n10e init failed with status {status}");
    }
    Ok(())
}

#[test]
fn codex_style_direct_method_profile() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    ensure_workspace_initialized(temp.path())?;

    let mut mcp = McpHarness::start(temp.path())?;

    let init = mcp.call(1, "initialize", json!({}))?;
    assert!(init.get("result").is_some());

    let created_project = mcp.call(
        2,
        "create_project",
        json!({ "title": "Codex Direct Project", "body": "direct profile" }),
    )?;
    let project_id = created_project
        .get("result")
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .context("missing project id in direct create_project result")?
        .to_string();

    let created_task = mcp.call(
        3,
        "create_task",
        json!({
            "title": "Codex Direct Task",
            "project_id": project_id,
            "assignee": "agent:codex"
        }),
    )?;

    let task_id = created_task
        .get("result")
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .context("missing task id from create_task")?
        .to_string();
    let task_revision = created_task
        .get("result")
        .and_then(|v| v.get("revision"))
        .and_then(Value::as_str)
        .context("missing task revision from create_task")?
        .to_string();

    let conflict = mcp.call(
        4,
        "update_task",
        json!({
            "id": task_id,
            "expected_revision": "stale-revision",
            "patch": { "status": "done" }
        }),
    )?;

    let code = conflict
        .get("error")
        .and_then(|v| v.get("code"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    assert_eq!(code, -32010);

    let update_ok = mcp.call(
        5,
        "update_task",
        json!({
            "id": created_task["result"]["id"],
            "expected_revision": task_revision,
            "patch": { "status": "done" }
        }),
    )?;
    assert!(update_ok.get("result").is_some());

    let search = mcp.call(6, "search_context", json!({ "query": "Codex" }))?;
    let result_len = search
        .get("result")
        .and_then(Value::as_array)
        .map(|v| v.len())
        .unwrap_or(0);
    assert!(result_len >= 1);

    Ok(())
}

#[test]
fn claude_style_tools_call_profile() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    ensure_workspace_initialized(temp.path())?;

    let mut mcp = McpHarness::start(temp.path())?;

    let _ = mcp.call(1, "initialize", json!({}))?;

    let tools = mcp.call(2, "tools/list", json!({}))?;
    let tools_list = tools
        .get("result")
        .and_then(|v| v.get("tools"))
        .and_then(Value::as_array)
        .context("tools/list missing tools")?;
    assert!(tools_list.iter().any(|tool| tool["name"] == "create_note"));

    let create_project = mcp.call(
        3,
        "tools/call",
        json!({
            "name": "create_project",
            "arguments": {
                "title": "Claude Tools Project"
            }
        }),
    )?;
    let project_id = create_project
        .get("result")
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .context("missing project id from tools/call create_project")?
        .to_string();

    let create_note = mcp.call(
        4,
        "tools/call",
        json!({
            "name": "create_note",
            "arguments": {
                "title": "Claude Tools Note",
                "project_id": project_id,
                "body": "tools call body"
            }
        }),
    )?;

    let note_id = create_note
        .get("result")
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .context("missing note id from create_note")?
        .to_string();

    let read_note = mcp.call(
        5,
        "tools/call",
        json!({
            "name": "read_entity",
            "arguments": { "id_or_path": note_id }
        }),
    )?;
    assert!(read_note.get("result").is_some());

    let activity = mcp.call(
        6,
        "tools/call",
        json!({
            "name": "list_recent_activity",
            "arguments": { "limit": 20 }
        }),
    )?;
    let events = activity
        .get("result")
        .and_then(Value::as_array)
        .context("missing activity result array")?;
    assert!(!events.is_empty());

    Ok(())
}
