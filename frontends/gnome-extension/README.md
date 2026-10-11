# OpenCUT GNOME Shell extension

Unofficial. Not affiliated with Anthropic. GPL-2.0-or-later.

A top-bar indicator for GNOME Shell 49/50 that shows your Claude plan usage,
for example `62% · 1h29m` (5-hour session percentage and the time until it
resets). It reads only the snapshot that the `ocutd` daemon writes to
`$XDG_RUNTIME_DIR/opencut/usage.json` (schema `opencut/usage@1`, see
`schema/usage.v1.json`). It makes no network requests and never reads
credentials.

## What it shows

- **Panel:** text from a template (default `{5h.pct}% · {5h.reset_in}`).
  The colour follows the highest window: warning at 75% or more, critical at
  90% or more (both configurable). `est.` appears when the 5-hour value is a
  local estimate. The indicator is dimmed when the snapshot is older than
  2 minutes or the status is not `ok`. It shows `—` when there is no snapshot.
- **Menu:** status and source badge, any errors from `ocutd`, bars for the
  5-hour, weekly (all models), weekly Opus and weekly Sonnet windows with
  reset countdowns (`n/a` when a window is unknown), the projection at
  reset, today's tokens, the cost estimate (an API-equivalent figure, not
  your bill), and the burn rate. Actions: **Refresh**, **Open dashboard**
  (shown only when `ocut tui` exists and a terminal is found), **Start
  ocutd** (shown only when the snapshot is missing) and **Settings**.
- **Settings:** panel template, position (left/center/right), show cost,
  warning and critical thresholds.

The panel template uses the same keys as `ocut --format`: `{5h|7d|opus|sonnet
.pct|reset_in|resets_at|est|proj|source}`, `{cost.5h|today|week}`,
`{tokens.5h|today|week}`, `{burn.tok_min|usd_h|pct_h}`, `{sessions}`,
`{status}`, `{plan}`. Add `:.N` for decimals (`{cost.today:.2}`) and use
`{{`/`}}` for literal braces. Unknown keys are shown as typed.

## Requirements

- GNOME Shell 49 or 50.
- `ocutd` running (see the top-level README; `packaging/systemd/opencut.service`).
- To build: `make`, `glib-compile-schemas`, `gnome-extensions` (ships with
  GNOME Shell), `unzip`. To test: Node 20+ and/or `gjs`.

## Build and test

```sh
make -C frontends/gnome-extension test    # lib.js unit tests under Node and GJS
make -C frontends/gnome-extension check   # parse every module, validate schema + metadata
make -C frontends/gnome-extension zip     # dist/opencut@opencut.github.io.shell-extension.zip
make -C frontends/gnome-extension smoke   # headless gnome-shell smoke test (see below)
```

## Install

```sh
make -C frontends/gnome-extension install
# same as: gnome-extensions install --force dist/opencut@opencut.github.io.shell-extension.zip
```

This copies the extension to
`~/.local/share/gnome-shell/extensions/opencut@opencut.github.io/`. It does
not enable it.

GNOME Shell on Wayland only discovers new extensions at login, so **log out
and back in**, then:

```sh
gnome-extensions enable opencut@opencut.github.io
```

(Or use the Extensions app.) Open the settings with
`gnome-extensions prefs opencut@opencut.github.io` or **Settings** in the menu.

If `ocutd` is not running yet:

```sh
systemctl --user enable --now opencut     # after installing the unit
```

The **Start ocutd** menu item runs `systemctl --user start opencut.service`.
If the unit is not installed it falls back to `ocutd` from `PATH`, run as a
transient user unit (`systemd-run --user --unit=opencut-adhoc`).

## Update

Run `make install` again, then log out and back in (Wayland cannot reload an
extension's code in place).

## Disable and uninstall

```sh
gnome-extensions disable opencut@opencut.github.io
make -C frontends/gnome-extension uninstall
# same as: gnome-extensions uninstall opencut@opencut.github.io
```

Settings are kept in dconf under `/org/gnome/shell/extensions/opencut/`;
remove them with `dconf reset -f /org/gnome/shell/extensions/opencut/`.

## Smoke test

`make smoke` (`tests/smoke/run-headless.sh`) starts a throwaway
`gnome-shell --headless --wayland --virtual-monitor` on a private
`dbus-run-session` bus with temporary `XDG_*` dirs and the keyfile GSettings
backend, so the live session and its dconf database are not touched. It
installs the zip into the temporary `XDG_DATA_HOME`, enables it together with
a small probe extension that logs what the indicator shows, swaps in the
fixture snapshots (ok, stale, estimated, truncated, hostile, deleted), opens
the preferences window, disables and re-enables the extension, and fails if
the extension is not ACTIVE or any JS error is logged.

## Design notes

- `lib.js` holds all parsing and formatting logic and has no `gi://`
  imports, so it is unit-tested under both Node and GJS.
- No sync IO in the shell process: the snapshot is read with
  `load_contents_async`, changes come from a `Gio.FileMonitor`
  (debounced 200 ms), and subprocesses use async `Gio.Subprocess`.
- A 30 s `GLib` timeout re-renders countdowns and staleness between
  snapshots. `disable()` destroys the indicator, which cancels pending IO,
  removes every timeout, cancels the file monitor, disconnects settings and
  destroys the tooltip actor.
