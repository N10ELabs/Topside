use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::net::UnixStream as StdUnixStream;

use anyhow::{Context, Result};
use serde_json::{Value, json};

struct McpHarness {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpHarness {
    fn start(workspace: &Path) -> Result<Self> {
        Self::start_with_autostart(workspace, false)
    }

    fn start_with_autostart(workspace: &Path, autostart: bool) -> Result<Self> {
        let bin = env!("CARGO_BIN_EXE_n10e");
        let mut command = Command::new(bin);
        command
            .arg("--workspace")
            .arg(workspace)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !autostart {
            command.env("N10E_MCP_SKIP_AUTOSTART", "1");
        }

        let mut child = command
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

#[cfg(unix)]
struct DaemonHarness {
    child: Child,
}

#[cfg(unix)]
impl DaemonHarness {
    fn start(workspace: &Path) -> Result<Self> {
        let bin = env!("CARGO_BIN_EXE_n10e");
        let mut child = Command::new(bin)
            .arg("--workspace")
            .arg(workspace)
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed spawning n10e daemon subprocess")?;

        let socket_path = n10e::mcp::daemon_socket_path(workspace);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last_err = None;

        while Instant::now() < deadline {
            match StdUnixStream::connect(&socket_path) {
                Ok(stream) => {
                    drop(stream);
                    return Ok(Self { child });
                }
                Err(err) => {
                    last_err = Some(err);
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }

        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!(
            "daemon socket did not become ready at {}: {}",
            socket_path.display(),
            last_err
                .map(|err| err.to_string())
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
}

#[cfg(unix)]
impl Drop for DaemonHarness {
    fn drop(&mut self) {
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

fn tool_structured_content<'a>(response: &'a Value, method: &str) -> Result<&'a Value> {
    response
        .get("result")
        .and_then(|v| v.get("structuredContent"))
        .with_context(|| format!("missing structuredContent in tools/call response for {method}"))
}

#[cfg(unix)]
fn stop_daemon_from_pid_file(workspace: &Path) -> Result<()> {
    let pid_path = n10e::mcp::daemon_pid_path(workspace);
    let raw = std::fs::read_to_string(&pid_path)
        .with_context(|| format!("failed reading daemon pid file {}", pid_path.display()))?;
    let pid = raw
        .trim()
        .parse::<u32>()
        .with_context(|| format!("invalid daemon pid in {}", pid_path.display()))?;

    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .context("failed invoking kill for daemon process")?;
    if !status.success() {
        anyhow::bail!("kill -9 failed for daemon pid {pid}");
    }

    let _ = std::fs::remove_file(n10e::mcp::daemon_socket_path(workspace));
    let _ = std::fs::remove_file(pid_path);
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

    let _ = mcp.call_framed(1, "initialize", json!({}))?;

    let tools = mcp.call_framed(2, "tools/list", json!({}))?;
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

    let list_projects_tool = tools_list
        .iter()
        .find(|tool| tool["name"] == "list_projects")
        .context("list_projects tool missing from tools/list")?;
    assert_eq!(
        list_projects_tool["inputSchema"]["additionalProperties"],
        Value::Bool(false)
    );
    assert_eq!(
        list_projects_tool["inputSchema"]["properties"]["limit"]["type"],
        Value::String("integer".to_string())
    );

    let update_project_tool = tools_list
        .iter()
        .find(|tool| tool["name"] == "update_project")
        .context("update_project tool missing from tools/list")?;
    let update_project_required = update_project_tool["inputSchema"]["required"]
        .as_array()
        .context("update_project inputSchema missing required list")?;
    assert!(update_project_required.iter().any(|item| item == "id"));
    assert!(
        update_project_required
            .iter()
            .any(|item| item == "expected_revision")
    );
    assert!(
        update_project_tool["inputSchema"]["properties"]["patch"]["properties"]["source_kind"]
            .get("enum")
            .is_some()
    );

    let archive_tool = tools_list
        .iter()
        .find(|tool| tool["name"] == "archive_entity")
        .context("archive_entity tool missing from tools/list")?;
    assert!(
        archive_tool["inputSchema"]["anyOf"]
            .as_array()
            .map(|rules| !rules.is_empty())
            .unwrap_or(false)
    );

    let create_project = mcp.call_framed(
        3,
        "tools/call",
        json!({
            "name": "create_project",
            "arguments": {
                "title": "Claude Tools Project"
            }
        }),
    )?;
    let create_project_result = tool_structured_content(&create_project, "create_project")?;
    let project_id = create_project_result
        .get("id")
        .and_then(Value::as_str)
        .context("missing project id from tools/call create_project")?
        .to_string();

    let create_note = mcp.call_framed(
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

    let create_note_result = tool_structured_content(&create_note, "create_note")?;
    let note_id = create_note_result
        .get("id")
        .and_then(Value::as_str)
        .context("missing note id from create_note")?
        .to_string();

    let list_projects = mcp.call_framed(
        5,
        "tools/call",
        json!({
            "name": "list_projects",
            "arguments": { "limit": 20 }
        }),
    )?;
    let listed_projects = tool_structured_content(&list_projects, "list_projects")?
        .as_array()
        .context("missing tools/call list_projects result array")?;
    assert!(
        listed_projects.iter().any(|project| {
            project.get("id").and_then(Value::as_str) == Some(project_id.as_str())
        })
    );

    let read_note = mcp.call_framed(
        6,
        "tools/call",
        json!({
            "name": "read_entity",
            "arguments": { "id_or_path": note_id }
        }),
    )?;
    assert!(tool_structured_content(&read_note, "read_entity")?.is_object());

    let workspace = mcp.call_framed(
        7,
        "tools/call",
        json!({
            "name": "get_project_workspace",
            "arguments": { "project_id": project_id }
        }),
    )?;
    let workspace_notes = tool_structured_content(&workspace, "get_project_workspace")?
        .get("notes")
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

    let activity = mcp.call_framed(
        8,
        "tools/call",
        json!({
            "name": "list_recent_activity",
            "arguments": { "limit": 20 }
        }),
    )?;
    let events = tool_structured_content(&activity, "list_recent_activity")?
        .as_array()
        .context("missing activity result array")?;
    assert!(!events.is_empty());

    Ok(())
}

#[cfg(unix)]
#[test]
fn warm_daemon_serves_mcp_over_unix_socket() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    ensure_workspace_initialized(temp.path())?;

    let _daemon = DaemonHarness::start(temp.path())?;
    let socket_path = n10e::mcp::daemon_socket_path(temp.path());
    let mut stream = StdUnixStream::connect(&socket_path).with_context(|| {
        format!(
            "failed connecting to daemon socket {}",
            socket_path.display()
        )
    })?;

    writeln!(
        stream,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        })
    )?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let response: Value = serde_json::from_str(line.trim())
        .context("invalid JSON response from warm daemon initialize")?;

    assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(response["result"]["serverInfo"]["name"], "n10e");

    Ok(())
}

#[cfg(unix)]
#[test]
fn mcp_auto_starts_daemon_when_socket_is_missing() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    ensure_workspace_initialized(temp.path())?;

    let socket_path = n10e::mcp::daemon_socket_path(temp.path());
    let pid_path = n10e::mcp::daemon_pid_path(temp.path());
    assert!(!socket_path.exists());
    assert!(!pid_path.exists());

    let mut mcp = McpHarness::start_with_autostart(temp.path(), true)?;
    let init = mcp.call(1, "initialize", json!({}))?;
    assert_eq!(init["result"]["serverInfo"]["name"], "n10e");
    assert!(socket_path.exists());
    assert!(pid_path.exists());

    drop(mcp);
    stop_daemon_from_pid_file(temp.path())?;

    Ok(())
}

#[test]
fn batch_task_tools_reduce_round_trips() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    ensure_workspace_initialized(temp.path())?;

    let mut mcp = McpHarness::start(temp.path())?;

    let _ = mcp.call_framed(1, "initialize", json!({}))?;

    let tools = mcp.call_framed(2, "tools/list", json!({}))?;
    let tools_list = tools
        .get("result")
        .and_then(|v| v.get("tools"))
        .and_then(Value::as_array)
        .context("tools/list missing tools for batch test")?;

    for tool_name in [
        "bulk_create_tasks",
        "bulk_update_tasks",
        "bulk_archive_entities",
    ] {
        assert!(
            tools_list.iter().any(|tool| tool["name"] == tool_name),
            "missing {tool_name} in tools/list"
        );
    }

    let bulk_create_tool = tools_list
        .iter()
        .find(|tool| tool["name"] == "bulk_create_tasks")
        .context("bulk_create_tasks tool missing from tools/list")?;
    assert_eq!(
        bulk_create_tool["inputSchema"]["properties"]["items"]["type"],
        Value::String("array".to_string())
    );

    let project = mcp.call(3, "create_project", json!({ "title": "Batch MCP Project" }))?;
    let project_id = project
        .get("result")
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .context("missing project id in batch test")?
        .to_string();

    let created = mcp.call_framed(
        4,
        "tools/call",
        json!({
            "name": "bulk_create_tasks",
            "arguments": {
                "items": [
                    {
                        "title": "Batch Task A",
                        "project_id": project_id
                    },
                    {
                        "title": "Batch Task B",
                        "project_id": project_id,
                        "assignee": "agent:codex"
                    }
                ]
            }
        }),
    )?;
    let created_items = tool_structured_content(&created, "bulk_create_tasks")?
        .as_array()
        .context("bulk_create_tasks did not return an array")?;
    assert_eq!(created_items.len(), 2);

    let first_task_id = created_items[0]
        .get("id")
        .and_then(Value::as_str)
        .context("missing first task id from bulk_create_tasks")?
        .to_string();
    let first_task_revision = created_items[0]
        .get("revision")
        .and_then(Value::as_str)
        .context("missing first task revision from bulk_create_tasks")?
        .to_string();
    let second_task_id = created_items[1]
        .get("id")
        .and_then(Value::as_str)
        .context("missing second task id from bulk_create_tasks")?
        .to_string();
    let second_task_revision = created_items[1]
        .get("revision")
        .and_then(Value::as_str)
        .context("missing second task revision from bulk_create_tasks")?
        .to_string();

    let updated = mcp.call(
        5,
        "bulk_update_tasks",
        json!({
            "items": [
                {
                    "id": first_task_id,
                    "expected_revision": first_task_revision,
                    "patch": { "status": "done" }
                },
                {
                    "id": second_task_id,
                    "expected_revision": second_task_revision,
                    "patch": { "status": "in_progress" }
                }
            ]
        }),
    )?;
    let updated_items = updated
        .get("result")
        .and_then(Value::as_array)
        .context("bulk_update_tasks did not return an array")?;
    assert_eq!(updated_items.len(), 2);
    let archived_revision = updated_items[1]
        .get("revision")
        .and_then(Value::as_str)
        .context("missing archived candidate revision after bulk_update_tasks")?;

    let archived = mcp.call_framed(
        6,
        "tools/call",
        json!({
            "name": "bulk_archive_entities",
            "arguments": {
                "items": [
                    {
                        "entity_id": second_task_id,
                        "expected_revision": archived_revision
                    }
                ]
            }
        }),
    )?;
    let archived_items = tool_structured_content(&archived, "bulk_archive_entities")?
        .as_array()
        .context("bulk_archive_entities did not return an array")?;
    assert_eq!(archived_items.len(), 1);
    assert_eq!(
        archived_items[0].get("archived").and_then(Value::as_bool),
        Some(true)
    );

    let workspace = mcp.call(
        7,
        "get_project_workspace",
        json!({ "project_id": project_id }),
    )?;
    let workspace_result = workspace
        .get("result")
        .context("missing workspace result after batch tool calls")?;
    let active_tasks = workspace_result
        .get("active_tasks")
        .and_then(Value::as_array)
        .context("missing active_tasks in batch test workspace")?;
    let done_tasks = workspace_result
        .get("done_tasks")
        .and_then(Value::as_array)
        .context("missing done_tasks in batch test workspace")?;

    assert!(active_tasks.is_empty());
    assert_eq!(done_tasks.len(), 1);
    assert_eq!(
        done_tasks[0].get("title").and_then(Value::as_str),
        Some("Batch Task A")
    );

    Ok(())
}

#[test]
#[ignore = "profiling harness; run with -- --ignored --nocapture"]
fn batch_task_tool_profile_single_vs_batch() -> Result<()> {
    const TASK_COUNT: usize = 20;

    let temp = tempfile::TempDir::new()?;
    ensure_workspace_initialized(temp.path())?;

    let mut mcp = McpHarness::start(temp.path())?;
    let _ = mcp.call(1, "initialize", json!({}))?;

    let project = mcp.call(
        2,
        "create_project",
        json!({ "title": "Batch Profile Project" }),
    )?;
    let project_id = project
        .get("result")
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .context("missing project id in batch profile test")?
        .to_string();

    let single_create_started = Instant::now();
    let mut single_tasks = Vec::with_capacity(TASK_COUNT);
    for index in 0..TASK_COUNT {
        let created = mcp.call(
            10 + index as i64,
            "create_task",
            json!({
                "title": format!("Single Task {}", index + 1),
                "project_id": project_id
            }),
        )?;
        single_tasks.push(
            created
                .get("result")
                .cloned()
                .context("missing create_task result in profile harness")?,
        );
    }
    let single_create_elapsed = single_create_started.elapsed();

    let batch_create_items = (0..TASK_COUNT)
        .map(|index| {
            json!({
                "title": format!("Batch Task {}", index + 1),
                "project_id": project_id
            })
        })
        .collect::<Vec<_>>();
    let batch_create_started = Instant::now();
    let batch_created = mcp.call(
        100,
        "bulk_create_tasks",
        json!({
            "items": batch_create_items
        }),
    )?;
    let batch_create_elapsed = batch_create_started.elapsed();
    let batch_tasks = batch_created
        .get("result")
        .and_then(Value::as_array)
        .cloned()
        .context("missing bulk_create_tasks result array in profile harness")?;
    assert_eq!(batch_tasks.len(), TASK_COUNT);

    let single_update_started = Instant::now();
    let mut single_archives = Vec::with_capacity(TASK_COUNT);
    for (index, task) in single_tasks.iter().enumerate() {
        let task_id = task
            .get("id")
            .and_then(Value::as_str)
            .context("missing single task id in profile harness")?;
        let task_revision = task
            .get("revision")
            .and_then(Value::as_str)
            .context("missing single task revision in profile harness")?;
        let updated = mcp.call(
            200 + index as i64,
            "update_task",
            json!({
                "id": task_id,
                "expected_revision": task_revision,
                "patch": { "status": "done" }
            }),
        )?;
        let updated_result = updated
            .get("result")
            .context("missing update_task result in profile harness")?;
        let archive_id = updated_result
            .get("id")
            .and_then(Value::as_str)
            .context("missing updated single task id in profile harness")?
            .to_string();
        let archive_revision = updated_result
            .get("revision")
            .and_then(Value::as_str)
            .context("missing updated single task revision in profile harness")?
            .to_string();
        single_archives.push((archive_id, archive_revision));
    }
    let single_update_elapsed = single_update_started.elapsed();

    let batch_update_items = batch_tasks
        .iter()
        .map(|task| {
            json!({
                "id": task.get("id").and_then(Value::as_str),
                "expected_revision": task.get("revision").and_then(Value::as_str),
                "patch": { "status": "done" }
            })
        })
        .collect::<Vec<_>>();
    let batch_update_started = Instant::now();
    let batch_updated = mcp.call(
        300,
        "bulk_update_tasks",
        json!({
            "items": batch_update_items
        }),
    )?;
    let batch_update_elapsed = batch_update_started.elapsed();
    let batch_updated_tasks = batch_updated
        .get("result")
        .and_then(Value::as_array)
        .cloned()
        .context("missing bulk_update_tasks result array in profile harness")?;
    assert_eq!(batch_updated_tasks.len(), TASK_COUNT);

    let single_archive_started = Instant::now();
    for (index, (task_id, task_revision)) in single_archives.iter().enumerate() {
        let archived = mcp.call(
            400 + index as i64,
            "archive_entity",
            json!({
                "id": task_id,
                "expected_revision": task_revision
            }),
        )?;
        assert_eq!(
            archived
                .get("result")
                .and_then(|v| v.get("archived"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }
    let single_archive_elapsed = single_archive_started.elapsed();

    let batch_archive_items = batch_updated_tasks
        .iter()
        .map(|task| {
            json!({
                "id": task.get("id").and_then(Value::as_str),
                "expected_revision": task.get("revision").and_then(Value::as_str)
            })
        })
        .collect::<Vec<_>>();
    let batch_archive_started = Instant::now();
    let batch_archived = mcp.call(
        500,
        "bulk_archive_entities",
        json!({
            "items": batch_archive_items
        }),
    )?;
    let batch_archive_elapsed = batch_archive_started.elapsed();
    let batch_archived_items = batch_archived
        .get("result")
        .and_then(Value::as_array)
        .context("missing bulk_archive_entities result array in profile harness")?;
    assert_eq!(batch_archived_items.len(), TASK_COUNT);

    println!(
        "mcp_profile::task_count={TASK_COUNT} single_create_ms={:.3} batch_create_ms={:.3} single_update_ms={:.3} batch_update_ms={:.3} single_archive_ms={:.3} batch_archive_ms={:.3}",
        single_create_elapsed.as_secs_f64() * 1_000.0,
        batch_create_elapsed.as_secs_f64() * 1_000.0,
        single_update_elapsed.as_secs_f64() * 1_000.0,
        batch_update_elapsed.as_secs_f64() * 1_000.0,
        single_archive_elapsed.as_secs_f64() * 1_000.0,
        batch_archive_elapsed.as_secs_f64() * 1_000.0,
    );

    Ok(())
}
