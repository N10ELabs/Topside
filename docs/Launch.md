Use the current architecture as the base: a local Rust process serving a localhost web UI. That already matches how `serve` works today in [src/main.rs:94](/Users/anthonymarti/Desktop/N10E%20LABS%20Code/n10e-01/src/main.rs#L94) and how releases are scaffolded in [release.yml:21](/Users/anthonymarti/Desktop/N10E%20LABS%20Code/n10e-01/.github/workflows/release.yml#L21).

1. Developer-First Launch

Distribute with `cargo install --path .` first, then add Homebrew as the nicer install path for technical users. The launch command is `n10e --workspace <PATH> serve`. The UI opens in the user’s normal browser at `http://127.0.0.1:7410`, and the terminal stays attached while the app runs.

This is the best immediate path because it matches the existing product model exactly. It is low-friction to ship, easy to debug, and keeps the app clearly local-first. For early adopters, the “terminal owns the process, browser shows the UI” model is acceptable.

Recommended UX copy:
`n10e serve listening on http://127.0.0.1:7410`
`Open this URL in your browser. Press Ctrl+C to stop.`

2. Team / Internal Launch

Distribute signed GitHub release binaries plus a Homebrew tap. Keep the UI in the browser, but make launch feel less technical by providing a single install command and a single launch command. The ideal user flow is: install once, run `n10e open` or `n10e serve`, and the default browser opens automatically to the local UI.

For internal teams, I would not jump to a `.dmg` yet. The better tradeoff is a polished CLI experience:
- `brew install n10e`
- `n10e init ~/Work/my-project`
- `n10e --workspace ~/Work/my-project serve`
- Browser opens automatically

This gives non-Rust users a stable install path without introducing native app packaging complexity. It also keeps support simple because the runtime model is still transparent.

3. Polished Consumer macOS Launch

If you want a consumer-grade Mac experience later, package a thin native `.app` inside a `.dmg`, but keep the UI in the browser. The `.app` should be a launcher/supervisor for the same `n10e` binary, not a rewrite of the frontend.

The ideal UX is:
- User drags `n10e.app` into Applications.
- On first launch, the app asks for or creates a workspace.
- The app starts the local `n10e` server in the background.
- The app opens the default browser to the localhost UI.
- The app exposes simple controls like “Open Browser”, “Open Workspace Folder”, and “Quit n10e”.

That gives you a Mac-native install story while preserving the current web UI architecture. It also avoids the cost of moving to Tauri or Electron before you actually need native-window behavior.

My recommendation is to ship in stages:
1. Ship Homebrew + tarball first.
2. Make launch smoother with automatic browser-open behavior.
3. Add a thin `.app`/`.dmg` only when you need non-technical Mac users to install it.

I would not make a `.dmg` the primary distribution yet. I would make “local binary that opens a browser UI” the product, and treat the `.app` as a convenience wrapper later.

No code changes were made.