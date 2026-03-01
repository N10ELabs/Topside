use std::path::{Path, PathBuf};

use anyhow::Result;

#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
use muda::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

#[cfg(target_os = "macos")]
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};

#[cfg(target_os = "macos")]
use wry::{NewWindowResponse, WebViewBuilder};

#[cfg(target_os = "macos")]
enum DesktopEvent {
    Menu(MenuEvent),
}

pub fn run_native_window(url: &str, title: &str, workspace_root: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let browser_url = url.to_string();
        let workspace_root = workspace_root.to_path_buf();
        let event_loop = EventLoopBuilder::<DesktopEvent>::with_user_event().build();
        let proxy = event_loop.create_proxy();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(DesktopEvent::Menu(event));
        }));

        let menu = DesktopMenu::install()?;
        let window = WindowBuilder::new()
            .with_title(title)
            .with_inner_size(LogicalSize::new(1440.0, 960.0))
            .with_min_inner_size(LogicalSize::new(1100.0, 720.0))
            .with_transparent(true)
            .build(&event_loop)?;

        let allowed_origin = app_origin(url);
        let _webview = WebViewBuilder::new()
            .with_url(url)
            .with_transparent(true)
            .with_initialization_script(
                "window.__N10E_DESKTOP__ = true; document.documentElement.dataset.n10eDesktop = 'true';",
            )
            .with_navigation_handler({
                let allowed_origin = allowed_origin.clone();
                move |target| allow_in_app_navigation(&allowed_origin, &target)
            })
            .with_new_window_req_handler(|target, _| {
                open_external_url(&target);
                NewWindowResponse::Deny
            })
            .with_accept_first_mouse(true)
            .build(&window)?;

        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;

            match event {
                Event::UserEvent(DesktopEvent::Menu(menu_event)) => {
                    menu.handle_event(&menu_event, &browser_url, &workspace_root)
                }
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            }
        });
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (url, title, workspace_root);
        anyhow::bail!("native desktop window is only supported on macOS")
    }
}

pub fn window_title(workspace_root: &Path) -> String {
    workspace_root
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("n10e - {value}"))
        .unwrap_or_else(|| "n10e".to_string())
}

fn app_origin(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

#[cfg(target_os = "macos")]
fn allow_in_app_navigation(allowed_origin: &str, target: &str) -> bool {
    if target.starts_with(allowed_origin) {
        return true;
    }

    open_external_url(target);
    false
}

#[cfg(target_os = "macos")]
fn open_external_url(target: &str) {
    let _ = Command::new("open").arg(target).spawn();
}

#[cfg(target_os = "macos")]
fn open_workspace_folder(path: &Path) {
    let _ = Command::new("open").arg(path).spawn();
}

#[cfg(target_os = "macos")]
struct DesktopMenu {
    _menu_bar: Menu,
    open_in_browser: MenuItem,
    open_workspace: MenuItem,
}

#[cfg(target_os = "macos")]
impl DesktopMenu {
    fn install() -> Result<Self> {
        let menu_bar = Menu::new();

        let app_menu = Submenu::new("n10e", true);
        let file_menu = Submenu::new("File", true);
        let edit_menu = Submenu::new("Edit", true);
        let window_menu = Submenu::new("Window", true);

        let open_in_browser = MenuItem::with_id("open-in-browser", "Open in Browser", true, None);
        let open_workspace =
            MenuItem::with_id("open-workspace-folder", "Open Workspace Folder", true, None);

        menu_bar.append_items(&[&app_menu, &file_menu, &edit_menu, &window_menu])?;

        app_menu.append_items(&[
            &PredefinedMenuItem::about(
                None,
                Some(AboutMetadata {
                    name: Some("n10e".to_string()),
                    version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    ..Default::default()
                }),
            ),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(None),
        ])?;

        file_menu.append_items(&[
            &open_in_browser,
            &open_workspace,
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::close_window(Some("Close")),
        ])?;

        edit_menu.append_items(&[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
        ])?;

        window_menu.append_items(&[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::fullscreen(None),
            &PredefinedMenuItem::bring_all_to_front(None),
        ])?;

        menu_bar.init_for_nsapp();
        window_menu.set_as_windows_menu_for_nsapp();

        Ok(Self {
            _menu_bar: menu_bar,
            open_in_browser,
            open_workspace,
        })
    }

    fn handle_event(&self, event: &MenuEvent, browser_url: &str, workspace_root: &PathBuf) {
        if event.id == self.open_in_browser.id() {
            open_external_url(browser_url);
        } else if event.id == self.open_workspace.id() {
            open_workspace_folder(workspace_root);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::window_title;

    #[test]
    fn window_title_uses_workspace_folder_name() {
        let title = window_title(Path::new("/tmp/my-workspace"));
        assert_eq!(title, "n10e - my-workspace");
    }

    #[test]
    fn window_title_falls_back_for_root_paths() {
        let title = window_title(Path::new("/"));
        assert_eq!(title, "n10e");
    }
}
