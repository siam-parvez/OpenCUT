// SPDX-License-Identifier: GPL-2.0-or-later
//
// OpenCUT top-bar indicator. Reads only the ocutd snapshot at
// $XDG_RUNTIME_DIR/opencut/usage.json: no network, no credentials, no sync IO.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

import * as Lib from './lib.js';

Gio._promisify(Gio.File.prototype, 'load_contents_async');
Gio._promisify(Gio.File.prototype, 'query_info_async');
Gio._promisify(Gio.Subprocess.prototype, 'communicate_utf8_async');

const TICK_S = 30;
const DEBOUNCE_MS = 200;
const BAR_WIDTH = 260;
const CLASSES = ['opencut-ok', 'opencut-warn', 'opencut-crit'];

function setClass(actor, cls) {
    for (const c of CLASSES)
        actor.remove_style_class_name(c);
    actor.add_style_class_name(cls);
}

function isCancelled(e) {
    return e instanceof GLib.Error && e.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED);
}

/** A popup row: title, percentage, bar, reset countdown and projection. */
const WindowItem = GObject.registerClass(
class WindowItem extends PopupMenu.PopupBaseMenuItem {
    _init(title) {
        super._init({reactive: false, can_focus: false, style_class: 'opencut-window-item'});
        const box = new St.BoxLayout({orientation: Clutter.Orientation.VERTICAL, x_expand: true});
        this.add_child(box);

        const header = new St.BoxLayout({x_expand: true});
        header.add_child(new St.Label({text: title, x_expand: true, style_class: 'opencut-window-title'}));
        this._pct = new St.Label({text: Lib.NA, style_class: 'opencut-window-pct'});
        header.add_child(this._pct);
        box.add_child(header);

        this._track = new St.Widget({style_class: 'opencut-bar-track', style: `width: ${BAR_WIDTH}px;`});
        this._fill = new St.Widget({style_class: 'opencut-bar-fill opencut-ok'});
        this._track.add_child(this._fill);
        box.add_child(this._track);

        this._reset = new St.Label({text: Lib.NA, style_class: 'opencut-window-sub'});
        box.add_child(this._reset);
        this._proj = new St.Label({text: '', style_class: 'opencut-window-sub', visible: false});
        box.add_child(this._proj);
    }

    update(row) {
        this._pct.text = row.pctText;
        setClass(this._pct, row.styleClass);
        setClass(this._fill, row.styleClass);
        this._fill.set_width(Math.round(BAR_WIDTH * row.pct / 100));
        this._fill.visible = row.known && row.pct > 0;
        this._reset.text = row.known ? `${row.resetText}${row.estimated ? ' · estimated' : ''}` : Lib.NA;
        this._proj.text = row.projText;
        this._proj.visible = row.projText !== '';
    }
});

const Indicator = GObject.registerClass(
class Indicator extends PanelMenu.Button {
    _init(ext) {
        super._init(0.5, 'OpenCUT');
        this._ext = ext;
        this._settings = ext.getSettings();
        this._state = {kind: 'missing'};
        this._cancellable = new Gio.Cancellable();
        this._loading = false;
        this._reloadQueued = false;
        this._debounceId = 0;
        this._tickId = 0;
        this._dashboardCmd = null;
        this._tooltipText = '';

        // Panel button.
        const box = new St.BoxLayout({style_class: 'panel-status-menu-box opencut-panel'});
        this._icon = new St.Icon({
            gicon: Gio.icon_new_for_string(`${ext.path}/icons/opencut-symbolic.svg`),
            style_class: 'system-status-icon opencut-icon',
        });
        this._label = new St.Label({text: Lib.MISSING_TEXT, y_align: Clutter.ActorAlign.CENTER, style_class: 'opencut-label'});
        this._est = new St.Label({text: 'est.', y_align: Clutter.ActorAlign.CENTER, style_class: 'opencut-est', visible: false});
        box.add_child(this._icon);
        box.add_child(this._label);
        box.add_child(this._est);
        this.add_child(box);

        this._tooltip = new St.Label({style_class: 'opencut-tooltip', visible: false});
        Main.uiGroup.add_child(this._tooltip);
        this.connect('notify::hover', () => this._syncTooltip());

        this._buildMenu();
        this.menu.connect('open-state-changed', (_m, open) => {
            if (open)
                this._reload();
            this._syncTooltip();
        });

        this._settings.connectObject(
            'changed::panel-format', () => this._render(),
            'changed::show-cost', () => this._render(),
            'changed::warn-threshold', () => this._render(),
            'changed::crit-threshold', () => this._render(),
            this);

        // Snapshot file + monitor. The monitor also reports the atomic rename.
        const dir = GLib.build_filenamev([GLib.get_user_runtime_dir(), 'opencut']);
        this._file = Gio.File.new_for_path(GLib.build_filenamev([dir, 'usage.json']));
        this._monitor = this._file.monitor_file(Gio.FileMonitorFlags.WATCH_MOVES, null);
        this._monitor.connectObject('changed', (_m, _f, _o, event) => {
            if (event !== Gio.FileMonitorEvent.ATTRIBUTE_CHANGED)
                this._queueReload();
        }, this);

        this._tickId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, TICK_S, () => {
            this._render();
            return GLib.SOURCE_CONTINUE;
        });

        this._detectDashboard();
        this._render();
        this._reload();
    }

    _buildMenu() {
        this._statusItem = new PopupMenu.PopupMenuItem('', {reactive: false, style_class: 'opencut-status'});
        this.menu.addMenuItem(this._statusItem);
        this._errorsItem = new PopupMenu.PopupMenuItem('', {reactive: false, style_class: 'opencut-errors'});
        this._errorsItem.label.clutter_text.line_wrap = true;
        this.menu.addMenuItem(this._errorsItem);

        this._windowsSection = new PopupMenu.PopupMenuSection();
        this._windowItems = {};
        for (const [key, title] of Lib.WINDOWS) {
            const item = new WindowItem(title);
            this._windowItems[key] = item;
            this._windowsSection.addMenuItem(item);
        }
        this.menu.addMenuItem(this._windowsSection);

        this._detailsSep = new PopupMenu.PopupSeparatorMenuItem();
        this.menu.addMenuItem(this._detailsSep);
        this._todayItem = new PopupMenu.PopupMenuItem('', {reactive: false});
        this._costItem = new PopupMenu.PopupMenuItem('', {reactive: false});
        this._costNoteItem = new PopupMenu.PopupMenuItem('', {reactive: false, style_class: 'opencut-note'});
        this._burnItem = new PopupMenu.PopupMenuItem('', {reactive: false});
        for (const i of [this._todayItem, this._costItem, this._costNoteItem, this._burnItem])
            this.menu.addMenuItem(i);

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._startItem = this.menu.addAction('Start ocutd', () => this._startDaemon());
        this._dashboardItem = this.menu.addAction('Open dashboard', () => this._openDashboard());
        this._dashboardItem.visible = false;
        this.menu.addAction('Refresh', () => this._reload());
        this.menu.addAction('Settings', () => this._ext.openPreferences());
    }

    _queueReload() {
        if (this._debounceId)
            return;
        this._debounceId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, DEBOUNCE_MS, () => {
            this._debounceId = 0;
            this._reload();
            return GLib.SOURCE_REMOVE;
        });
    }

    async _reload() {
        if (this._loading) {
            this._reloadQueued = true;
            return;
        }
        this._loading = true;
        try {
            // Refuse an oversized file before reading it into the shell.
            const info = await this._file.query_info_async(Gio.FILE_ATTRIBUTE_STANDARD_SIZE,
                Gio.FileQueryInfoFlags.NONE, GLib.PRIORITY_DEFAULT, this._cancellable);
            if (info.get_size() > Lib.MAX_BYTES)
                throw new Error('snapshot too large');
            const [contents] = await this._file.load_contents_async(this._cancellable);
            if (contents.length > Lib.MAX_BYTES)
                throw new Error('snapshot too large');
            const res = Lib.parseSnapshot(new TextDecoder().decode(contents));
            this._state = res.ok ? {kind: 'ok', snap: res.snap} : {kind: 'invalid', error: res.error};
        } catch (e) {
            if (isCancelled(e))
                return;
            if (e instanceof GLib.Error && e.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.NOT_FOUND))
                this._state = {kind: 'missing'};
            else
                this._state = {kind: 'invalid', error: Lib.clean(e.message)};
        } finally {
            this._loading = false;
        }
        if (this._cancellable.is_cancelled())
            return;
        this._render();
        if (this._reloadQueued) {
            this._reloadQueued = false;
            this._reload();
        }
    }

    _opts() {
        return {
            format: this._settings.get_string('panel-format') || Lib.DEFAULT_FORMAT,
            warn: this._settings.get_int('warn-threshold'),
            crit: this._settings.get_int('crit-threshold'),
        };
    }

    _render() {
        const now = Date.now();
        const opts = this._opts();
        const view = Lib.panelView(this._state, opts, now);
        this._label.text = view.text;
        setClass(this._label, view.styleClass);
        this._est.visible = view.estimated;
        this.opacity = view.dim ? 140 : 255;
        this._tooltipText = view.tooltip;
        this._tooltip.text = view.tooltip;
        this.accessible_name = `OpenCUT ${view.text}. ${view.tooltip}`;

        const missing = this._state.kind === 'missing';
        this._startItem.visible = missing;
        const snap = this._state.kind === 'ok' ? this._state.snap : null;

        if (!snap) {
            this._statusItem.label.text = missing
                ? 'ocutd not running: no snapshot'
                : `Snapshot unreadable: ${this._state.error}`;
            this._errorsItem.visible = false;
            this._windowsSection.actor.visible = false;
            for (const i of [this._detailsSep, this._todayItem, this._costItem, this._costNoteItem, this._burnItem])
                i.visible = false;
            return;
        }

        this._statusItem.label.text = Lib.badgeText(snap, now);
        setClass(this._statusItem.label, snap.status === 'ok' ? 'opencut-ok'
            : snap.status === 'token_expired' || snap.status === 'error' ? 'opencut-crit' : 'opencut-warn');
        this._errorsItem.visible = snap.errors.length > 0;
        this._errorsItem.label.text = Lib.errorText(snap);

        this._windowsSection.actor.visible = true;
        for (const [key] of Lib.WINDOWS)
            this._windowItems[key].update(Lib.windowRow(snap, key, now, opts.warn, opts.crit));

        const showCost = this._settings.get_boolean('show-cost');
        const lines = Lib.detailLines(snap, showCost);
        this._detailsSep.visible = true;
        this._todayItem.visible = true;
        this._todayItem.label.text = lines.today;
        this._costItem.visible = showCost;
        this._costItem.label.text = lines.cost ?? '';
        this._costNoteItem.visible = showCost;
        this._costNoteItem.label.text = snap.cost.note || 'API-equivalent estimate, not your bill';
        this._burnItem.visible = true;
        this._burnItem.label.text = lines.burn;
    }

    _syncTooltip() {
        const show = this.hover && !this.menu.isOpen && this._tooltipText !== '';
        if (!show) {
            this._tooltip.hide();
            return;
        }
        const [x, y] = this.get_transformed_position();
        const [w, h] = this.get_transformed_size();
        this._tooltip.show();
        const [tw] = this._tooltip.get_size();
        const monitor = Main.layoutManager.findMonitorForActor(this);
        let tx = Math.round(x + w / 2 - tw / 2);
        if (monitor)
            tx = Math.max(monitor.x, Math.min(tx, monitor.x + monitor.width - tw));
        this._tooltip.set_position(tx, Math.round(y + h + 4));
    }

    async _run(argv) {
        const proc = Gio.Subprocess.new(argv,
            Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE);
        const [stdout, stderr] = await proc.communicate_utf8_async(null, this._cancellable);
        return {ok: proc.get_successful(), stdout: stdout ?? '', stderr: stderr ?? ''};
    }

    /**
     * Starts the daemon without blocking the shell: the systemd user unit
     * first, then (if the unit is not installed) `ocutd` from PATH as a
     * transient user unit so it is not a child of gnome-shell.
     */
    async _startDaemon() {
        try {
            let r = await this._run(['systemctl', '--user', 'start', 'opencut.service']);
            if (this._cancellable.is_cancelled())
                return;
            if (!r.ok) {
                const unitErr = r.stderr.trim();
                r = await this._run(['sh', '-c',
                    'p=$(command -v ocutd) || { echo "ocutd not found in PATH" >&2; exit 127; }\n' +
                    'exec systemd-run --user --collect --quiet --unit=opencut-adhoc "$p"']);
                if (this._cancellable.is_cancelled())
                    return;
                if (!r.ok) {
                    Main.notify('OpenCUT', `Could not start ocutd: ${r.stderr.trim() || unitErr || 'unknown error'}`);
                    return;
                }
            }
            this._queueReload();
        } catch (e) {
            if (!isCancelled(e))
                Main.notify('OpenCUT', `Could not start ocutd: ${e.message}`);
        }
    }

    /** Shows "Open dashboard" only when `ocut tui` exists and a terminal is found. */
    async _detectDashboard() {
        const script = [
            'command -v ocut >/dev/null 2>&1 || exit 1',
            'ocut --help 2>/dev/null | grep -qw tui || exit 1',
            'for t in xdg-terminal-exec ptyxis kgx gnome-terminal x-terminal-emulator; do',
            '  command -v "$t" >/dev/null 2>&1 && { echo "$t"; exit 0; }',
            'done',
            'exit 1',
        ].join('\n');
        try {
            const r = await this._run(['sh', '-c', script]);
            if (this._cancellable.is_cancelled())
                return;
            const term = r.stdout.trim();
            if (!r.ok || !term)
                return;
            const argv = {
                'xdg-terminal-exec': ['xdg-terminal-exec', 'ocut', 'tui'],
                'ptyxis': ['ptyxis', '--', 'ocut', 'tui'],
                'kgx': ['kgx', '--', 'ocut', 'tui'],
                'gnome-terminal': ['gnome-terminal', '--', 'ocut', 'tui'],
                'x-terminal-emulator': ['x-terminal-emulator', '-e', 'ocut', 'tui'],
            }[term];
            if (!argv)
                return;
            this._dashboardCmd = argv;
            this._dashboardItem.visible = true;
        } catch (e) {
            if (!isCancelled(e))
                console.debug(`OpenCUT: dashboard detection failed: ${e.message}`);
        }
    }

    _openDashboard() {
        if (!this._dashboardCmd)
            return;
        try {
            Gio.Subprocess.new(this._dashboardCmd, Gio.SubprocessFlags.NONE);
        } catch (e) {
            Main.notify('OpenCUT', `Could not open dashboard: ${e.message}`);
        }
    }

    destroy() {
        this._cancellable.cancel();
        if (this._tickId) {
            GLib.source_remove(this._tickId);
            this._tickId = 0;
        }
        if (this._debounceId) {
            GLib.source_remove(this._debounceId);
            this._debounceId = 0;
        }
        if (this._monitor) {
            this._monitor.disconnectObject(this);
            this._monitor.cancel();
            this._monitor = null;
        }
        this._settings.disconnectObject(this);
        this._settings = null;
        this._tooltip?.destroy();
        this._tooltip = null;
        this._ext = null;
        super.destroy();
    }
});

export default class OpenCutExtension extends Extension {
    enable() {
        this._settings = this.getSettings();
        this._settings.connectObject('changed::panel-position', () => this._recreate(), this);
        this._create();
    }

    _create() {
        this._indicator = new Indicator(this);
        const pos = this._settings.get_string('panel-position');
        const box = ['left', 'center', 'right'].includes(pos) ? pos : 'right';
        Main.panel.addToStatusArea(this.uuid, this._indicator, box === 'right' ? 0 : -1, box);
    }

    _recreate() {
        this._indicator?.destroy();
        this._indicator = null;
        this._create();
    }

    disable() {
        this._settings?.disconnectObject(this);
        this._settings = null;
        this._indicator?.destroy();
        this._indicator = null;
    }
}
