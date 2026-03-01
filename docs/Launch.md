Use the current architecture as the base: a local Rust process serving a localhost web UI. That already matches how `serve` and `open` work today in [src/main.rs](/Users/anthonymarti/Desktop/N10E%20LABS%20Code/n10e-01/src/main.rs) and how releases are scaffolded in [release.yml:21](/Users/anthonymarti/Desktop/N10E%20LABS%20Code/n10e-01/.github/workflows/release.yml#L21).

1. Developer-First Launch

Distribute with `cargo install --path .` first, then add Homebrew as the nicer install path for technical users. The launch commands are `n10e --workspace <PATH> serve` for browser-first usage and `n10e --workspace <PATH> open` for the native macOS window shell. `serve` opens the UI in the user’s normal browser at `http://127.0.0.1:7410`; `open` launches the same local UI inside a native macOS webview window.

This is the best immediate path because it matches the existing product model exactly. It is low-friction to ship, easy to debug, and keeps the app clearly local-first. For early adopters, the “terminal owns the process, browser shows the UI” model is acceptable.

Recommended UX copy:
`n10e serve listening on http://127.0.0.1:7410`
`Open this URL in your browser. Press Ctrl+C to stop.`

2. Team / Internal Launch

Distribute signed GitHub release binaries plus a Homebrew tap. Keep the UI local-first, but make launch feel less technical by providing a single install command and a single launch command. The ideal user flow is: install once, run `n10e open` or `n10e serve`, and the app opens either in the native desktop shell (`open`) or the default browser (`serve`).

For internal teams, I would not jump to a `.dmg` yet. The better tradeoff is a polished CLI experience:
- `brew install n10e`
- `n10e init ~/Work/my-project`
- `n10e --workspace ~/Work/my-project open`
- Native app window opens automatically

This gives non-Rust users a stable install path without introducing native app packaging complexity. It also keeps support simple because the runtime model is still transparent.

3. Polished Consumer macOS Launch

If you want a consumer-grade Mac experience later, package a thin native `.app` inside a `.dmg`, but keep the UI in the browser. The `.app` should be a launcher/supervisor for the same `n10e` binary, not a rewrite of the frontend.

Current implementation note: `n10e bundle-app --output-dir ./dist` now creates `dist/n10e.app`, bundling the current `n10e` binary plus a small launcher script. The generated app launches the bundled binary with `open`, using the embedded `--workspace` path if one was provided at bundle time, otherwise prompting the user to choose a workspace on first launch. `bundle-app` also accepts `--icon /path/to/icon.icns` so the app bundle can carry a real macOS icon.

The native window shell now installs a proper macOS menu bar. That menu includes standard App/Edit/Window menus plus `Open in Browser` and `Open Workspace Folder`, so the wrapped web UI behaves more like a Mac app instead of a bare webview.

The ideal UX is:
- User drags `n10e.app` into Applications.
- On first launch, the app asks for or creates a workspace.
- The app starts the local `n10e` server in the background.
- The app opens the local UI in a native webview window.
- The app exposes simple controls like “Open Browser”, “Open Workspace Folder”, and “Quit n10e”.

That gives you a Mac-native install story while preserving the current web UI architecture. It also avoids the cost of moving to Tauri or Electron before you actually need native-window behavior.

For release packaging, `./scripts/package-macos-release.sh --output-dir ./dist` now builds the release binary, generates `dist/n10e.app`, and packages it as `dist/n10e-macos.dmg`. If you have a Developer ID certificate, pass `--sign-identity` to sign the `.app` and `.dmg` during the same step.

My recommendation is to ship in stages:
1. Ship Homebrew + tarball first.
2. Make launch smoother with automatic browser-open behavior.
3. Add a thin `.app`/`.dmg` only when you need non-technical Mac users to install it.

I would not make a `.dmg` the primary distribution yet. I would make “local binary that opens a browser UI” the product, and treat the `.app` as a convenience wrapper later.
