# Renaming Guide (Codename -> Final Brand)

Current codename is `n10e-01` with runtime codename value in config (`codename`).

When final naming is chosen:

1. Update Cargo package/binary naming if needed (`Cargo.toml`).
2. Keep compatibility alias if possible for one release cycle.
3. Update `PROJECT_CODENAME` constant default.
4. Update docs and release workflows.
5. Provide migration note for existing `n10e.toml` files.

Recommended: keep CLI command `n10e` stable unless a strong rebrand requirement exists.

