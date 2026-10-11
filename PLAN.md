# OpenCUT (Open Claude Usage Tracker): Implementation Plan

**Target machine:** Ubuntu 26.04, GNOME Shell 50.1 on Wayland, Claude Code 2.1.296, Pro plan. Installed tools: cargo, python3 with PyGObject, GTK 4.22, libadwaita 1.9, gjs and node 24. The AppIndicator extension is enabled. Project directory: local checkout (empty at start).

## 1. Decisions (conflicts resolved)

| Topic | Options in the reports | **Decision** | Why |
|---|---|---|---|
| Name | `claude-usage`, `claude-meter`, `ocut` | **OpenCUT**: CLI `ocut`, daemon `ocutd`, D-Bus `io.github.opencut.Usage1` | Matches what you asked for and the folder name, and keeps "Claude" out of binary names (trademark). Can be renamed (Q1). |
| Core language | Rust or Python | **Rust** (Cargo workspace) | Status bars call the CLI every 1–5 s: ~2 ms cold start vs ~80 ms. A single static binary. Mature `zbus`, `notify` and `ksni` crates. cargo is already installed. |
| Widget languages | — | Each widget uses its desktop's own language: GJS (GNOME), QML (KDE), shell/JSON snippets for bars | Widgets only read the snapshot and never link the core. |
| Contract | — | **JSON Schema `schema/usage.v1.json`**, additive-only | The schema comes first; each widget is tested against fixture JSON. |
| Snapshot path | `state.json` / `usage.json` | `$XDG_RUNTIME_DIR/opencut/usage.json` (dir 0700, file 0600, written by atomic rename) | |
| Transport order | file / D-Bus / socket | **M0–M1: file plus Gio file monitor. M2: D-Bus `Changed` signal.** Unix socket deferred. | The file alone works for every widget, and D-Bus is an optimisation. |
| OAuth poll | 60–600 s | Default 120 s, never under 60 s. 60 s while JSONL shows activity, 600 s after 15 min idle. Exponential backoff with jitter on 429/5xx, honouring Retry-After. | Only the daemon calls the API, so 10 widgets still make 1 request. |
| Notify thresholds | 50/75/90/100 or 80/95 | Default **75 / 90 / 100**, plus "5h window reset". Configurable, with 5-point hysteresis and the sent state persisted. | |
| Desktop widget on GNOME | gtk4-layer-shell | **Not possible on Mutter.** On GNOME the "on-desktop" view is drawn by the extension. Layer-shell is only for KDE and wlroots (M2+). | Mutter has no wlr-layer-shell. |
| License | — | Core: MIT OR Apache-2.0. GNOME extension: GPL-2.0-or-later. Plasmoid: GPL-2.0+. DCO, no CLA. | |

## 2. Data sources and merge rule

| Pri | Source | Provides | Default |
|---|---|---|---|
| A | `GET https://api.anthropic.com/api/oauth/usage`, sending `Authorization: Bearer <claudeAiOauth.accessToken>` and `anthropic-beta: oauth-2025-04-20` | Real 5h and weekly % and `resets_at`, plus opus/sonnet scoped limits and extra usage | **Opt-in at first run** (Q2) |
| B | Statusline tee: `ocut hook statusline` reads stdin `rate_limits` and writes `$XDG_CACHE_HOME/opencut/statusline.json` | 5h and 7d % while Claude Code is running. Official behaviour, no token needed. | On, if the user installs the hook |
| C | `~/.claude/projects/**/*.jsonl` (plus `$CLAUDE_CONFIG_DIR` and `~/.config/claude/projects`) | Tokens, cost estimate, burn rate, 5h block estimate | Always on, works offline |
| — | `~/.claude/sessions/*.json` | "N active sessions" indicator | On |
| Docs only | claude.ai `sessionKey` cookie, `anthropic-ratelimit-unified-*` headers, Admin API | Not used in v1. The Admin API may be added later for API-key users. | Off |

**Merge rule:**
- Limit % and `resets_at` come from A, then B (if under 10 min old), then C as an estimate. An estimate always sets `estimated: true`.
- Tokens, cost, per-model figures and burn rate always come from C.
- Every window records its `source`, `fetched_at` and `stale`.

**Parsing rules:**
- Accept both OAuth response shapes: the flat `five_hour` / `seven_day*` keys, and the 2026 `limits[]` array of `{kind: session|weekly_all|weekly_scoped, percent, resets_at}`. `resets_at: null` means no window is open.
- `resets_at` can be epoch seconds or an ISO string. Normalise it to RFC 3339 UTC.
- JSONL: keep `type=="assistant"` lines that have `message.usage`. Dedupe on `message.id + requestId`. Use `cost-state` lines when present. Ignore unknown keys (`serde(default)`).
- Never read message content.
- 5h estimate (ccusage blocks): a block starts at the first message's timestamp floored to the hour and lasts 5 h. A new block starts after a gap of 5 h or more. The weekly figure can't be estimated, so show "n/a" without A or B.

**Credential rules:**
- Read only the `claudeAiOauth` subtree. Refuse if the file mode is wider than 0600 or the owner isn't you.
- Keep the token in memory only. Send it to `api.anthropic.com` only, over rustls.
- **Never refresh the token.** If `expiresAt` (epoch ms) has passed, or the API returns 401, set status `token_expired` and tell the user to "run `claude` once".
- Never log or print the token, email, prompts or the `mcpOAuth.*` entries.

## 3. Snapshot schema v1 (contract)

```json
{
  "schema": "opencut/usage@1",
  "generated_at": "2026-10-11T12:30:05Z",
  "status": "ok|stale|degraded|offline|token_expired|error",
  "account": { "plan": "pro|max_5x|max_20x|team|api|unknown" },
  "windows": {
    "session_5h":    { "pct": 62.0, "resets_at": "…", "seconds_to_reset": 5395,
                       "source": "oauth_api|statusline|local_estimate", "estimated": false,
                       "fetched_at": "…", "stale": false,
                       "projection": { "pct_at_reset": 91.4, "hits_limit_at": null, "confidence": "low|med|high" } },
    "weekly_all":    { "pct": 34.0, "resets_at": "…", "source": "oauth_api", "estimated": false },
    "weekly_opus":   { "pct": 12.0, "resets_at": "…", "source": "oauth_api" },
    "weekly_sonnet": null,
    "extra_usage":   { "enabled": false, "used_usd": 0, "limit_usd": null }
  },
  "tokens": { "session_5h": {"input":0,"output":0,"cache_write":0,"cache_read":0,"total":0,"messages":0},
              "today": {}, "week": {} },
  "by_model": [ { "model": "claude-opus-5-5", "family": "opus", "total": 0, "est_cost_usd": 0.0, "share_pct": 0.0 } ],
  "cost": { "session_5h_usd": 0.0, "today_usd": 0.0, "week_usd": 0.0,
            "pricing": "builtin@2026-10-01", "note": "API-equivalent estimate; plan is flat-rate" },
  "burn_rate": { "tokens_per_min": 0.0, "usd_per_hour": 0.0, "pct_per_hour": 0.0, "window_min": 15 },
  "active_sessions": 1,
  "sources": { "oauth_api": {"enabled": true, "ok": true, "last_success": "…", "next_poll_at": "…", "backoff_s": 0},
               "statusline": {"ok": false, "last_push": null},
               "local_logs": {"ok": true, "files": 7, "parse_errors": 0} },
  "errors": [ { "code": "token_expired", "message": "…", "at": "…" } ]
}
```

- **Projection:** an EWMA of pct/hour over the last 15–30 minutes, extrapolated to `resets_at`.
- **Privacy:** project names are redacted by default.

## 4. Repo layout

```
OpenCUT/
  Cargo.toml                         # workspace
  crates/
    ocut-core/      # types (serde, matches the schema), merge, windows/blocks, pricing, projection; pure, no IO
    ocut-sources/   # oauth.rs, jsonl.rs (inotify tail + offsets), statusline.rs, sessions.rs, credentials.rs
    ocut-cli/       # bin `ocut`: --json, --format, --waybar, watch, tui, doctor, hook statusline, install-*
    ocutd/          # bin `ocutd`: scheduler, snapshot writer, notifier, (M2) zbus service
    ocut-tray/      # bin `ocut-tray`: ksni StatusNotifierItem
  schema/usage.v1.json
  pricing/models.json                # bundled snapshot of LiteLLM prices, overridable in config
  frontends/
    gnome-extension/opencut@opencut.github.io/   # GJS ESM, GPL-2.0+
    gtk-dashboard/                   # Python PyGObject + libadwaita details window
    plasma-plasmoid/  waybar/  polybar/  i3blocks/  tmux/  starship/  conky/
  packaging/{systemd,dbus,deb,aur,rpm,flatpak,nix}/
  tests/fixtures/{jsonl,oauth}/      # sanitized fixtures plus a canary token
  docs/  README.md  SECURITY.md  LICENSE-MIT  LICENSE-APACHE
```

Config lives at `~/.config/opencut/config.toml` with sections `[sources]`, `[polling]`, `[notify]`, `[pricing]`, `[display]` and `[privacy]`. Cache lives at `~/.cache/opencut/`, holding `offsets.json`, `statusline.json`, `notify-state.json` and, from M2, `history.db`.

## 5. Milestones

### M0: core and CLI (about 2–3 days)
| Task | Acceptance criteria |
|---|---|
| Schema plus `ocut-core` types and merge logic | `insta` snapshots from fixtures validate against `usage.v1.json` in CI |
| JSONL source: discovery, incremental tail via offsets, dedupe, truncate/rotate handling | Totals match `ccusage` within 1% on the local `~/.claude`. Re-runs parse only new bytes. A fuzzed line parser never panics. |
| Pricing table plus `cost-state` preference | Per-model cost is correct for input, output, cache write (5m/1h) and cache read |
| 5h block estimate plus burn rate and projection | Unit and `proptest` tests: pct in 0..=100, reset in the future, DST and timezone cases |
| Credentials reader plus OAuth source (both shapes, backoff, 401 leads to `token_expired`) | `wiremock` covers 200 (old and new shape), 401, 429 with Retry-After and unknown fields. A canary token never appears in any output, log or panic. |
| `ocut --json`, `--format '{5h.pct:.0}% ↻{5h.reset_in}'`, `--waybar`, `doctor` | A cached call runs in under 5 ms (`hyperfine`). `--no-daemon` works. `doctor` reports file modes, source health and the Claude Code version. |
| `ocut hook statusline`: tee stdin to the cache and print one ANSI line | Returns in under 50 ms, and still works with `rate_limits` missing |
| `ocutd` minimum: scheduler plus atomic snapshot writer plus `opencut.service` (systemd --user, hardened) | `systemctl --user start opencut` writes the snapshot every 30 s. The service has read-only access to `~/.claude`. |

### M1: first widgets on this machine (about 3–4 days)
| Surface | Tasks | Acceptance criteria |
|---|---|---|
| **GNOME Shell extension** (main) | `PanelMenu.Button` showing "62% · 1h29m" with a colour by threshold. Popup with 5h, weekly and opus/sonnet bars, reset countdowns, today's tokens and cost, source/stale badge, and an "Open dashboard" item. `Gio.FileMonitor` on the snapshot. Adw `prefs.js` for format and thresholds. | No sync IO or network calls in the shell process. `disable()` frees everything. Tested in `dbus-run-session gnome-shell --devkit`. ESLint passes. The panel updates within 2 s of a snapshot change. |
| **Notifications** (in `ocutd`) | Threshold crossings, reset, token expired | No duplicates across restarts. 90% and above uses critical urgency. Can be fully disabled. |
| **Statusline install** | `ocut install statusline` | Prints the snippet and a diff. It writes `~/.claude/settings.json` only with `--apply`, makes a backup, and **never overwrites an existing `statusLine`**. |
| **Tray** (`ocut-tray`) | `ksni` icon showing a ring for 5h % and a text menu | Visible via ubuntu-appindicators. Works under KDE and Waybar's tray in a container. |

### M2: more surfaces (about 4–6 days)
| Surface | Acceptance criteria |
|---|---|
| D-Bus `io.github.opencut.Usage1` (`Snapshot` property plus `Changed` signal); the GNOME extension switches to `Gio.DBusProxy` | A `busctl --user` integration test passes under `dbus-run-session` |
| TUI `ocut tui` (ratatui): gauges, burn sparkline, per-model and per-project table, 5h blocks | Works over SSH at 80x24. `q` exits cleanly. |
| GTK4/libadwaita dashboard (`frontends/gtk-dashboard`), opened from the extension or tray | Reads only the snapshot. Docs explain the right-click "Always on Top" workaround on GNOME. |
| Waybar / Polybar / i3blocks / tmux / starship snippets | Each is under 15 lines, uses `--waybar` (warn/crit classes) or `--format`, and is tested in a container |
| KDE Plasma 6 plasmoid (panel and desktop), plus gtk4-layer-shell widget for KDE/wlroots | `qmllint` passes. Smoke-tested in a KDE Neon VM. |
| Conky example; `history.db` rollups | Conky is labelled "X11/XWayland, best-effort" |

### M3: packaging and release (about 2–3 days)
| Task | Acceptance criteria |
|---|---|
| `cargo-dist`: musl tarballs, `.deb` (cargo-deb with the user unit and D-Bus service), `.rpm`, AUR `opencut-bin`, Nix flake | `apt install ./opencut.deb && systemctl --user enable --now opencut` works on a clean Ubuntu 26.04 VM |
| GNOME extension zip, then submission to extensions.gnome.org | A manual zip install works while review is pending. Plan a `shell-version` bump every 6 months. |
| KDE Store plasmoid; Flatpak for the GTK dashboard (`--filesystem=~/.claude:ro`) | |
| CI: build, test, clippy, `cargo deny`, schema validation, privacy canary, hyperfine gate | Release is driven by tags |
| README with "Unofficial, not affiliated with Anthropic", data-source and privacy disclosure, and how to run offline (`oauth_api=false`) | |

## 6. Risks

1. **The OAuth endpoint is undocumented.** It can change or be locked down. Mitigations: tolerant parsing of both shapes, falling back to the statusline and then to the local estimate, and a schema-drift fixture in CI.
2. **Terms of service.** I found nothing confirming that third-party read-only use of the consumer OAuth token is allowed. That is why A is opt-in and the tool works fully without it.
3. **Credential handling.** A bug could leak the token. Mitigations: the canary test, reading only the subtree, never refreshing the token (refreshing could log out the CLI), and pinning the host.
4. **Local logs only cover Claude Code on this machine.** Web, desktop and mobile usage is visible only through A. The weekly figure can't be estimated locally. Estimated values are always labelled.
5. **GNOME** changes its extension API every release, and EGO review takes weeks. There are no true desktop widgets on Mutter.
6. **The JSONL format changes often** (many new keys in 2.1.x). Mitigation: parse defensively and fuzz.
7. **Cost figures are API-equivalent estimates**, not what you are billed on the flat-rate plan.

## 7. Findings to pass on to the user

- The research flagged a step that edits `~/.claude/settings.json` (the statusline hook). The plan makes it **opt-in**: preview first, apply only with `--apply`, back up the file, and never replace an existing `statusLine`. Nothing in this plan changes your Claude Code config by itself.
- One source the research cited is a third-party mirror of unknown trust (`download.ai.regdesk.co/...provider-usage.fetch.claude.ts`). Check the endpoint details against CodexBar `docs/claude.md` and ccusage instead.
- `~/.claude/metrics/costs.jsonl` and `cost-tracker.log` come from the ECC plugin, not from Claude Code. The plan does not depend on them.

## 8. Decisions (confirmed 2026-10-11)

1. **Name:** OpenCUT. The CLI is `ocut` and the daemon is `ocutd`.
2. **OAuth usage endpoint:** off until the user agrees. `ocut` asks once, on first run.
3. **First widget:** GNOME top-bar extension (M1).
4. **Language:** Rust only. No Python port of the core.
5. **Notifications:** 75 / 90 / 100%, plus a 5h-reset notice.
6. **Publishing:** public GitHub repo from day one. Submit to extensions.gnome.org at M3.
7. **Plan presets:** Pro, Max 5x and Max 20x for the local estimate. Auto-calibrate from API or statusline readings when they're available.
8. **Offline 5h estimate:** keep the Pro, Max 5x and Max 20x presets, and auto-calibrate from API or statusline readings. Mark the value as estimated until then.
9. **Per-model breakdown:** covers a rolling 7 days.
10. **OAuth calls:** made by `ocutd` only. `ocut` never calls the endpoint, even with `--no-daemon`.
