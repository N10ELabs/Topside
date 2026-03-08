# Renaming Guide

Topside is the public product name.

Current rename rules:

1. The CLI, crate, bundle, and release assets use `topside`.
2. New workspaces use `topside.toml` and `.topside`.
3. Existing workspaces may still auto-migrate from legacy `n10e` workspace names.
4. Managed task sync still reads legacy `n10e` sidecars and inline IDs where needed, but writes Topside-named sidecars going forward.
5. Documentation and packaging should treat `Topside` as the public-facing identity.
