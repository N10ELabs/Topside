use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use thiserror::Error;

#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::Mutex;

const LSOF_LISTEN_ARGS: &[&str] = &["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcLtn"];
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const TERM_POLL_ATTEMPTS: usize = 5;
const KILL_POLL_ATTEMPTS: usize = 2;
const DEV_PROCESS_HINTS: &[&str] = &[
    "topside",
    "node",
    "bun",
    "vite",
    "next",
    "nuxt",
    "astro",
    "react",
    "npm",
    "pnpm",
    "yarn",
    "deno",
    "python",
    "uvicorn",
    "gunicorn",
    "flask",
    "django",
    "manage.py",
    "ruby",
    "rails",
    "puma",
    "rackup",
    "php",
    "artisan",
    "symfony",
    "go",
    "air",
    "reflex",
    "cargo",
    "rustc",
    "trunk",
    "watchexec",
    "java",
    "gradle",
    "mvn",
    "spring",
    "postgres",
    "postmaster",
    "redis",
    "redis-server",
    "mysqld",
    "mongod",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSession {
    pub pid: u32,
    pub port: u16,
    pub process_name: String,
    pub command_line: String,
    pub user: String,
    pub bindings: Vec<String>,
    pub other_ports: Vec<u16>,
    pub is_topside_process: bool,
    pub can_terminate: bool,
    pub is_likely_dev: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminatePortResult {
    pub items: Vec<PortSession>,
    pub message: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PortManagerError {
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    CommandFailed(String),
}

pub trait PortManager: Send + Sync {
    fn list_sessions(&self) -> Result<Vec<PortSession>, PortManagerError>;
    fn terminate_session(
        &self,
        pid: u32,
        port: u16,
    ) -> Result<TerminatePortResult, PortManagerError>;
}

pub struct UnsupportedPortManager;

impl Default for UnsupportedPortManager {
    fn default() -> Self {
        Self
    }
}

impl PortManager for UnsupportedPortManager {
    fn list_sessions(&self) -> Result<Vec<PortSession>, PortManagerError> {
        Err(PortManagerError::Unsupported(
            "port viewer is only available on macOS".to_string(),
        ))
    }

    fn terminate_session(
        &self,
        _pid: u32,
        _port: u16,
    ) -> Result<TerminatePortResult, PortManagerError> {
        Err(PortManagerError::Unsupported(
            "port viewer is only available on macOS".to_string(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawListener {
    pid: u32,
    fallback_process_name: String,
    user: String,
    binding: String,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessDetails {
    command_line: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub(crate) trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[String]) -> Result<CommandResult, PortManagerError>;
    fn sleep(&self, duration: Duration);
}

pub struct SystemPortManager<R> {
    runner: R,
    topside_pid: u32,
    platform_supported: bool,
    current_user: Option<String>,
}

pub type DefaultPortManager = SystemPortManager<SystemCommandRunner>;

impl DefaultPortManager {
    pub fn new(topside_pid: u32) -> Self {
        Self {
            runner: SystemCommandRunner,
            topside_pid,
            platform_supported: cfg!(target_os = "macos"),
            current_user: std::env::var("USER").ok(),
        }
    }
}

impl<R> SystemPortManager<R> {
    #[cfg(test)]
    fn with_runner(runner: R, topside_pid: u32) -> Self {
        Self {
            runner,
            topside_pid,
            platform_supported: true,
            current_user: Some("tester".to_string()),
        }
    }
}

impl<R: CommandRunner> PortManager for SystemPortManager<R> {
    fn list_sessions(&self) -> Result<Vec<PortSession>, PortManagerError> {
        self.ensure_supported()?;
        self.list_sessions_impl()
    }

    fn terminate_session(
        &self,
        pid: u32,
        port: u16,
    ) -> Result<TerminatePortResult, PortManagerError> {
        self.ensure_supported()?;
        if pid == self.topside_pid {
            return Err(PortManagerError::Forbidden(
                "Topside cannot terminate its own process".to_string(),
            ));
        }

        let sessions = self.list_sessions_impl()?;
        if !sessions
            .iter()
            .any(|session| session.pid == pid && session.port == port)
        {
            return Ok(TerminatePortResult {
                items: sessions,
                message: format!("Port {port} is already closed"),
            });
        }

        self.send_signal("-TERM", pid)?;
        if let Some(items) = self.poll_until_gone(pid, port, TERM_POLL_ATTEMPTS)? {
            return Ok(TerminatePortResult {
                items,
                message: format!("Ended session on port {port}"),
            });
        }

        self.send_signal("-KILL", pid)?;
        if let Some(items) = self.poll_until_gone(pid, port, KILL_POLL_ATTEMPTS)? {
            return Ok(TerminatePortResult {
                items,
                message: format!("Force ended session on port {port}"),
            });
        }

        Err(PortManagerError::CommandFailed(format!(
            "port {port} is still active after termination attempts"
        )))
    }
}

#[allow(private_bounds)]
impl<R: CommandRunner> SystemPortManager<R> {
    fn ensure_supported(&self) -> Result<(), PortManagerError> {
        if self.platform_supported {
            Ok(())
        } else {
            Err(PortManagerError::Unsupported(
                "port viewer is only available on macOS".to_string(),
            ))
        }
    }

    fn list_sessions_impl(&self) -> Result<Vec<PortSession>, PortManagerError> {
        let raw = self.run_lsof()?;
        let details = self.enrich_process_details(&raw);
        Ok(group_port_sessions(
            raw,
            &details,
            self.topside_pid,
            self.current_user.as_deref(),
        ))
    }

    fn run_lsof(&self) -> Result<Vec<RawListener>, PortManagerError> {
        let args = LSOF_LISTEN_ARGS
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>();
        let output = self.runner.run("lsof", &args)?;
        if !output.success {
            let message = normalize_command_error(&output.stderr, "failed listing active ports");
            return Err(PortManagerError::CommandFailed(message));
        }

        Ok(parse_lsof_output(&output.stdout))
    }

    fn enrich_process_details(&self, listeners: &[RawListener]) -> HashMap<u32, ProcessDetails> {
        let pids = listeners
            .iter()
            .map(|listener| listener.pid)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if pids.is_empty() {
            return HashMap::new();
        }

        let pid_list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let args = vec![
            "-p".to_string(),
            pid_list,
            "-ww".to_string(),
            "-o".to_string(),
            "pid=".to_string(),
            "-o".to_string(),
            "comm=".to_string(),
            "-o".to_string(),
            "args=".to_string(),
        ];
        let output = match self.runner.run("ps", &args) {
            Ok(output) => output,
            Err(_) => return HashMap::new(),
        };
        if !output.success {
            return HashMap::new();
        }

        parse_ps_output(&output.stdout)
    }

    fn send_signal(&self, signal: &str, pid: u32) -> Result<(), PortManagerError> {
        let args = vec![signal.to_string(), pid.to_string()];
        let output = self.runner.run("kill", &args)?;
        if output.success {
            return Ok(());
        }

        let message = normalize_command_error(
            &output.stderr,
            &format!("failed sending {signal} to process {pid}"),
        );
        Err(PortManagerError::CommandFailed(message))
    }

    fn poll_until_gone(
        &self,
        pid: u32,
        port: u16,
        attempts: usize,
    ) -> Result<Option<Vec<PortSession>>, PortManagerError> {
        let mut last_items = Vec::new();
        for _ in 0..attempts {
            self.runner.sleep(POLL_INTERVAL);
            let items = self.list_sessions_impl()?;
            if !items
                .iter()
                .any(|item| item.pid == pid && item.port == port)
            {
                return Ok(Some(items));
            }
            last_items = items;
        }

        if last_items.is_empty() {
            return Ok(Some(last_items));
        }

        Ok(None)
    }
}

pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<CommandResult, PortManagerError> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|err| PortManagerError::CommandFailed(err.to_string()))?;
        Ok(CommandResult {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

fn parse_lsof_output(stdout: &str) -> Vec<RawListener> {
    let mut listeners = Vec::new();
    let mut current_pid = None;
    let mut current_process_name = String::new();
    let mut current_user = String::new();

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }

        let (prefix, value) = line.split_at(1);
        match prefix {
            "p" => {
                current_pid = value.trim().parse::<u32>().ok();
            }
            "c" => {
                current_process_name = value.trim().to_string();
            }
            "L" => {
                current_user = value.trim().to_string();
            }
            "f" => {}
            "n" => {
                let Some(pid) = current_pid else {
                    continue;
                };
                let binding = value.trim();
                let Some(port) = parse_port(binding) else {
                    continue;
                };
                listeners.push(RawListener {
                    pid,
                    fallback_process_name: current_process_name.clone(),
                    user: current_user.clone(),
                    binding: binding.to_string(),
                    port,
                });
            }
            _ => {}
        }
    }

    listeners
}

fn parse_port(binding: &str) -> Option<u16> {
    let (_, value) = binding.rsplit_once(':')?;
    value.parse::<u16>().ok()
}

fn parse_ps_output(stdout: &str) -> HashMap<u32, ProcessDetails> {
    let mut details = HashMap::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let Some(pid_part) = parts.next() else {
            continue;
        };
        let Ok(pid) = pid_part.parse::<u32>() else {
            continue;
        };
        let Some(comm_part) = parts.next() else {
            continue;
        };
        let remainder = parts.collect::<Vec<_>>().join(" ");
        let command_line = if remainder.is_empty() {
            comm_part.to_string()
        } else {
            remainder
        };

        details.insert(pid, ProcessDetails { command_line });
    }

    details
}

fn group_port_sessions(
    listeners: Vec<RawListener>,
    details: &HashMap<u32, ProcessDetails>,
    topside_pid: u32,
    current_user: Option<&str>,
) -> Vec<PortSession> {
    let mut by_process_port: BTreeMap<(u16, u32), Vec<RawListener>> = BTreeMap::new();
    for listener in listeners {
        by_process_port
            .entry((listener.port, listener.pid))
            .or_default()
            .push(listener);
    }

    let mut sessions = by_process_port
        .into_iter()
        .map(|((port, pid), group)| {
            let fallback_process_name = group
                .first()
                .map(|entry| entry.fallback_process_name.clone())
                .unwrap_or_default();
            let user = group
                .first()
                .map(|entry| entry.user.clone())
                .unwrap_or_default();
            let bindings = group
                .into_iter()
                .map(|entry| entry.binding)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let command_line = details
                .get(&pid)
                .map(|entry| entry.command_line.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| fallback_process_name.clone());
            PortSession {
                pid,
                port,
                process_name: fallback_process_name,
                command_line,
                user,
                bindings,
                other_ports: Vec::new(),
                is_topside_process: pid == topside_pid,
                can_terminate: pid != topside_pid,
                is_likely_dev: false,
            }
        })
        .collect::<Vec<_>>();

    let pid_ports = sessions
        .iter()
        .fold(HashMap::<u32, Vec<u16>>::new(), |mut acc, session| {
            acc.entry(session.pid).or_default().push(session.port);
            acc
        });

    for session in &mut sessions {
        let other_ports = pid_ports
            .get(&session.pid)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|port| *port != session.port)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        session.other_ports = other_ports;
        if session.process_name.trim().is_empty() {
            session.process_name = derive_process_name(&session.command_line);
        }
        session.is_likely_dev = classify_likely_dev(session, current_user);
    }

    sessions.sort_by_key(|session| (session.port, session.pid));
    sessions
}

fn derive_process_name(command_line: &str) -> String {
    let trimmed = command_line.trim();
    if trimmed.is_empty() {
        return "Unknown process".to_string();
    }

    let first_token = trimmed.split_whitespace().next().unwrap_or(trimmed);
    Path::new(first_token)
        .file_name()
        .and_then(|value| value.to_str())
        .map(ToString::to_string)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| trimmed.to_string())
}

fn normalize_command_error(stderr: &str, fallback: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn classify_likely_dev(session: &PortSession, current_user: Option<&str>) -> bool {
    if session.is_topside_process {
        return true;
    }

    let owned_by_current_user = current_user
        .map(|user| user.eq_ignore_ascii_case(session.user.trim()))
        .unwrap_or(true);
    if !owned_by_current_user {
        return false;
    }

    let process_name = session.process_name.to_lowercase();
    let command_line = session.command_line.to_lowercase();
    let has_dev_hint = DEV_PROCESS_HINTS
        .iter()
        .any(|hint| process_name.contains(hint) || command_line.contains(hint));
    if has_dev_hint {
        return true;
    }

    let loopback_only = session
        .bindings
        .iter()
        .all(|binding| is_loopback_binding(binding));
    let userland_command = command_line.starts_with("/users/")
        || command_line.starts_with("/opt/homebrew/")
        || command_line.starts_with("/usr/local/")
        || command_line.contains("/.cargo/")
        || command_line.contains("/.nvm/");
    loopback_only && userland_command
}

fn is_loopback_binding(binding: &str) -> bool {
    let host = binding
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(binding);
    matches!(host, "127.0.0.1" | "[::1]" | "localhost")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct ScriptedCall {
        program: String,
        args: Vec<String>,
        result: Result<CommandResult, PortManagerError>,
    }

    #[derive(Default)]
    struct ScriptedCommandRunner {
        calls: Mutex<VecDeque<ScriptedCall>>,
        sleeps: Mutex<Vec<Duration>>,
    }

    impl ScriptedCommandRunner {
        fn with_calls(calls: Vec<ScriptedCall>) -> Self {
            Self {
                calls: Mutex::new(calls.into_iter().collect()),
                sleeps: Mutex::new(Vec::new()),
            }
        }

        fn assert_drained(&self) {
            assert!(
                self.calls.lock().unwrap().is_empty(),
                "expected all scripted commands to be consumed"
            );
        }

        fn sleep_count(&self) -> usize {
            self.sleeps.lock().unwrap().len()
        }
    }

    impl CommandRunner for ScriptedCommandRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<CommandResult, PortManagerError> {
            let call = self
                .calls
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected command: {program} {:?}", args));
            assert_eq!(call.program, program);
            assert_eq!(call.args, args);
            call.result
        }

        fn sleep(&self, duration: Duration) {
            self.sleeps.lock().unwrap().push(duration);
        }
    }

    fn scripted_call(
        program: &str,
        args: &[&str],
        result: Result<CommandResult, PortManagerError>,
    ) -> ScriptedCall {
        ScriptedCall {
            program: program.to_string(),
            args: args.iter().map(|value| (*value).to_string()).collect(),
            result,
        }
    }

    fn ok(stdout: &str) -> Result<CommandResult, PortManagerError> {
        Ok(CommandResult {
            success: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    fn failed(stderr: &str) -> Result<CommandResult, PortManagerError> {
        Ok(CommandResult {
            success: false,
            stdout: String::new(),
            stderr: stderr.to_string(),
        })
    }

    #[test]
    fn groups_listeners_and_computes_other_ports() {
        let listeners = parse_lsof_output(
            "p625\ncControlCenter\nLanthonymarti\nf9\ntIPv4\nn*:7000\nf10\ntIPv6\nn*:7000\nf11\ntIPv4\nn*:5000\n\
             p826\ncpostgres\nLanthonymarti\nf7\ntIPv6\nn[::1]:5432\nf8\ntIPv4\nn127.0.0.1:5432\n",
        );
        let details = parse_ps_output(
            "625 /System/Library/ControlCenter /System/Library/ControlCenter --service\n\
             826 /opt/homebrew/bin/postgres /opt/homebrew/bin/postgres -D /tmp/db\n",
        );

        let sessions = group_port_sessions(listeners, &details, 10158, Some("anthonymarti"));

        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].port, 5000);
        assert_eq!(sessions[0].bindings, vec!["*:5000".to_string()]);
        assert_eq!(sessions[0].other_ports, vec![7000]);
        assert!(!sessions[0].is_likely_dev);
        assert_eq!(sessions[1].port, 5432);
        assert_eq!(
            sessions[1].bindings,
            vec!["127.0.0.1:5432".to_string(), "[::1]:5432".to_string()]
        );
        assert!(sessions[1].is_likely_dev);
        assert_eq!(sessions[2].port, 7000);
        assert_eq!(sessions[2].other_ports, vec![5000]);
        assert!(!sessions[2].is_likely_dev);
    }

    #[test]
    fn preserves_fallback_process_name_when_ps_missing() {
        let listeners =
            parse_lsof_output("p10158\nctopside\nLanthonymarti\nf12\ntIPv4\nn127.0.0.1:7410\n");
        let details = HashMap::new();

        let sessions = group_port_sessions(listeners, &details, 10158, Some("anthonymarti"));

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].process_name, "topside");
        assert_eq!(sessions[0].command_line, "topside");
        assert!(sessions[0].is_topside_process);
        assert!(!sessions[0].can_terminate);
        assert!(sessions[0].is_likely_dev);
    }

    #[test]
    fn does_not_mark_consumer_app_loopback_ports_as_dev() {
        let session = PortSession {
            pid: 1008,
            port: 24642,
            process_name: "Canva Helper".to_string(),
            command_line:
                "/Applications/Canva.app/Contents/Frameworks/Canva Helper.app/Contents/MacOS/Canva Helper"
                    .to_string(),
            user: "anthonymarti".to_string(),
            bindings: vec!["[::1]:24642".to_string()],
            other_ports: Vec::new(),
            is_topside_process: false,
            can_terminate: true,
            is_likely_dev: false,
        };

        assert!(!classify_likely_dev(&session, Some("anthonymarti")));
    }

    #[test]
    fn terminates_gracefully_when_port_disappears_after_term() {
        let runner = ScriptedCommandRunner::with_calls(vec![
            scripted_call(
                "lsof",
                LSOF_LISTEN_ARGS,
                ok("p999\ncnode\nLme\nf10\ntIPv4\nn127.0.0.1:3000\n"),
            ),
            scripted_call(
                "ps",
                &[
                    "-p", "999", "-ww", "-o", "pid=", "-o", "comm=", "-o", "args=",
                ],
                ok("999 /usr/local/bin/node /usr/local/bin/node server.js\n"),
            ),
            scripted_call("kill", &["-TERM", "999"], ok("")),
            scripted_call("lsof", LSOF_LISTEN_ARGS, ok("")),
        ]);
        let manager = SystemPortManager::with_runner(runner, 10158);

        let result = manager.terminate_session(999, 3000).unwrap();

        assert_eq!(result.message, "Ended session on port 3000");
        assert!(result.items.is_empty());
        manager.runner.assert_drained();
        assert_eq!(manager.runner.sleep_count(), 1);
    }

    #[test]
    fn escalates_to_kill_when_term_is_not_enough() {
        let runner = ScriptedCommandRunner::with_calls(vec![
            scripted_call(
                "lsof",
                LSOF_LISTEN_ARGS,
                ok("p999\ncnode\nLme\nf10\ntIPv4\nn127.0.0.1:3000\n"),
            ),
            scripted_call(
                "ps",
                &[
                    "-p", "999", "-ww", "-o", "pid=", "-o", "comm=", "-o", "args=",
                ],
                ok("999 /usr/local/bin/node /usr/local/bin/node server.js\n"),
            ),
            scripted_call("kill", &["-TERM", "999"], ok("")),
            scripted_call(
                "lsof",
                LSOF_LISTEN_ARGS,
                ok("p999\ncnode\nLme\nf10\ntIPv4\nn127.0.0.1:3000\n"),
            ),
            scripted_call(
                "ps",
                &[
                    "-p", "999", "-ww", "-o", "pid=", "-o", "comm=", "-o", "args=",
                ],
                ok("999 /usr/local/bin/node /usr/local/bin/node server.js\n"),
            ),
            scripted_call(
                "lsof",
                LSOF_LISTEN_ARGS,
                ok("p999\ncnode\nLme\nf10\ntIPv4\nn127.0.0.1:3000\n"),
            ),
            scripted_call(
                "ps",
                &[
                    "-p", "999", "-ww", "-o", "pid=", "-o", "comm=", "-o", "args=",
                ],
                ok("999 /usr/local/bin/node /usr/local/bin/node server.js\n"),
            ),
            scripted_call(
                "lsof",
                LSOF_LISTEN_ARGS,
                ok("p999\ncnode\nLme\nf10\ntIPv4\nn127.0.0.1:3000\n"),
            ),
            scripted_call(
                "ps",
                &[
                    "-p", "999", "-ww", "-o", "pid=", "-o", "comm=", "-o", "args=",
                ],
                ok("999 /usr/local/bin/node /usr/local/bin/node server.js\n"),
            ),
            scripted_call(
                "lsof",
                LSOF_LISTEN_ARGS,
                ok("p999\ncnode\nLme\nf10\ntIPv4\nn127.0.0.1:3000\n"),
            ),
            scripted_call(
                "ps",
                &[
                    "-p", "999", "-ww", "-o", "pid=", "-o", "comm=", "-o", "args=",
                ],
                ok("999 /usr/local/bin/node /usr/local/bin/node server.js\n"),
            ),
            scripted_call(
                "lsof",
                LSOF_LISTEN_ARGS,
                ok("p999\ncnode\nLme\nf10\ntIPv4\nn127.0.0.1:3000\n"),
            ),
            scripted_call(
                "ps",
                &[
                    "-p", "999", "-ww", "-o", "pid=", "-o", "comm=", "-o", "args=",
                ],
                ok("999 /usr/local/bin/node /usr/local/bin/node server.js\n"),
            ),
            scripted_call("kill", &["-KILL", "999"], ok("")),
            scripted_call("lsof", LSOF_LISTEN_ARGS, ok("")),
        ]);
        let manager = SystemPortManager::with_runner(runner, 10158);

        let result = manager.terminate_session(999, 3000).unwrap();

        assert_eq!(result.message, "Force ended session on port 3000");
        assert!(result.items.is_empty());
        manager.runner.assert_drained();
        assert_eq!(manager.runner.sleep_count(), 6);
    }

    #[test]
    fn reports_already_closed_ports_without_killing() {
        let runner = ScriptedCommandRunner::with_calls(vec![scripted_call(
            "lsof",
            LSOF_LISTEN_ARGS,
            ok(""),
        )]);
        let manager = SystemPortManager::with_runner(runner, 10158);

        let result = manager.terminate_session(999, 3000).unwrap();

        assert_eq!(result.message, "Port 3000 is already closed");
        assert!(result.items.is_empty());
        manager.runner.assert_drained();
    }

    #[test]
    fn returns_command_failures_from_kill() {
        let runner = ScriptedCommandRunner::with_calls(vec![
            scripted_call(
                "lsof",
                LSOF_LISTEN_ARGS,
                ok("p999\ncnode\nLme\nf10\ntIPv4\nn127.0.0.1:3000\n"),
            ),
            scripted_call(
                "ps",
                &[
                    "-p", "999", "-ww", "-o", "pid=", "-o", "comm=", "-o", "args=",
                ],
                ok("999 /usr/local/bin/node /usr/local/bin/node server.js\n"),
            ),
            scripted_call("kill", &["-TERM", "999"], failed("Operation not permitted")),
        ]);
        let manager = SystemPortManager::with_runner(runner, 10158);

        let error = manager.terminate_session(999, 3000).unwrap_err();

        assert_eq!(
            error,
            PortManagerError::CommandFailed("Operation not permitted".to_string())
        );
        manager.runner.assert_drained();
    }

    #[test]
    fn rejects_self_termination() {
        let runner = ScriptedCommandRunner::default();
        let manager = SystemPortManager::with_runner(runner, 10158);

        let error = manager.terminate_session(10158, 7410).unwrap_err();

        assert_eq!(
            error,
            PortManagerError::Forbidden("Topside cannot terminate its own process".to_string())
        );
    }
}
