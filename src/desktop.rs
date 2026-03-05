use std::path::{Path, PathBuf};

use anyhow::Result;

#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
use objc2::{AllocAnyThread, MainThreadMarker};

#[cfg(target_os = "macos")]
use objc2::rc::Retained;

#[cfg(target_os = "macos")]
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSImage, NSMenu, NSMenuItem};

#[cfg(target_os = "macos")]
use objc2_foundation::{NSData, NSProcessInfo, NSString};

#[cfg(target_os = "macos")]
use muda::accelerator::{Accelerator, CMD_OR_CTRL, Code};

#[cfg(target_os = "macos")]
use muda::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

#[cfg(target_os = "macos")]
use tao::{
    dpi::LogicalSize,
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};

#[cfg(target_os = "macos")]
use window_vibrancy::{NSVisualEffectMaterial, NSVisualEffectState, apply_vibrancy};

#[cfg(target_os = "macos")]
use wry::{NewWindowResponse, WebView, WebViewBuilder};

#[cfg(target_os = "macos")]
enum DesktopEvent {
    Menu(MenuEvent),
}

#[cfg(target_os = "macos")]
enum ZoomAction {
    In,
    Out,
    Reset,
}

#[cfg(target_os = "macos")]
const DEFAULT_ZOOM_LEVEL: f64 = 1.0;

#[cfg(target_os = "macos")]
const MIN_ZOOM_LEVEL: f64 = 0.5;

#[cfg(target_os = "macos")]
const MAX_ZOOM_LEVEL: f64 = 3.0;

#[cfg(target_os = "macos")]
const ZOOM_STEP: f64 = 0.1;

#[cfg(target_os = "macos")]
const APP_ICON_BYTES: &[u8] = include_bytes!("../topside.icns");

#[cfg(target_os = "macos")]
fn normalize_zoom_level(value: f64) -> f64 {
    ((value * 10.0).round() / 10.0).clamp(MIN_ZOOM_LEVEL, MAX_ZOOM_LEVEL)
}

#[cfg(target_os = "macos")]
fn apply_zoom_level(webview: &WebView, current_zoom: &mut f64, next_zoom: f64) {
    let next_zoom = normalize_zoom_level(next_zoom);
    if (next_zoom - *current_zoom).abs() < 0.001 {
        return;
    }

    if webview.zoom(next_zoom).is_ok() {
        *current_zoom = next_zoom;
    }
}

#[cfg(target_os = "macos")]
fn apply_zoom_action(webview: &WebView, current_zoom: &mut f64, action: ZoomAction) {
    match action {
        ZoomAction::In => apply_zoom_level(webview, current_zoom, *current_zoom + ZOOM_STEP),
        ZoomAction::Out => apply_zoom_level(webview, current_zoom, *current_zoom - ZOOM_STEP),
        ZoomAction::Reset => apply_zoom_level(webview, current_zoom, DEFAULT_ZOOM_LEVEL),
    }
}

#[cfg(target_os = "macos")]
fn find_menu_item_by_title(menu: &NSMenu, title: &str) -> Option<Retained<NSMenuItem>> {
    for index in 0..menu.numberOfItems() {
        let item = menu.itemAtIndex(index)?;
        if item.title().to_string() == title {
            return Some(item);
        }
    }

    None
}

#[cfg(target_os = "macos")]
fn configure_native_zoom_in_shortcut() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(main_menu) = NSApplication::sharedApplication(mtm).mainMenu() else {
        return;
    };
    let Some(view_item) = find_menu_item_by_title(&main_menu, "View") else {
        return;
    };
    let Some(view_menu) = view_item.submenu() else {
        return;
    };
    let Some(zoom_in_item) = find_menu_item_by_title(&view_menu, "Zoom In") else {
        return;
    };

    let key_equivalent = NSString::from_str("+");
    zoom_in_item.setKeyEquivalent(&key_equivalent);
    zoom_in_item.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);
}

#[cfg(target_os = "macos")]
fn configure_process_name() {
    let process_name = NSString::from_str("Topside");
    NSProcessInfo::processInfo().setProcessName(&process_name);
}

#[cfg(target_os = "macos")]
fn configure_application_icon() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let icon_data = NSData::with_bytes(APP_ICON_BYTES);
    let Some(icon) = NSImage::initWithData(NSImage::alloc(), &icon_data) else {
        return;
    };

    unsafe {
        NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&icon));
    }
}

pub fn run_native_window(url: &str, title: &str, workspace_root: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let browser_url = url.to_string();
        let workspace_root = workspace_root.to_path_buf();
        configure_process_name();
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
        let webview = WebViewBuilder::new()
            .with_url(url)
            .with_transparent(true)
            .with_background_color((0, 0, 0, 0))
            .with_initialization_script(
                "window.__TOPSIDE_DESKTOP__ = true; document.documentElement.dataset.topsideDesktop = 'true';",
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

        apply_vibrancy(
            &window,
            NSVisualEffectMaterial::UnderWindowBackground,
            Some(NSVisualEffectState::Active),
            None,
        )?;
        configure_application_icon();

        let mut zoom_level = DEFAULT_ZOOM_LEVEL;

        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;

            match event {
                Event::NewEvents(StartCause::Init) | Event::Resumed => {
                    configure_process_name();
                    configure_application_icon();
                }
                Event::UserEvent(DesktopEvent::Menu(menu_event)) => menu.handle_event(
                    &menu_event,
                    &browser_url,
                    &workspace_root,
                    &webview,
                    &mut zoom_level,
                ),
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

pub fn window_title(_workspace_root: &Path) -> String {
    "Topside".to_string()
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
    zoom_in: MenuItem,
    zoom_out: MenuItem,
    reset_zoom: MenuItem,
}

#[cfg(target_os = "macos")]
impl DesktopMenu {
    fn install() -> Result<Self> {
        let menu_bar = Menu::new();

        let app_menu = Submenu::new("Topside", true);
        let file_menu = Submenu::new("File", true);
        let edit_menu = Submenu::new("Edit", true);
        let view_menu = Submenu::new("View", true);
        let window_menu = Submenu::new("Window", true);

        let open_in_browser = MenuItem::with_id("open-in-browser", "Open in Browser", true, None);
        let open_workspace =
            MenuItem::with_id("open-workspace-folder", "Open Workspace Folder", true, None);
        let zoom_in = MenuItem::with_id("view-zoom-in", "Zoom In", true, None);
        let zoom_out = MenuItem::with_id(
            "view-zoom-out",
            "Zoom Out",
            true,
            Some(Accelerator::new(Some(CMD_OR_CTRL), Code::Minus)),
        );
        let reset_zoom = MenuItem::with_id(
            "view-zoom-reset",
            "Actual Size",
            true,
            Some(Accelerator::new(Some(CMD_OR_CTRL), Code::Digit0)),
        );

        menu_bar.append_items(&[&app_menu, &file_menu, &edit_menu, &view_menu, &window_menu])?;

        app_menu.append_items(&[
            &PredefinedMenuItem::about(
                None,
                Some(AboutMetadata {
                    name: Some("Topside".to_string()),
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

        view_menu.append_items(&[
            &zoom_in,
            &zoom_out,
            &reset_zoom,
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::fullscreen(None),
        ])?;

        window_menu.append_items(&[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::bring_all_to_front(None),
        ])?;

        menu_bar.init_for_nsapp();
        configure_native_zoom_in_shortcut();
        window_menu.set_as_windows_menu_for_nsapp();

        Ok(Self {
            _menu_bar: menu_bar,
            open_in_browser,
            open_workspace,
            zoom_in,
            zoom_out,
            reset_zoom,
        })
    }

    fn handle_event(
        &self,
        event: &MenuEvent,
        browser_url: &str,
        workspace_root: &PathBuf,
        webview: &WebView,
        zoom_level: &mut f64,
    ) {
        if event.id == self.open_in_browser.id() {
            open_external_url(browser_url);
        } else if event.id == self.open_workspace.id() {
            open_workspace_folder(workspace_root);
        } else if event.id == self.zoom_in.id() {
            apply_zoom_action(webview, zoom_level, ZoomAction::In);
        } else if event.id == self.zoom_out.id() {
            apply_zoom_action(webview, zoom_level, ZoomAction::Out);
        } else if event.id == self.reset_zoom.id() {
            apply_zoom_action(webview, zoom_level, ZoomAction::Reset);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::window_title;

    #[cfg(target_os = "macos")]
    use super::{MAX_ZOOM_LEVEL, MIN_ZOOM_LEVEL, normalize_zoom_level};

    #[test]
    fn window_title_is_fixed() {
        let title = window_title(Path::new("/tmp/my-workspace"));
        assert_eq!(title, "Topside");
    }

    #[test]
    fn window_title_falls_back_for_root_paths() {
        let title = window_title(Path::new("/"));
        assert_eq!(title, "Topside");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn normalize_zoom_level_rounds_and_clamps() {
        assert_eq!(normalize_zoom_level(1.04), 1.0);
        assert_eq!(normalize_zoom_level(1.06), 1.1);
        assert_eq!(normalize_zoom_level(0.2), MIN_ZOOM_LEVEL);
        assert_eq!(normalize_zoom_level(9.9), MAX_ZOOM_LEVEL);
    }
}
