# OpenCUT: Open Claude Usage Tracker

Unofficial and not affiliated with Anthropic. OpenCUT shows your Claude plan usage on the Linux desktop: the 5-hour session and weekly limits, reset countdowns, tokens, and a cost estimate. It can display this in the GNOME top bar, a tray icon, status bars, a terminal dashboard, and other places.

Status: **M0** (core, CLI and daemon). Desktop widgets arrive in M1. See [PLAN.md](PLAN.md).

## Install (from source)

Requires Rust 1.88 or newer.

```sh
cargo install --path crates/ocut-cli   # installs `ocut`
cargo install --path crates/ocutd      # installs `ocutd`
```

Run the daemon as a systemd user service (hardened, with read-only access to `~/.claude`):

```sh
mkdir -p ~/.config/systemd/user
cp packaging/systemd/opencut.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now opencut
```

`ocutd` writes `$XDG_RUNTIME_DIR/opencut/usage.json` (directory 0700, file 0600) every 30 s. The format is the JSON Schema in [`schema/usage.v1.json`](schema/usage.v1.json).

## Usage

```sh
ocut                                    # one line, using display.format from the config
ocut --format '{5h.pct:.0}% ↻{5h.reset_in}'
ocut --json                             # full snapshot
ocut --waybar                           # {"text", "tooltip", "class": ok|warn|crit|stale…}
ocut --no-daemon --json                 # compute in-process instead of reading ocutd's snapshot
ocut doctor                             # file modes, source health, Claude Code version
```

Without `--no-daemon`, `ocut` reads the daemon's snapshot if it is less than 90 s old. Otherwise it computes one in-process. Only `ocutd`, or an explicit `ocut --no-daemon`, ever calls the OAuth endpoint, so status bars that run `ocut` every few seconds never add API requests.

Format keys: `5h`, `7d`, `opus` and `sonnet`, each with `.pct`, `.reset_in`, `.resets_at`, `.source`, `.est` (`~` for an estimate) and `.proj`. Also `cost.{5h,today,week}`, `tokens.{5h,today,week}`, `burn.{tok_min,usd_h,pct_h}`, `sessions`, `status` and `plan`. Add `:.N` to set the number of decimals. Write `{{` and `}}` for literal braces. A value that isn't known prints `n/a`.

### Waybar

```json
"custom/claude": {
  "exec": "ocut --waybar",
  "return-type": "json",
  "interval": 5
}
```

Style it with `#custom-claude.warn` (75% or more) and `#custom-claude.crit` (90% or more).

### Claude Code statusline (optional source B)

```sh
ocut install statusline          # prints the snippet and a diff; writes nothing
ocut install statusline --apply  # backs up ~/.claude/settings.json, then adds statusLine
```

`--apply` never overwrites an existing `statusLine`. The hook (`ocut hook statusline`) caches only the rate limits, the model, the session cost and the Claude Code version, in `~/.cache/opencut/statusline.json`. It then prints one coloured line. It never stores the session id, the working directory or the transcript path.

## Data sources

| Source | What it gives | Default |
|---|---|---|
| A. OAuth usage endpoint (`api.anthropic.com/api/oauth/usage`) | Real 5h and weekly %, reset times | **Off until you agree.** `ocut` asks once in a terminal. Without a terminal, the answer counts as no. |
| B. Statusline hook | 5h and 7d % while Claude Code runs | On, if you install the hook |
| C. `~/.claude/projects/**/*.jsonl` | Tokens, cost estimate, burn rate, 5h estimate | Always on, works offline |

Limits come from A, then from B if it is less than 10 minutes old, then from C as an estimate (marked `estimated: true` and `~`). The weekly limit can't be estimated locally, so without A or B it shows `n/a`. Costs are API-equivalent estimates, not what a flat-rate plan bills you.

## Configuration

`~/.config/opencut/config.toml`. Every key is optional, and the daemon picks up changes without a restart.

```toml
[sources]
oauth_api = "off"        # "ask" (default), "on", "off"
local_plan = "pro"       # pro | max_5x | max_20x: scale for the local 5h estimate

[polling]
oauth_interval_s = 120   # 60 while active, 600 when idle; never under 60
snapshot_interval_s = 30 # read at startup

[notify]
enabled = true
thresholds = [75, 90, 100]
hysteresis = 5
reset_notice = true

[display]
format = "{5h.pct:.0}% ↻{5h.reset_in}"
```

Notification state is kept in `~/.cache/opencut/notify-state.json`, so a restart never repeats a notification.

## Privacy and security

- OpenCUT never reads message content from the logs, only the usage numbers.
- From `~/.claude/.credentials.json` it reads only the `claudeAiOauth` access token, and only when `oauth_api = "on"`. It refuses the file if the mode is wider than 0600 or another user owns it. The token is sent only to `api.anthropic.com` over rustls with redirects disabled. It is never stored, logged or refreshed. If it has expired, run `claude` once.
- `ocut doctor` prints paths, modes, counts and ages, never tokens or settings values.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests use only the sanitized fixtures in `tests/fixtures/`.

## License

MIT OR Apache-2.0.
