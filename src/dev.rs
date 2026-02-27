use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

const DEV_WATCH_PATHS: &[&str] = &["src", "templates", "assets", "Cargo.toml", "Cargo.lock"];

pub async fn run_dev_supervisor(workspace: Option<PathBuf>) -> Result<()> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = workspace.map(|path| {
        if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    });

    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })
    .context("failed creating dev watcher")?;

    for relative in DEV_WATCH_PATHS {
        let path = manifest_dir.join(relative);
        if !path.exists() {
            continue;
        }
        let mode = if path.is_dir() {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher
            .watch(&path, mode)
            .with_context(|| format!("failed watching {}", path.display()))?;
    }

    let mut generation = 1u64;
    let mut child = spawn_dev_child(&manifest_dir, workspace.as_deref(), generation)?;

    println!("n10e dev watching {}", DEV_WATCH_PATHS.join(", "));

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                stop_child(&mut child)?;
                println!("n10e dev stopped");
                break;
            }
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else {
                    stop_child(&mut child)?;
                    break;
                };

                if let Err(err) = event {
                    eprintln!("n10e dev watcher error: {err}");
                    continue;
                }

                tokio::time::sleep(Duration::from_millis(250)).await;
                while rx.try_recv().is_ok() {}

                println!("n10e dev change detected, rebuilding...");
                stop_child(&mut child)?;
                generation += 1;
                child = spawn_dev_child(&manifest_dir, workspace.as_deref(), generation)?;
            }
        }
    }

    Ok(())
}

fn spawn_dev_child(
    manifest_dir: &Path,
    workspace: Option<&Path>,
    generation: u64,
) -> Result<Child> {
    let manifest_path = manifest_dir.join("Cargo.toml");
    let mut command = Command::new("cargo");
    command
        .arg("run")
        .arg("--manifest-path")
        .arg(&manifest_path)
        .arg("--")
        .env("N10E_DEV_RELOAD_TOKEN", generation.to_string())
        .current_dir(manifest_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    if let Some(workspace) = workspace {
        command.arg("--workspace").arg(workspace);
    }

    command.arg("serve");

    command.spawn().with_context(|| {
        format!(
            "failed spawning cargo serve from {}",
            manifest_dir.display()
        )
    })
}

fn stop_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }

    child.kill().context("failed stopping dev child")?;
    let _ = child.wait();
    Ok(())
}
