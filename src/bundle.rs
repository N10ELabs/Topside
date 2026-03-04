use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const BUNDLE_DISPLAY_NAME: &str = "Topside";
const BUNDLE_EXECUTABLE_NAME: &str = "topside";
const BUNDLE_IDENTIFIER: &str = "labs.topside.desktop";
const MINIMUM_MACOS_VERSION: &str = "12.0";

pub fn bundle_macos_app(
    source_binary: &Path,
    output_dir: &Path,
    default_workspace: Option<&Path>,
    icon_path: Option<&Path>,
) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let bundle_root = output_dir.join("Topside.app");
        if bundle_root.exists() {
            fs::remove_dir_all(&bundle_root).with_context(|| {
                format!("failed removing existing bundle {}", bundle_root.display())
            })?;
        }

        let contents_dir = bundle_root.join("Contents");
        let macos_dir = contents_dir.join("MacOS");
        let resources_dir = contents_dir.join("Resources");
        let bundle_binary_dir = resources_dir.join("bin");
        let icon_file_name = icon_path.map(bundle_icon_file_name).transpose()?;
        fs::create_dir_all(&macos_dir)
            .with_context(|| format!("failed creating {}", macos_dir.display()))?;
        fs::create_dir_all(&bundle_binary_dir)
            .with_context(|| format!("failed creating {}", bundle_binary_dir.display()))?;

        let launcher_path = macos_dir.join(BUNDLE_EXECUTABLE_NAME);
        let bundled_binary_path = bundle_binary_dir.join(BUNDLE_EXECUTABLE_NAME);
        let info_plist_path = contents_dir.join("Info.plist");

        let launcher = render_launcher_script(default_workspace);
        fs::write(&launcher_path, launcher)
            .with_context(|| format!("failed writing {}", launcher_path.display()))?;
        make_executable(&launcher_path)?;

        fs::copy(source_binary, &bundled_binary_path).with_context(|| {
            format!(
                "failed copying {} to {}",
                source_binary.display(),
                bundled_binary_path.display()
            )
        })?;
        make_executable(&bundled_binary_path)?;

        if let (Some(icon_path), Some(icon_file_name)) = (icon_path, icon_file_name.as_deref()) {
            let bundled_icon_path = resources_dir.join(icon_file_name);
            fs::copy(icon_path, &bundled_icon_path).with_context(|| {
                format!(
                    "failed copying {} to {}",
                    icon_path.display(),
                    bundled_icon_path.display()
                )
            })?;
        }

        fs::write(
            &info_plist_path,
            render_info_plist(icon_file_name.as_deref()),
        )
        .with_context(|| format!("failed writing {}", info_plist_path.display()))?;

        Ok(bundle_root)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (source_binary, output_dir, default_workspace, icon_path);
        anyhow::bail!("app bundling is only supported on macOS")
    }
}

fn render_info_plist(icon_file_name: Option<&str>) -> String {
    let icon_entry = icon_file_name
        .map(|name| {
            format!(
                "  <key>CFBundleIconFile</key>\n  <string>{}</string>\n",
                xml_escape(name)
            )
        })
        .unwrap_or_default();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>{bundle_executable_name}</string>
  <key>CFBundleIdentifier</key>
  <string>{bundle_identifier}</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>{bundle_display_name}</string>
  <key>CFBundleDisplayName</key>
  <string>{bundle_display_name}</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
{icon_entry}  <key>CFBundleShortVersionString</key>
  <string>{bundle_version}</string>
  <key>CFBundleVersion</key>
  <string>{bundle_version}</string>
  <key>LSMinimumSystemVersion</key>
  <string>{minimum_macos_version}</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
</dict>
</plist>
"#,
        bundle_identifier = xml_escape(BUNDLE_IDENTIFIER),
        bundle_display_name = xml_escape(BUNDLE_DISPLAY_NAME),
        bundle_executable_name = xml_escape(BUNDLE_EXECUTABLE_NAME),
        bundle_version = xml_escape(env!("CARGO_PKG_VERSION")),
        icon_entry = icon_entry,
        minimum_macos_version = xml_escape(MINIMUM_MACOS_VERSION),
    )
}

fn render_launcher_script(default_workspace: Option<&Path>) -> String {
    let default_workspace = default_workspace
        .map(|path| shell_single_quote(&path.to_string_lossy()))
        .unwrap_or_else(|| "''".to_string());

    format!(
        r#"#!/bin/zsh
set -eu

APP_CONTENTS="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$APP_CONTENTS/Resources/bin/topside"
DEFAULT_WORKSPACE={default_workspace}

WORKSPACE="${{1:-}}"
if [ -n "$WORKSPACE" ] && [[ "$WORKSPACE" == -psn_* ]]; then
  WORKSPACE=""
fi

if [ -z "$WORKSPACE" ]; then
  for arg in "$@"; do
    if [[ "$arg" == -psn_* ]]; then
      continue
    fi
    WORKSPACE="$arg"
    break
  done
fi

if [ -z "$WORKSPACE" ] && [ -n "$DEFAULT_WORKSPACE" ] && [ -d "$DEFAULT_WORKSPACE" ]; then
  WORKSPACE="$DEFAULT_WORKSPACE"
fi

if [ -z "$WORKSPACE" ]; then
  WORKSPACE="$(osascript -e 'POSIX path of (choose folder with prompt "Select Topside workspace")' 2>/dev/null || true)"
  WORKSPACE="${{WORKSPACE%/}}"
fi

if [ -z "$WORKSPACE" ]; then
  exit 0
fi

"$BIN" --workspace "$WORKSPACE" open &
wait "$!"
"#
    )
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

fn bundle_icon_file_name(path: &Path) -> Result<String> {
    if !path.exists() {
        anyhow::bail!("bundle icon does not exist: {}", path.display());
    }

    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !extension.eq_ignore_ascii_case("icns") {
        anyhow::bail!("bundle icon must be a .icns file: {}", path.display());
    }

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .context("bundle icon file name is invalid")?;

    Ok(file_name.to_string())
}

fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path)
            .with_context(|| format!("failed reading metadata for {}", path.display()))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed setting executable bit for {}", path.display()))?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use tempfile::NamedTempFile;

    use super::{
        bundle_icon_file_name, render_info_plist, render_launcher_script, shell_single_quote,
        xml_escape,
    };

    #[test]
    fn info_plist_contains_app_identity() {
        let plist = render_info_plist(None);
        assert!(plist.contains("<string>Topside</string>"));
        assert!(plist.contains("<string>topside</string>"));
        assert!(plist.contains("<string>APPL</string>"));
        assert!(plist.contains(env!("CARGO_PKG_VERSION")));
        assert!(!plist.contains("CFBundleIconFile"));
    }

    #[test]
    fn info_plist_includes_icon_when_requested() {
        let plist = render_info_plist(Some("topside.icns"));
        assert!(plist.contains("CFBundleIconFile"));
        assert!(plist.contains("<string>topside.icns</string>"));
    }

    #[test]
    fn launcher_script_embeds_default_workspace() {
        let script = render_launcher_script(Some(Path::new("/tmp/project")));
        assert!(script.contains("DEFAULT_WORKSPACE='/tmp/project'"));
        assert!(script.contains(r#""$BIN" --workspace "$WORKSPACE" open &"#));
        assert!(script.contains(r#"wait "$!""#));
        assert!(script.contains(r#"[[ "$WORKSPACE" == -psn_* ]]"#));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        let quoted = shell_single_quote("/tmp/o'reilly");
        assert_eq!(quoted, r#"'/tmp/o'\''reilly'"#);
    }

    #[test]
    fn bundle_icon_requires_icns_extension() {
        let file = NamedTempFile::new().expect("create temp file");
        let error = bundle_icon_file_name(file.path()).expect_err("non-icns icon should fail");
        assert!(error.to_string().contains(".icns"));
    }

    #[test]
    fn bundle_icon_uses_file_name() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let icon_path = dir.path().join("topside.icns");
        fs::write(&icon_path, b"fake-icon").expect("write icon");

        let icon_name = bundle_icon_file_name(&icon_path).expect("icon file name");
        assert_eq!(icon_name, "topside.icns");
    }

    #[test]
    fn xml_escape_handles_plist_special_characters() {
        assert_eq!(xml_escape(r#""<&>'"#), "&quot;&lt;&amp;&gt;&apos;");
    }
}
