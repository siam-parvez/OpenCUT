// SPDX-License-Identifier: GPL-2.0-or-later

import Adw from 'gi://Adw';
import Gio from 'gi://Gio';
import Gtk from 'gi://Gtk';

import {ExtensionPreferences} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

import * as Lib from './lib.js';

const POSITIONS = ['left', 'center', 'right'];
const POSITION_LABELS = ['Left', 'Center', 'Right'];
const FORMAT_HELP = 'Keys: {5h|7d|opus|sonnet.pct|reset_in|resets_at|est|proj|source}, ' +
    '{cost.today:.2}, {tokens.today}, {burn.pct_h}, {sessions}, {status}';

export default class OpenCutPreferences extends ExtensionPreferences {
    fillPreferencesWindow(window) {
        const settings = this.getSettings();
        window._settings = settings;

        const page = new Adw.PreferencesPage({title: 'OpenCUT', icon_name: 'preferences-system-symbolic'});
        window.add(page);

        // Panel.
        const panel = new Adw.PreferencesGroup({title: 'Panel', description: FORMAT_HELP});
        page.add(panel);

        const format = new Adw.EntryRow({title: 'Panel format', show_apply_button: true});
        format.text = settings.get_string('panel-format');
        const checkFormat = () => {
            const bad = Lib.templateErrors(format.text);
            if (bad.length)
                format.add_css_class('error');
            else
                format.remove_css_class('error');
            format.tooltip_text = bad.length ? `Unknown keys: ${bad.join(', ')}` : '';
        };
        format.connect('changed', checkFormat);
        format.connect('apply', () => settings.set_string('panel-format', format.text || Lib.DEFAULT_FORMAT));
        checkFormat();
        const reset = new Gtk.Button({
            icon_name: 'edit-undo-symbolic',
            valign: Gtk.Align.CENTER,
            tooltip_text: 'Reset to default',
            css_classes: ['flat'],
        });
        reset.connect('clicked', () => {
            settings.reset('panel-format');
            format.text = settings.get_string('panel-format');
        });
        format.add_suffix(reset);
        panel.add(format);

        const position = new Adw.ComboRow({
            title: 'Position',
            model: Gtk.StringList.new(POSITION_LABELS),
        });
        position.selected = Math.max(0, POSITIONS.indexOf(settings.get_string('panel-position')));
        position.connect('notify::selected', () =>
            settings.set_string('panel-position', POSITIONS[position.selected] ?? 'right'));
        panel.add(position);

        // Menu.
        const menu = new Adw.PreferencesGroup({title: 'Menu'});
        page.add(menu);
        const cost = new Adw.SwitchRow({
            title: 'Show cost estimate',
            subtitle: 'API-equivalent estimate; a flat-rate plan does not bill this',
        });
        settings.bind('show-cost', cost, 'active', Gio.SettingsBindFlags.DEFAULT);
        menu.add(cost);

        // Thresholds.
        const thresholds = new Adw.PreferencesGroup({
            title: 'Thresholds',
            description: 'Colour the panel by the highest window percentage',
        });
        page.add(thresholds);
        for (const [key, title] of [['warn-threshold', 'Warning (%)'], ['crit-threshold', 'Critical (%)']]) {
            const row = Adw.SpinRow.new_with_range(1, 100, 1);
            row.title = title;
            settings.bind(key, row, 'value', Gio.SettingsBindFlags.DEFAULT);
            thresholds.add(row);
        }

        return Promise.resolve();
    }
}
