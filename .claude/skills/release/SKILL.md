---
name: release
description: Release a new e9s version to GitHub releases, Homebrew (simion/tap) and crates.io, end to end. Use when asked to release, publish, ship or tag a version.
---

# Release e9s

Do every step yourself, in the main checkout on `main` (never a git worktree), and report at the end.
Cargo is at `/opt/homebrew/opt/rustup/bin` if not on PATH.
Run `gh` as the `simion` account: prefix it with `GH_TOKEN=$(gh auth token --user simion)`.

1. On `main`: `git pull --rebase`. If the tree has uncommitted work meant for this release, commit it first (a message per change, as usual) and push; the tree must be clean before the bump.
2. Bump `version` in `Cargo.toml` (patch for fixes, minor for features), then `cargo build --release` so `Cargo.lock` follows, and `make test`.
3. Private-data check: if `CLAUDE.local.md` has a gated grep, run it over the tracked files and stop on any match. Never copy that pattern into a committed file.
4. Commit `Cargo.toml` and `Cargo.lock` as `e9s X.Y.Z`, tag and push both:
   `git tag -a vX.Y.Z -m "e9s X.Y.Z" && git push origin main vX.Y.Z`.
   The tag must match the `Cargo.toml` version or the workflow stops at `plan`.
5. crates.io: `cargo publish` from this checkout, right after the push, with the tree clean and the version matching the tag. Never `--allow-dirty`: a published version can never be replaced.
   No token (`no token found`): ask Simion to run `! cargo login` with a token from https://crates.io/settings/tokens (scope `publish-update`), then publish.
   Check: `curl -s -A e9s https://crates.io/api/v1/crates/e9s` shows `max_version` X.Y.Z.
6. Watch the Release workflow to the end (about 25 minutes):
   `gh run list --repo simion/e9s --workflow Release --limit 1`, then `gh run watch <id> --repo simion/e9s --exit-status` in the background.
   If only `publish-homebrew-formula` failed, read its log (`gh run view <id> --log-failed`); a missing or empty `HOMEBREW_TAP_TOKEN` is fixed by Simion setting the secret, then `gh run rerun <id> --failed`.
7. Homebrew: `brew update && brew upgrade e9s` (or `brew install simion/tap/e9s`), then `brew list --versions e9s` shows X.Y.Z.
8. Report the release page `https://github.com/simion/e9s/releases/tag/vX.Y.Z`, the crates.io version, and `brew install simion/tap/e9s`.

Never delete or move a pushed tag; fix forward with the next patch version.
