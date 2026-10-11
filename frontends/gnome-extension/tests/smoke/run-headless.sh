#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-or-later
#
# Headless smoke test: runs a throwaway gnome-shell on a private D-Bus session
# with temp XDG dirs and the keyfile GSettings backend, so nothing touches the
# live desktop session or its dconf database. Installs the zip into the temp
# XDG_DATA_HOME, enables it there, feeds it fixture snapshots and checks that
# the extension reaches state ACTIVE with no JS errors from it in the log.
#
# Usage: tests/smoke/run-headless.sh dist/opencut@opencut.github.io.shell-extension.zip
set -eu

ZIP=$(realpath "$1")
UUID=opencut@opencut.github.io
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../.." && pwd)
WAIT=${SMOKE_WAIT:-12}

T=$(mktemp -d "${TMPDIR:-/tmp}/opencut-smoke.XXXXXX")
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/data/gnome-shell/extensions/$UUID" "$T/config/glib-2.0/settings" \
         "$T/cache" "$T/state" "$T/run/opencut"
chmod 700 "$T/run" "$T/run/opencut"
(cd "$T/data/gnome-shell/extensions/$UUID" && unzip -q "$ZIP" && glib-compile-schemas schemas)

# Probe extension (temp dir only): logs what the OpenCUT indicator shows,
# because Shell.Eval is disabled outside unsafe mode.
P="$T/data/gnome-shell/extensions/opencut-probe@test"
mkdir -p "$P"
cat > "$P/metadata.json" <<'EOF'
{"uuid": "opencut-probe@test", "name": "probe", "description": "smoke probe", "shell-version": ["49", "50"]}
EOF
cat > "$P/extension.js" <<'EOF'
import GLib from 'gi://GLib';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
export default class Probe extends Extension {
    enable() {
        this._id = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 1000, () => {
            const i = Main.panel.statusArea['opencut@opencut.github.io'];
            if (!i) {
                console.log('OPENCUT_PROBE indicator=absent');
                return GLib.SOURCE_CONTINUE;
            }
            const items = i.menu._getMenuItems().filter(m => m.visible).length;
            console.log(`OPENCUT_PROBE text=${JSON.stringify(i._label.text)} class=${i._label.get_style_class_name()} ` +
                `opacity=${i.opacity} est=${i._est.visible} start=${i._startItem.visible} visibleItems=${items} ` +
                `status=${JSON.stringify(i._statusItem.label.text)}`);
            return GLib.SOURCE_CONTINUE;
        });
    }
    disable() { GLib.source_remove(this._id); }
}
EOF

cat > "$T/config/glib-2.0/settings/keyfile" <<EOF
[org/gnome/shell]
enabled-extensions=['$UUID', 'opencut-probe@test']
disable-user-extensions=false
disable-extension-version-validation=true
welcome-dialog-last-shown-version='999'
EOF

# Fresh copy of the example (generated_at = now) so the "not stale" path is shown.
sed "0,/\"generated_at\": \"[^\"]*\"/s//\"generated_at\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\"/" \
    "$REPO/tests/fixtures/snapshot/usage.v1.example.json" >"$T/run/opencut/usage.json"

cat > "$T/probe.sh" <<'EOF'
#!/bin/sh
# Runs inside the private bus once the shell is up.
uuid=$1; wait=$2; runtime=$3; fixtures=$4
i=0
while [ $i -lt "$wait" ]; do
    if gdbus call --session --dest org.gnome.Shell.Extensions \
        --object-path /org/gnome/Shell/Extensions \
        --method org.gnome.Shell.Extensions.GetExtensionInfo "$uuid" >"$runtime/info.txt" 2>/dev/null \
        && grep -q "'state': <" "$runtime/info.txt"; then
        break
    fi
    sleep 1; i=$((i + 1))
done
sleep 2  # let the probe log the initial (example fixture) state
# Open the Adw preferences window on the headless display; errors from
# prefs.js land in the session log (the prefs service is D-Bus activated).
WAYLAND_DISPLAY=wayland-0 gnome-extensions prefs "$uuid" >"$runtime/prefs.txt" 2>&1 || echo "prefs launch failed: $?" >>"$runtime/prefs.txt"
sleep 3
# Exercise the file monitor: atomic rename of other fixtures, a truncated
# file and a deletion. Errors would surface in the shell log.
for f in stale.json estimated.json truncated.json hostile.json; do
    cp "$fixtures/$f" "$runtime/opencut/.usage.tmp" && mv "$runtime/opencut/.usage.tmp" "$runtime/opencut/usage.json"
    sleep 1
done
rm -f "$runtime/opencut/usage.json"; sleep 1
# Disable + re-enable to exercise disable() cleanup.
gdbus call --session --dest org.gnome.Shell.Extensions --object-path /org/gnome/Shell/Extensions \
    --method org.gnome.Shell.Extensions.DisableExtension "$uuid" >/dev/null 2>&1 || true
sleep 1
gdbus call --session --dest org.gnome.Shell.Extensions --object-path /org/gnome/Shell/Extensions \
    --method org.gnome.Shell.Extensions.EnableExtension "$uuid" >/dev/null 2>&1 || true
sleep 2
gdbus call --session --dest org.gnome.Shell.Extensions --object-path /org/gnome/Shell/Extensions \
    --method org.gnome.Shell.Extensions.GetExtensionInfo "$uuid" >"$runtime/info2.txt" 2>&1 || true
EOF
chmod +x "$T/probe.sh"

cat > "$T/session.sh" <<EOF
#!/bin/sh
gnome-shell --headless --wayland --no-x11 --virtual-monitor 1280x800 >"$T/shell.log" 2>&1 &
pid=\$!
"$T/probe.sh" "$UUID" "$WAIT" "$T/run" "$HERE/../fixtures"
kill \$pid 2>/dev/null; wait \$pid 2>/dev/null || true
EOF
chmod +x "$T/session.sh"

env -i PATH="$PATH" HOME="$HOME" LANG="${LANG:-C.UTF-8}" \
    XDG_DATA_HOME="$T/data" XDG_CONFIG_HOME="$T/config" XDG_CACHE_HOME="$T/cache" \
    XDG_STATE_HOME="$T/state" XDG_RUNTIME_DIR="$T/run" \
    XDG_DATA_DIRS="${XDG_DATA_DIRS:-/usr/local/share:/usr/share}" \
    GSETTINGS_BACKEND=keyfile \
    dbus-run-session -- "$T/session.sh" >"$T/session.log" 2>&1 || true

echo "--- prefs window ---"
cat "$T/run/prefs.txt" 2>/dev/null
grep -o "Successfully activated service .org.gnome.Shell.Extensions." "$T/session.log" | head -n1 || echo "(prefs service was not activated)"
if grep -iE "prefs\.js|JS ERROR|Gjs-CRITICAL|TypeError|ReferenceError" "$T/session.log" "$T/shell.log"; then
    echo "prefs: errors above"; PREFS_OK=0
else
    echo "prefs: opened, no JS errors logged"; PREFS_OK=1
fi

echo "--- extension info (after start) ---"
cat "$T/run/info.txt" 2>/dev/null | tr ',' '\n' | grep -E "'(state|error|uuid)'" || echo "(no info: shell did not answer)"
echo "--- extension info (after disable+enable) ---"
cat "$T/run/info2.txt" 2>/dev/null | tr ',' '\n' | grep -E "'(state|error)'" || echo "(none)"
echo "--- probe (what the indicator showed, deduplicated) ---"
grep -o "OPENCUT_PROBE.*" "$T/shell.log" | sed 's/status="[^"]*updated [^"]*"/status=.../' | uniq || echo "(none)"
echo "--- shell log lines mentioning the extension or JS errors ---"
grep -iE "opencut|JS ERROR|Extension .* had error|Gjs-CRITICAL" "$T/shell.log" | grep -v OPENCUT_PROBE || echo "(none)"

# ExtensionState.ACTIVE == 1 (ENABLED on older shells).
if grep -q "'state': <1.0>\|'state': <1>" "$T/run/info.txt" 2>/dev/null \
   && grep -q "OPENCUT_PROBE text=" "$T/shell.log" && [ "$PREFS_OK" = 1 ] \
   && ! grep -v OPENCUT_PROBE "$T/shell.log" | grep -iE "JS ERROR|opencut.*(error|exception)" >/dev/null; then
    echo "SMOKE: PASS"
else
    echo "SMOKE: FAIL (full log below)"; tail -n 60 "$T/shell.log"
    exit 1
fi
