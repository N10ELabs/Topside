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
    let project_revision = created_project
        .get("result")
        .and_then(|v| v.get("revision"))
        .and_then(Value::as_str)
        .context("missing project revision in direct create_project result")?
        .to_string();

    let listed_projects = mcp.call(3, "list_projects", json!({ "limit": 20 }))?;
    let projects = listed_projects
        .get("result")
        .and_then(Value::as_array)
        .context("missing list_projects result array")?;
    assert!(
        projects.iter().any(|project| {
            project.get("id").and_then(Value::as_str) == Some(project_id.as_str())
        })
    );

    let updated_project = mcp.call(
        4,
        "update_project",
        json!({
            "id": project_id,
            "expected_revision": project_revision,
            "patch": { "title": "Codex Direct Project Renamed" }
        }),
    )?;
    let updated_title = updated_project
        .get("result")
        .and_then(|v| v.get("title"))
        .and_then(Value::as_str)
        .context("missing updated project title")?;
    assert_eq!(updated_title, "Codex Direct Project Renamed");

    let created_task = mcp.call(
        5,
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
        6,
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
        7,
        "update_task",
        json!({
            "id": created_task["result"]["id"],
            "expected_revision": task_revision,
            "patch": { "status": "done" }
        }),
    )?;
    assert!(update_ok.get("result").is_some());

    let created_task_second = mcp.call(
        8,
        "create_task",
        json!({
            "title": "Codex Active Task B",
            "project_id": project_id
        }),
    )?;
    let second_task_id = created_task_second
        .get("result")
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .context("missing second task id from create_task")?
        .to_string();

    let created_task_third = mcp.call(
        9,
        "create_task",
        json!({
            "title": "Codex Active Task C",
            "project_id": project_id
        }),
    )?;
    let third_task_id = created_task_third
        .get("result")
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .context("missing third task id from create_task")?
        .to_string();

    let reordered_workspace = mcp.call(
        10,
        "reorder_project_tasks",
        json!({
            "project_id": project_id,
            "ordered_active_task_ids": [third_task_id, second_task_id]
        }),
    )?;
    let reordered_active = reordered_workspace
        .get("result")
        .and_then(|v| v.get("active_tasks"))
        .and_then(Value::as_array)
        .context("missing active_tasks in reorder_project_tasks result")?;
    assert_eq!(
        reordered_active
            .first()
            .and_then(|task| task.get("id"))
            .and_then(Value::as_str),
        Some(third_task_id.as_str())
    );
    assert_eq!(
        reordered_active
            .get(1)
            .and_then(|task| task.get("id"))
            .and_then(Value::as_str),
        Some(second_task_id.as_str())
    );

    let workspace = mcp.call(
        11,
        "get_project_workspace",
        json!({ "project_id": project_id }),
    )?;
    let workspace_result = workspace
        .get("result")
        .context("missing get_project_workspace result")?;
    let workspace_project_title = workspace_result
        .get("project")
        .and_then(|v| v.get("title"))
        .and_then(Value::as_str)
        .context("missing workspace project title")?;
    assert_eq!(workspace_project_title, "Codex Direct Project Renamed");
    let done_tasks = workspace_result
        .get("done_tasks")
        .and_then(Value::as_array)
        .context("missing done_tasks in workspace result")?;
    assert_eq!(done_tasks.len(), 1);
    assert_eq!(
        done_tasks
            .first()
            .and_then(|task| task.get("id"))
            .and_then(Value::as_str),
        Some(task_id.as_str())
    );

    let search = mcp.call(12, "search_context", json!({ "query": "Codex" }))?;
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
    assert!(
        tools_list
            .iter()
            .any(|tool| tool["name"] == "list_projects")
    );
    assert!(
        tools_list
            .iter()
            .any(|tool| tool["name"] == "get_project_workspace")
    );

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

    let list_projects = mcp.call(
        5,
        "tools/call",
        json!({
            "name": "list_projects",
            "arguments": { "limit": 20 }
        }),
    )?;
    let listed_projects = list_projects
        .get("result")
        .and_then(Value::as_array)
        .context("missing tools/call list_projects result array")?;
    assert!(
        listed_projects.iter().any(|project| {
            project.get("id").and_then(Value::as_str) == Some(project_id.as_str())
        })
    );

    let read_note = mcp.call(
        6,
        "tools/call",
        json!({
            "name": "read_entity",
            "arguments": { "id_or_path": note_id }
        }),
    )?;
    assert!(read_note.get("result").is_some());

    let workspace = mcp.call(
        7,
        "tools/call",
        json!({
            "name": "get_project_workspace",
            "arguments": { "project_id": project_id }
        }),
    )?;
    let workspace_notes = workspace
        .get("result")
        .and_then(|v| v.get("notes"))
        .and_then(Value::as_array)
        .context("missing notes in tools/call get_project_workspace result")?;
    assert_eq!(workspace_notes.len(), 1);
    assert_eq!(
        workspace_notes
            .first()
            .and_then(|note| note.get("id"))
            .and_then(Value::as_str),
        Some(note_id.as_str())
    );

    let activity = mcp.call(
        8,
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
