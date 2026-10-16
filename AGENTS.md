# e9s

A read-only, k9s-style terminal UI for AWS, in Rust (edition 2024, ratatui 0.30).
The README is the user guide; this file is for whoever changes the code.

## Build and check

```sh
make                          # list the targets
make run ARGS="-p <profile>"  # build and launch
make test                     # 33 tests, no AWS access needed
make install                  # ./install.sh: a `e9s` launcher in ~/.local/bin that rebuilds when src/ changes
e9s -p <profile> -r <region> --cluster <name> --script "@wait @snap"
```

`--script` drives the real app on an in-memory screen and prints it.
Tokens are separated by spaces: `:svc <Enter> @wait H @snap`, not `:svc<Enter>`.
`@shot=shots/<name>` writes the screen as HTML with exact colours; `make shots` renders every `shots/*.html` to PNG with headless Chrome (`shots/` is gitignored).
The screen is 200x60; `@size=140x40` checks a laptop-sized terminal, where most layout problems show first.
This is how every UI change gets checked without a terminal.

The Makefile finds cargo even when it is not on PATH (rustup under Homebrew); plain `cargo` calls need `export PATH=/opt/homebrew/opt/rustup/bin:$PATH` there.
macOS has no `timeout`; run long checks in the background and kill them.

## Layout

| File | What lives there |
|---|---|
| `src/aws.rs` | The read-only guard (`check_read_only`: service allowlist, `describe-`/`list-`/`get-` prefixes, `READ_EXACT`, `DENIED`), the per-screen command log (`Call`, `screen_log`, `inflight`), `display_cmd`, profiles and the SSO login state (`sso_login`) |
| `src/demo.rs` | `E9S_DEMO=1`: a made-up account for the README recording and screenshots: `make demo` runs `docs/demo/screens.sh` and `docs/demo/demo.tape` with `HOME=docs/demo/home` (made-up profiles and SSO login). A field a view starts reading needs adding here too, as in `sdk.rs` |
| `src/upgrade.rs` | The k9s-style check for a newer release at startup (curl to the GitHub releases API, in the background, silent on failure), shown under the logo as `⚡ v0.1.1 available` with the upgrade command right-aligned on the header's last row (next to `Rev:` when the logo is hidden), and printed on quit, chosen by where the binary lives (brew `Cellar`, `cargo install` per `~/.cargo/.crates2.json`, else the shell installer). Skipped for builds from a checkout, in demo mode, and with `E9S_NO_UPGRADE_CHECK=1` |
| `src/sdk.rs` | The transport: AWS SDK clients on one tokio runtime, credentials per profile shared across regions; `run` takes the CLI's service, operation and arguments and returns the CLI's JSON shape, so views never changed. Only the reads in `dispatch` exist |
| `src/views.rs` | Every view: `View` enum, fetch, columns, health, drill-down, log resolution and live tail (`resolve`, `logs`, `filter_all`, `tail_poll`), failures |
| `src/app.rs` | State (`App`, `Frame` stack), keys, commands, tail thread, save and copy |
| `src/ui.rs` | Rendering: header and logo, tables, the log/describe text body, overlays (`help`, `calls`), footer |
| `src/describe.rs` | Readable `d` pages per resource |
| `src/learn.rs` | `?` text: each view in AWS terms and its Kubernetes equivalent |
| `src/logic.rs` | Pure helpers: deployment split, redaction, sorting, time |

## Rules the code keeps

- **Read-only.** Every call goes through `Aws::call`, which runs `check` first. Do not add a second path to the runner. `logs filter-log-events` and `cloudtrail lookup-events` are the only non-prefix reads allowed.
- **`--write` opens `WRITES` only** (`aws.rs`): secret values, `ecs update-service` (`sdk.rs` sends only `--desired-count` or `--force-new-deployment`, the `r` restart), `register-scalable-target` and `put-scheduled-action` (the `e9s-restore` action behind `s ... for 2h`). Every write is staged as `app::Pending` and runs from a k9s-style dialog (Cancel focused first; arrows or Tab move to OK, Enter presses, `y` is a shortcut). A secret write reads it again and refuses if its version moved; the dialog names keys, never values. `H` shows `--secret-string` as `<redacted>`.
- **Never show secrets** (except Secrets Manager values under `--write`, masked until `x`). Environment variable and secret values are redacted from every response (`logic.rs`, tested), including CloudTrail request parameters (keys naming a password, secret or token).
- **Failures are loud.** A failed or partial fetch shows a note; it never renders as an empty or healthy list.
- **k9s parity.** Keys and screens follow k9s. When adding log or table behaviour, read the k9s source first (github.com/derailed/k9s, `internal/view/log.go`, `log_indicator.go`) and match its keys.
- **Measure before claiming speed.** The log design came from timing real calls (see below); keep doing that.

## Log design, and why

- Service logs name each running task's stream (`prefix/container/taskid`) and send them 100 per `filter-log-events` call (the API limit), in parallel, merged by timestamp.
  Querying by the service's stream prefix also matched every stream the service ever had (about 10,000 on a busy service) and took 18.6 s for 7 s of logs; naming the streams took about 2.5 s.
- Live tail polls `filter-log-events` every 2 s with 5 s of overlap, dropping duplicates by `eventId`.
  StartLiveTail is billed per minute; polling costs nothing extra and lines keep their stream.
- If one poll returns more than 10,000 lines, the cursor jumps to now and the screen says lines were skipped.
- Windows `2`-`6` read up to 5,000 lines per 100 streams and say when cut.

## Transport, and why

Calls go through the AWS SDK for Rust (`src/sdk.rs`); the CLI is only run for `L` (`aws sso login`), with the screen suspended.
Measured on prod (2026-10-15): a CLI call cost about 1.0 s, of which about 0.7 s was the process (Python start, botocore, reading credentials).
With the SDK a warm services refresh is 2.2 s, all AWS time (`describe-tasks` of 52 tasks alone takes 1.5 s); screens later in a session got 30 to 40% faster.
The first call of a run is slower than with the CLI (the SDK fetches role credentials once per profile, the CLI kept them in `~/.aws/cli/cache`).
A clean release build takes about 2.5 minutes (eleven SDK crates); incremental builds are seconds.
`dispatch` builds the CLI's JSON by hand from the SDK types (their serde support is behind an unstable flag), so a field a view starts reading must be added there too.

## Authentication

`aws.rs` works out how each profile signs in (`auth_of`), follows roles to the profile that holds the login (`login_target`), and reads the login state from the exact cache file the SDK reads (`login_state`: sha1 of the sso-session name or start URL under `~/.aws/sso/cache`, sha256 of the `login_session` ARN under `~/.aws/login/cache`).
An expired or missing login shows `Mode::OfferLogin` once (asked again only after credentials worked again); the login runs with the terminal handed back to the CLI (`main.rs`, `run_login`).
Verified 2026-10-15: SSO, legacy SSO, `aws login`, role via SSO, role via an expired SSO profile, MFA role, keys, `credential_process`, web identity, env vars and `AWS_PROFILE`, plus one real `aws sso login` through `L` (the browser step completed on its own, e9s came back and loaded).
`aws-config` is built with `credentials-login` for `login_session` profiles.

## Known limits

- A 5 m or longer window on a busy service is cut, and the tail that follows starts behind and skips ahead.
- The tail follows the tasks running when the view opened; tasks started mid-rollout do not appear until the view is reopened.
- The services view lists STOPPED tasks for its `FAILED 1H` column, which costs a second or two per refresh.
- The `H` command log shows the short display form of each command; long ones are cut at the window edge.
- `:pulse` reads alarms, RDS, Lambda and the services of every cluster on each refresh (6.3 s measured on a real dev account, 2026-10-16).
- Lambda STATUS is read only for functions changed in the last 15 minutes (list-functions has no state); ERRORS and THROTTLES cost one GetMetricData per 250 functions.
- CloudTrail answers about 5 minutes after a call, allows 2 LookupEvents a second, and a `d` page caches its "Recent changes" for a minute.
- When a table does not fit, wide columns narrow to 24 and wrap, then columns drop from the right (k9s drops without narrowing). The dropped ones are only in `d` and `y`.

## Release

Steps: `.claude/skills/release/SKILL.md` (`/release`).
- Public at github.com/simion/e9s. CI (`.github/workflows/ci.yml`) runs tests on every push.
- A `vX.Y.Z` tag runs `.github/workflows/release.yml` (cargo-dist, config in `dist-workspace.toml`, regenerate with `dist generate`): binaries for macOS and Linux, a GitHub release, and `Formula/e9s.rb` pushed to github.com/simion/homebrew-tap with the `HOMEBREW_TAP_TOKEN` secret. About 25 minutes, mostly the macOS builds.
- crates.io: `cargo publish`, run by the release skill from the main checkout (the token is in `~/.cargo/credentials.toml`, set by `cargo login`).
- github.com/simion/homebrew-tap also holds a copy of Termic's cask; github.com/simion/homebrew-termic stays as it is.
- README images come from `E9S_DEMO=1` only, never from a real account.

## Open decisions

- **Region scan.** It counts ECS clusters, services and tasks per region; EC2, Lambda and RDS are not counted.
- **Copy in `H`.** Copying the full, runnable command of a selected call is not built.
- **k9s keys left out.** `ctrl-w` (wide columns: e9s has no narrow/wide column sets) and `Space` marks (nothing acts on several rows in a read-only tool).

## Ideas from taws (huseyinbabal/taws, MIT), not built yet

taws covers about 61 resource types, shallowly, and writes by default; its ideas were weighed against k9s habits on 2026-10-15.
- `0`-`9` as recent regions or clusters, like k9s favourite namespaces (conflicts with today's fixed quick views).
- `/-l key=value` tag filter: EC2 `--filters Name=tag:...`, client-side elsewhere.
- Shell completion for `-p` and `-r` (`e9s completion zsh`).
- A Docker image.
- Data-defined list screens for shallow resources (S3, DynamoDB, IAM), still through `Aws::call`.
Not to copy: write actions, decoding secrets, shells, Enter as describe, `R` refresh, `[` `]` paging, its own credential chain.

## Writing

No em dashes or en dashes anywhere (README, comments, commits): use a comma, colon, period or parentheses.
Markdown uses one sentence per line.
