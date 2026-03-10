# Release Guide

Last updated: 2026-03-10

Topside `v0.1.0` is shipped from the `N10ELabs/Topside` GitHub repository.

## Preflight

Run the local checks before tagging:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
tmpdir="$(mktemp -d)"
cargo run -- init "$tmpdir"
cargo run -- --workspace "$tmpdir" doctor
./scripts/package-macos-release.sh --output-dir ./dist
```

Expected local release outputs:

- `dist/Topside.app`
- `dist/topside-macos-<arch>.dmg`
- `dist/topside-macos-<arch>.tar.gz`
- `dist/checksums.txt`

## GitHub Release

Push a semver tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The GitHub release workflow will:

- build and package the macOS release artifacts
- publish the `.dmg`, CLI tarball, checksums, and generated `topside.rb` formula as release assets
- update `Formula/topside.rb` on the default branch to match the tagged release

## Homebrew

Because the repository is named `Topside` rather than `homebrew-topside`, use an explicit tap URL:

```bash
brew tap N10ELabs/Topside https://github.com/N10ELabs/Topside
brew install N10ELabs/Topside/topside
```

You can regenerate the formula locally for a tagged release with:

```bash
./scripts/render-homebrew-formula.sh --repo N10ELabs/Topside --version 0.1.0
```

## Post-release verification

After the workflow finishes:

1. Confirm the GitHub release includes the `.dmg`, CLI tarball, `checksums.txt`, and `topside.rb`.
2. Confirm `Formula/topside.rb` on the default branch has the new version and SHA-256.
3. On a clean macOS machine, verify:

```bash
brew tap N10ELabs/Topside https://github.com/N10ELabs/Topside
brew install N10ELabs/Topside/topside
topside --version
```
