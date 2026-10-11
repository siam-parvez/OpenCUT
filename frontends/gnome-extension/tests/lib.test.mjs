// SPDX-License-Identifier: GPL-2.0-or-later
//
// Tests for lib.js. Runs under both Node and GJS:
//   node --test tests/lib.test.mjs     (or plain `node tests/lib.test.mjs`)
//   gjs -m tests/lib.test.mjs

import * as Lib from '../opencut@opencut.github.io/lib.js';

const isNode = Boolean(globalThis.process?.versions?.node);
async function readText(rel) {
    if (isNode) {
        const fs = await import('node:fs');
        return fs.readFileSync(new URL(rel, import.meta.url), 'utf8');
    }
    // GJS has no global URL; resolve against this file's directory with Gio.
    const {default: Gio} = await import('gi://Gio');
    const file = Gio.File.new_for_uri(import.meta.url).get_parent().resolve_relative_path(rel);
    const [, bytes] = file.load_contents(null);
    return new TextDecoder().decode(bytes);
}

let passed = 0, failed = 0;
const tests = [];
function test(name, fn) {
    tests.push([name, fn]);
}
function eq(actual, expected, msg = '') {
    const a = JSON.stringify(actual), e = JSON.stringify(expected);
    if (a !== e)
        throw new Error(`${msg} expected ${e}, got ${a}`);
}
function ok(cond, msg = 'assertion failed') {
    if (!cond)
        throw new Error(msg);
}

const NOW = Date.parse('2026-10-11T12:30:35Z');
const OPTS = {format: Lib.DEFAULT_FORMAT, warn: 75, crit: 90};

async function load(rel) {
    const r = Lib.parseSnapshot(await readText(rel));
    ok(r.ok, `parse ${rel}: ${r.error}`);
    return r.snap;
}

test('fmtDuration matches ocut', () => {
    eq(Lib.fmtDuration(5395), '1h29m');
    eq(Lib.fmtDuration(720), '12m');
    eq(Lib.fmtDuration(3600), '1h00m');
    eq(Lib.fmtDuration(3 * 86400 + 4 * 3600 + 59), '3d4h');
    eq(Lib.fmtDuration(-50), '0m');
    eq(Lib.fmtDuration(59), '0m');
});

test('pctClass thresholds', () => {
    eq(Lib.pctClass(74.9), 'opencut-ok');
    eq(Lib.pctClass(75), 'opencut-warn');
    eq(Lib.pctClass(89.99), 'opencut-warn');
    eq(Lib.pctClass(90), 'opencut-crit');
    eq(Lib.pctClass(100), 'opencut-crit');
    eq(Lib.pctClass(null), 'opencut-ok');
    eq(Lib.pctClass(60, 50, 60), 'opencut-crit');
});

test('example fixture: panel "62% · 1h29m", ok, not dimmed', async () => {
    const snap = await load('../../../tests/fixtures/snapshot/usage.v1.example.json');
    const v = Lib.panelView({kind: 'ok', snap}, OPTS, NOW);
    eq(v.text, '62% · 1h29m');
    eq(v.styleClass, 'opencut-ok');
    eq(v.dim, false);
    eq(v.estimated, false);
    eq(snap.windows.weekly_sonnet, null);
    eq(Lib.windowRow(snap, 'weekly_sonnet', NOW).pctText, 'n/a');
    eq(Lib.windowRow(snap, 'weekly_all', NOW).resetText, 'resets in 3d20h');
    eq(Lib.windowRow(snap, 'session_5h', NOW).projText, '→ 91% at reset (med)');
    // weekly_opus has no `estimated` key and an oauth source: not estimated.
    eq(snap.windows.weekly_opus.estimated, false);
});

test('example fixture: countdown ticks without a reload', async () => {
    const snap = await load('../../../tests/fixtures/snapshot/usage.v1.example.json');
    eq(Lib.panelView({kind: 'ok', snap}, OPTS, NOW + 30 * 60 * 1000).text, '62% · 0h59m'.replace('0h', ''));
    // Snapshot ages past MAX_AGE_S -> dimmed.
    eq(Lib.panelView({kind: 'ok', snap}, OPTS, NOW + 200 * 1000).dim, true);
});

test('example fixture: details, cost toggle and badge', async () => {
    const snap = await load('../../../tests/fixtures/snapshot/usage.v1.example.json');
    const on = Lib.detailLines(snap, true);
    eq(on.today, 'Today: 0 tokens (0 msgs)');
    eq(on.cost, 'Cost today: $1.12 · week $9.80');
    eq(on.burn, 'Burn: 3.1k tok/min · 14.2%/h · $0.90/h');
    const off = Lib.detailLines(snap, false);
    eq(off.cost, null);
    eq(off.burn, 'Burn: 3.1k tok/min · 14.2%/h');
    eq(Lib.badgeText(snap, NOW), 'OK · API · updated 30s ago');
    eq(snap.cost.note, 'API-equivalent estimate; plan is flat-rate');
});

test('stale fixture: dimmed, warn colour, errors kept', async () => {
    const snap = await load('fixtures/stale.json');
    const v = Lib.panelView({kind: 'ok', snap}, OPTS, NOW);
    eq(v.text, '80% · 1h29m');
    eq(v.styleClass, 'opencut-warn');
    eq(v.dim, true);
    ok(Lib.badgeText(snap, NOW).startsWith('Stale · '));
    eq(Lib.badgeText(snap, NOW).match(/stale/gi).length, 1, 'stale shown once');
    eq(snap.errors, [{code: 'oauth_backoff', message: '429 from usage endpoint; retrying in 240s'}]);
});

test('estimated fixture: est marker, crit, projection with limit time', async () => {
    const snap = await load('fixtures/estimated.json');
    const v = Lib.panelView({kind: 'ok', snap}, OPTS, NOW);
    eq(v.text, '94% · 14m');
    eq(v.estimated, true);
    eq(v.styleClass, 'opencut-crit');
    eq(v.dim, false);
    const row = Lib.windowRow(snap, 'session_5h', NOW);
    eq(row.pctText, '~94%');
    eq(row.projText, '→ 100% at reset (low), limit in 10m');
    // Missing total is summed from the parts.
    eq(snap.tokens.today.total, 6000);
    eq(Lib.detailLines(snap, true).today, 'Today: 6.0k tokens (4 msgs)');
    eq(Lib.renderTemplate('{5h.est}{5h.pct:.1}% {7d.pct} {7d.reset_in}', snap, NOW), '~93.6% n/a n/a');
});

test('null-windows fixture: n/a everywhere, dimmed (degraded)', async () => {
    const snap = await load('fixtures/null-windows.json');
    const v = Lib.panelView({kind: 'ok', snap}, OPTS, NOW);
    eq(v.text, 'n/a% · n/a');
    eq(v.dim, true);
    eq(v.estimated, false);
    for (const [key] of Lib.WINDOWS) {
        const r = Lib.windowRow(snap, key, NOW);
        eq([r.known, r.pctText, r.resetText], [false, 'n/a', 'n/a'], key);
    }
    eq(Lib.maxPct(snap), null);
    eq(Lib.badgeText(snap, NOW), 'Degraded · no limit data · stale · updated 30s ago');
});

test('token-expired fixture: crit colour, no open window', async () => {
    const snap = await load('fixtures/token-expired.json');
    const v = Lib.panelView({kind: 'ok', snap}, OPTS, NOW);
    eq(v.styleClass, 'opencut-crit');
    eq(v.dim, true);
    eq(v.text, '10% · n/a');
    eq(Lib.windowRow(snap, 'session_5h', NOW).resetText, 'no window open');
    ok(Lib.statusText(snap.status).includes('claude'));
});

test('hostile fixture: never throws, clamps and drops bad values', async () => {
    const snap = await load('fixtures/hostile.json');
    eq(snap.status, 'error');
    eq(snap.generatedMs, null);
    eq(snap.windows.session_5h.pct, 100);
    eq(snap.windows.session_5h.resetsMs, null);
    eq(snap.windows.session_5h.source, 'unknown');
    eq(snap.windows.session_5h.projection, null);
    eq(snap.windows.weekly_all, null);
    eq(snap.windows.weekly_opus, null);
    eq(snap.windows.weekly_sonnet.pct, 0);
    eq(snap.tokens.today.total, 0);
    eq(snap.cost.today, 0);
    eq(snap.errors, [{code: '1', message: ''}]);
    const v = Lib.panelView({kind: 'ok', snap}, OPTS, NOW);
    eq(v.dim, true);
    eq(v.styleClass, 'opencut-crit');
});

test('invalid inputs are rejected with an error, not a throw', async () => {
    eq(Lib.parseSnapshot(await readText('fixtures/truncated.json')).ok, false);
    const ws = Lib.parseSnapshot(await readText('fixtures/wrong-schema.json'));
    eq(ws, {ok: false, error: 'unsupported schema: opencut/usage@2'});
    eq(Lib.parseSnapshot('').ok, false);
    eq(Lib.parseSnapshot('null').ok, false);
    eq(Lib.parseSnapshot('[1]').ok, false);
});

test('missing / invalid states', () => {
    const m = Lib.panelView({kind: 'missing'}, OPTS, NOW);
    eq([m.text, m.tooltip, m.dim], ['—', 'ocutd not running', true]);
    const i = Lib.panelView({kind: 'invalid', error: 'boom'}, OPTS, NOW);
    eq(i.text, '—');
    ok(i.tooltip.includes('boom'));
    eq(Lib.windowRow(null, 'session_5h', NOW).pctText, 'n/a');
});

test('templates: keys, precision, escapes, unknown keys', async () => {
    const snap = await load('../../../tests/fixtures/snapshot/usage.v1.example.json');
    const r = t => Lib.renderTemplate(t, snap, NOW);
    eq(r('{{5h}} {5h.pct:.1}%'), '{5h} 62.0%');
    eq(r('{cost.today} {cost.week:.0} {tokens.5h} {burn.pct_h} {sessions} {status} {plan}'),
        '1.12 10 997200 14.2 1 ok pro');
    eq(r('{opus.pct}/{sonnet.pct} {5h.source} {5h.proj}'), '12/n/a oauth_api 91');
    eq(r('{bogus} {5h.nope} {'), '{bogus} {5h.nope} {');
    // Local-time rendering; compute the expectation so the test is TZ-independent.
    const d = new Date(snap.windows.session_5h.resetsMs);
    const hhmm = `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
    eq(r('{5h.resets_at}'), `${['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'][d.getDay()]} ${hhmm}`);
    if (d.getTimezoneOffset() === 0)
        eq(r('{5h.resets_at}'), 'Sun 14:00');
    eq(Lib.templateErrors(Lib.DEFAULT_FORMAT), []);
    eq(Lib.templateErrors('{5h.pct} {bad} {{x}} {cost.today:.2}'), ['bad']);
});

test('seconds_to_reset fallback when resets_at is absent', () => {
    const r = Lib.parseSnapshot(JSON.stringify({
        schema: 'opencut/usage@1', generated_at: '2026-10-11T12:30:05Z', status: 'ok',
        windows: {session_5h: {pct: 5, seconds_to_reset: 600, source: 'statusline'}},
    }));
    eq(Lib.countdown(r.snap.windows.session_5h, NOW), '9m');
});

test('untrusted text is cleaned and errors are capped', () => {
    eq(Lib.clean('a\nb\u0007c'), 'a b c');
    eq(Lib.clean('x'.repeat(500)).length, Lib.MAX_TEXT);
    const errors = Array.from({length: 1000}, (_, i) => ({code: `c${i}`, message: `m\n${'z'.repeat(10000)}`, at: '2026-10-11T12:30:05Z'}));
    const r = Lib.parseSnapshot(JSON.stringify({schema: 'opencut/usage@1', generated_at: '2026-10-11T12:30:05Z', status: 'error', errors}));
    eq(r.ok, true);
    const text = Lib.errorText(r.snap);
    const lines = text.split('\n');
    eq(lines.length, Lib.MAX_ERRORS + 1);
    eq(lines[Lib.MAX_ERRORS], '… 995 more');
    ok(lines.every(l => l.length <= Lib.MAX_TEXT + 50), 'error lines are bounded');
    const ws = Lib.parseSnapshot(JSON.stringify({schema: {deep: 'x'.repeat(5000)}}));
    ok(!ws.ok && ws.error.length < 100, 'schema in error is bounded');
});

test('future-dated snapshot is stale; small skew is not', () => {
    const at = gen => Lib.parseSnapshot(JSON.stringify({schema: 'opencut/usage@1', generated_at: gen, status: 'ok'})).snap;
    eq(Lib.isStale(at('2026-10-11T12:30:38Z'), NOW), false);
    eq(Lib.isStale(at('2026-10-11T13:30:35Z'), NOW), true);
    eq(Lib.isStale(at('2026-10-11T12:29:00Z'), NOW), true);
});

for (const [name, fn] of tests) {
    try {
        await fn();
        passed++;
        print_(`ok - ${name}`);
    } catch (e) {
        failed++;
        print_(`FAIL - ${name}: ${e.message}`);
    }
}
print_(`\n${passed} passed, ${failed} failed (${isNode ? 'node' : 'gjs'})`);
if (failed) {
    if (isNode)
        globalThis.process.exit(1);
    else
        (await import('system')).exit(1);
}

function print_(s) {
    if (isNode)
        console.log(s);
    else
        globalThis.print(s);
}
