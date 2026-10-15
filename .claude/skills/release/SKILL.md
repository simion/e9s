---
name: release
description: Release a new e9s version to GitHub releases, Homebrew (simion/tap) and crates.io. Use when asked to release, publish, ship or tag a version.
---

# Release e9s

Cargo is at `/opt/homebrew/opt/rustup/bin` if not on PATH.
Run `gh` as the `simion` account: prefix it with `GH_TOKEN=$(gh auth token --user simion)`.

1. On `main`, clean, up to date: `git pull --rebase`.
2. Bump `version` in `Cargo.toml` (patch for fixes, minor for features), then `cargo build --release` so `Cargo.lock` follows, and `make test`.
3. Private-data check: if `CLAUDE.local.md` exists, run its gated grep over the tracked files and stop on any match. Never copy that pattern into a committed file.
4. Commit `Cargo.toml` and `Cargo.lock` as `e9s X.Y.Z`, tag and push both:
   `git tag -a vX.Y.Z -m "e9s X.Y.Z" && git push origin main vX.Y.Z`.
   The tag must match the `Cargo.toml` version or the workflow stops at `plan`.
5. Watch the Release workflow to the end (about 25 minutes):
   `gh run list --repo simion/e9s --workflow Release --limit 1`, then `gh run watch <id> --repo simion/e9s --exit-status` in the background.
   If only `publish-homebrew-formula` failed, read its log (`gh run view <id> --log-failed`); a missing or empty `HOMEBREW_TAP_TOKEN` is fixed by Simion setting the secret, then `gh run rerun <id> --failed`.
6. Check: `brew update && brew upgrade e9s` (or `brew install simion/tap/e9s`), then `brew list --versions e9s` shows X.Y.Z.
7. crates.io: ask Simion to run `cargo publish` in the repo (auto mode refuses it for agents). Check: `curl -s -A e9s https://crates.io/api/v1/crates/e9s` shows `max_version` X.Y.Z.
8. Report the release page `https://github.com/simion/e9s/releases/tag/vX.Y.Z` and `brew install simion/tap/e9s`.

Never delete or move a pushed tag; fix forward with the next patch version.
