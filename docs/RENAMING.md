# Renaming Guide

Topside is now the final product name.

Current rename rules:

1. The CLI, crate, bundle, and release assets use `topside`.
2. New workspaces use `topside.toml` and `.topside`.
3. Existing workspaces auto-migrate from `n10e.toml` and `.n10e` when loaded.
4. Managed task sync still reads legacy `n10e` sidecars and inline IDs, but writes Topside-named sidecars going forward.
5. Documentation and packaging should treat `Topside` as the only public-facing product identity.
