// SPDX-License-Identifier: GPL-2.0-or-later
//
// Pure logic for the OpenCUT GNOME Shell extension: snapshot parsing,
// formatting, threshold classes and countdown text. No `gi://` imports and
// no Shell imports, so it runs under both `gjs -m` and Node for tests.

export const SCHEMA_ID = 'opencut/usage@1';

/**
 * Snapshot older than this (seconds) is treated as stale: 3 x the 30 s write
 * interval, the same rule as `ocut-tray`.
 */
export const MAX_AGE_S = 90;
/** Clock skew tolerated for a snapshot dated in the future (seconds). */
export const FUTURE_SKEW_S = 5;
/** Snapshots are a few KiB; anything larger is refused unread. */
export const MAX_BYTES = 1 << 20;
/** Longest text taken from the snapshot into one menu line. */
export const MAX_TEXT = 200;
/** Most snapshot errors listed in the menu. */
export const MAX_ERRORS = 5;

export const DEFAULT_FORMAT = '{5h.pct}% · {5h.reset_in}';
export const DEFAULT_WARN = 75;
export const DEFAULT_CRIT = 90;

/** Popup rows, in display order: [snapshot key, label, template alias]. */
export const WINDOWS = [
    ['session_5h', '5-hour session', '5h'],
    ['weekly_all', 'Weekly (all models)', '7d'],
    ['weekly_opus', 'Weekly Opus', 'opus'],
    ['weekly_sonnet', 'Weekly Sonnet', 'sonnet'],
];

const ALIASES = Object.fromEntries(WINDOWS.map(([key, , alias]) => [alias, key]));

const STATUSES = new Set(['ok', 'stale', 'degraded', 'offline', 'token_expired', 'error']);
const SOURCES = new Set(['oauth_api', 'statusline', 'local_estimate']);

export const NA = 'n/a';
export const MISSING_TEXT = '—'; // em dash

/**
 * Text from the snapshot made safe for one label: control characters become
 * spaces and the result is cut to `max` characters.
 */
export function clean(v, max = MAX_TEXT) {
    // eslint-disable-next-line no-control-regex
    let t = String(v ?? '').replace(/[\u0000-\u001f\u007f-\u009f]/g, ' ');
    if (t.length > max)
        t = `${t.slice(0, max - 1)}…`;
    return t.trim();
}

function isObj(v) {
    return v !== null && typeof v === 'object' && !Array.isArray(v);
}

function num(v) {
    return typeof v === 'number' && Number.isFinite(v) ? v : null;
}

function nonNeg(v) {
    const n = num(v);
    return n === null ? 0 : Math.max(0, n);
}

function clampPct(v) {
    const n = num(v);
    return n === null ? null : Math.min(100, Math.max(0, n));
}

/** RFC 3339 string -> epoch ms, or null. */
export function parseTime(v) {
    if (typeof v !== 'string' || v.length === 0)
        return null;
    const t = Date.parse(v);
    return Number.isFinite(t) ? t : null;
}

function normWindow(w, generatedMs) {
    if (!isObj(w))
        return null;
    const pct = clampPct(w.pct);
    if (pct === null)
        return null;
    let resetsMs = parseTime(w.resets_at);
    // Fall back to seconds_to_reset, relative to when the snapshot was made.
    if (resetsMs === null && w.resets_at === undefined &&
        num(w.seconds_to_reset) !== null && generatedMs !== null)
        resetsMs = generatedMs + w.seconds_to_reset * 1000;
    const source = SOURCES.has(w.source) ? w.source : 'unknown';
    let projection = null;
    if (isObj(w.projection) && clampPct(w.projection.pct_at_reset) !== null) {
        projection = {
            pctAtReset: clampPct(w.projection.pct_at_reset),
            hitsLimitMs: parseTime(w.projection.hits_limit_at),
            confidence: typeof w.projection.confidence === 'string' ? w.projection.confidence : 'low',
        };
    }
    return {
        pct,
        resetsMs,
        source,
        estimated: w.estimated === true || source === 'local_estimate',
        stale: w.stale === true,
        projection,
    };
}

function normTokens(t) {
    t = isObj(t) ? t : {};
    const out = {};
    for (const k of ['input', 'output', 'cache_write', 'cache_read', 'total', 'messages'])
        out[k] = nonNeg(t[k]);
    if (out.total === 0)
        out.total = out.input + out.output + out.cache_write + out.cache_read;
    return out;
}

/**
 * Parses snapshot text defensively. Never throws.
 *
 * @param {string} text raw file contents
 * @returns {{ok: true, snap: object} | {ok: false, error: string}}
 */
export function parseSnapshot(text) {
    let raw;
    try {
        raw = JSON.parse(text);
    } catch (e) {
        return {ok: false, error: clean(`invalid JSON: ${e.message}`)};
    }
    if (!isObj(raw))
        return {ok: false, error: 'snapshot is not an object'};
    if (raw.schema !== SCHEMA_ID)
        return {ok: false, error: `unsupported schema: ${clean(typeof raw.schema === 'string' ? raw.schema : String(JSON.stringify(raw.schema)), 60)}`};

    const generatedMs = parseTime(raw.generated_at);
    const windows = isObj(raw.windows) ? raw.windows : {};
    const tokens = isObj(raw.tokens) ? raw.tokens : {};
    const cost = isObj(raw.cost) ? raw.cost : {};
    const burn = isObj(raw.burn_rate) ? raw.burn_rate : {};
    const sources = isObj(raw.sources) ? raw.sources : {};

    const snap = {
        generatedMs,
        status: STATUSES.has(raw.status) ? raw.status : 'error',
        plan: isObj(raw.account) && typeof raw.account.plan === 'string' ? clean(raw.account.plan, 20) : 'unknown',
        windows: {},
        tokens: {
            session_5h: normTokens(tokens.session_5h),
            today: normTokens(tokens.today),
            week: normTokens(tokens.week),
        },
        cost: {
            session_5h: nonNeg(cost.session_5h_usd),
            today: nonNeg(cost.today_usd),
            week: nonNeg(cost.week_usd),
            note: typeof cost.note === 'string' ? clean(cost.note) : '',
        },
        burn: {
            tokensPerMin: nonNeg(burn.tokens_per_min),
            usdPerHour: nonNeg(burn.usd_per_hour),
            pctPerHour: nonNeg(burn.pct_per_hour),
            windowMin: nonNeg(burn.window_min),
        },
        activeSessions: nonNeg(raw.active_sessions),
        sources: {
            oauthApi: isObj(sources.oauth_api) && sources.oauth_api.enabled === true,
            oauthOk: isObj(sources.oauth_api) && sources.oauth_api.ok === true,
            statusline: isObj(sources.statusline) && sources.statusline.ok === true,
            localLogs: isObj(sources.local_logs) && sources.local_logs.ok === true,
        },
        errors: Array.isArray(raw.errors)
            ? raw.errors.filter(isObj).map(e => ({
                code: clean(e.code, 40),
                message: clean(e.message),
            }))
            : [],
    };
    for (const [key] of WINDOWS)
        snap.windows[key] = normWindow(windows[key], generatedMs);
    return {ok: true, snap};
}

/** `1h29m`, `12m`, `3d4h`; matches `ocut`'s fmt_duration. */
export function fmtDuration(secs) {
    const m = Math.floor(Math.max(0, secs) / 60);
    const d = Math.floor(m / 1440), h = Math.floor(m / 60) % 24, mm = m % 60;
    if (d === 0 && h === 0)
        return `${mm}m`;
    if (d === 0)
        return `${h}h${String(mm).padStart(2, '0')}m`;
    return `${d}d${h}h`;
}

/** Countdown text for a window, or `n/a` when unknown or no window is open. */
export function countdown(win, nowMs) {
    if (!win || win.resetsMs === null)
        return NA;
    return fmtDuration((win.resetsMs - nowMs) / 1000);
}

/** True when the data should be shown dimmed. */
export function isStale(snap, nowMs, maxAgeS = MAX_AGE_S) {
    if (!snap)
        return true;
    if (snap.status !== 'ok')
        return true;
    if (snap.generatedMs === null)
        return true;
    const ageS = (nowMs - snap.generatedMs) / 1000;
    if (ageS > maxAgeS || ageS < -FUTURE_SKEW_S)
        return true;
    const w = snap.windows.session_5h;
    return Boolean(w && w.stale);
}

/** CSS class for a percentage. */
export function pctClass(pct, warn = DEFAULT_WARN, crit = DEFAULT_CRIT) {
    if (pct === null || pct === undefined)
        return 'opencut-ok';
    if (pct >= crit)
        return 'opencut-crit';
    if (pct >= warn)
        return 'opencut-warn';
    return 'opencut-ok';
}

/** Highest known window percentage, or null. */
export function maxPct(snap) {
    let best = null;
    for (const [key] of WINDOWS) {
        const w = snap?.windows[key];
        if (w && (best === null || w.pct > best))
            best = w.pct;
    }
    return best;
}

/** Panel colour class: by the highest window, like `ocut --waybar`. */
export function panelClass(snap, warn = DEFAULT_WARN, crit = DEFAULT_CRIT) {
    if (!snap)
        return 'opencut-ok';
    if (snap.status === 'token_expired' || snap.status === 'error') {
        const p = maxPct(snap);
        return p !== null && p >= warn ? pctClass(p, warn, crit) : 'opencut-crit';
    }
    return pctClass(maxPct(snap), warn, crit);
}

function fmtNum(v, decimals) {
    return v.toFixed(decimals);
}

/** 1234567 -> `1.2M`, 3400 -> `3.4k`. */
export function fmtTokens(n) {
    if (n >= 1e9)
        return `${(n / 1e9).toFixed(1)}B`;
    if (n >= 1e6)
        return `${(n / 1e6).toFixed(1)}M`;
    if (n >= 1e3)
        return `${(n / 1e3).toFixed(1)}k`;
    return String(Math.round(n));
}

export function fmtUsd(v) {
    return `$${v.toFixed(2)}`;
}

function localResetText(ms) {
    const d = new Date(ms);
    const days = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
    const hh = String(d.getHours()).padStart(2, '0');
    const mm = String(d.getMinutes()).padStart(2, '0');
    return `${days[d.getDay()]} ${hh}:${mm}`;
}

/** Value for one template key, or undefined when the key is unknown. */
function templateValue(snap, key, decimals, nowMs) {
    const dot = key.indexOf('.');
    const head = dot < 0 ? key : key.slice(0, dot);
    const field = dot < 0 ? '' : key.slice(dot + 1);
    if (head in ALIASES) {
        const fields = ['pct', 'reset_in', 'resets_at', 'source', 'est', 'proj'];
        if (!fields.includes(field))
            return undefined;
        const w = snap.windows[ALIASES[head]];
        if (!w)
            return NA;
        switch (field) {
        case 'pct': return fmtNum(w.pct, decimals ?? 0);
        case 'reset_in': return countdown(w, nowMs);
        case 'resets_at': return w.resetsMs === null ? NA : localResetText(w.resetsMs);
        case 'source': return w.source;
        case 'est': return w.estimated ? '~' : '';
        case 'proj': return w.projection ? fmtNum(w.projection.pctAtReset, decimals ?? 0) : NA;
        }
    }
    switch (key) {
    case 'cost.5h': return fmtNum(snap.cost.session_5h, decimals ?? 2);
    case 'cost.today': return fmtNum(snap.cost.today, decimals ?? 2);
    case 'cost.week': return fmtNum(snap.cost.week, decimals ?? 2);
    case 'tokens.5h': return String(snap.tokens.session_5h.total);
    case 'tokens.today': return String(snap.tokens.today.total);
    case 'tokens.week': return String(snap.tokens.week.total);
    case 'burn.tok_min': return fmtNum(snap.burn.tokensPerMin, decimals ?? 0);
    case 'burn.usd_h': return fmtNum(snap.burn.usdPerHour, decimals ?? 2);
    case 'burn.pct_h': return fmtNum(snap.burn.pctPerHour, decimals ?? 1);
    case 'sessions': return String(snap.activeSessions);
    case 'status': return snap.status;
    case 'plan': return snap.plan;
    }
    return undefined;
}

/**
 * Renders a panel template using the same keys as `ocut --format`
 * (`{5h.pct}`, `{7d.reset_in}`, `{cost.today:.2}`, `{{`/`}}` escapes).
 * Unknown keys are left in place so a typo is visible.
 */
export function renderTemplate(template, snap, nowMs) {
    let out = '';
    let i = 0;
    while (i < template.length) {
        const c = template[i];
        if ((c === '{' || c === '}') && template[i + 1] === c) {
            out += c;
            i += 2;
            continue;
        }
        if (c === '{') {
            const end = template.indexOf('}', i);
            if (end < 0) {
                out += template.slice(i);
                break;
            }
            const spec = template.slice(i + 1, end);
            const m = /^([a-z0-9_.]+)(?::\.(\d))?$/.exec(spec);
            const v = m ? templateValue(snap, m[1], m[2] === undefined ? undefined : Number(m[2]), nowMs) : undefined;
            out += v === undefined ? `{${spec}}` : v;
            i = end + 1;
            continue;
        }
        out += c;
        i++;
    }
    return out;
}

/** Returns the unknown keys in a template (empty when valid). */
export function templateErrors(template) {
    const probe = parseSnapshot(JSON.stringify({schema: SCHEMA_ID, generated_at: '2026-01-01T00:00:00Z', status: 'ok'}));
    const bad = [];
    const re = /\{\{|\}\}|\{([^{}]*)\}/g;
    let m;
    while ((m = re.exec(template)) !== null) {
        if (m[1] === undefined)
            continue;
        const spec = /^([a-z0-9_.]+)(?::\.(\d))?$/.exec(m[1]);
        if (!spec || templateValue(probe.snap, spec[1], undefined, 0) === undefined)
            bad.push(m[1]);
    }
    return bad;
}

const STATUS_TEXT = {
    ok: 'OK',
    stale: 'Stale',
    degraded: 'Degraded',
    offline: 'Offline',
    token_expired: 'Token expired: run `claude` once',
    error: 'Error',
};

const SOURCE_TEXT = {
    oauth_api: 'API',
    statusline: 'statusline',
    local_estimate: 'local estimate',
    unknown: 'unknown',
};

export function statusText(status) {
    return STATUS_TEXT[status] ?? status;
}

export function sourceText(source) {
    return SOURCE_TEXT[source] ?? source;
}

/** Age text like `12s ago`, `3m ago`. */
export function ageText(snap, nowMs) {
    if (!snap || snap.generatedMs === null)
        return 'unknown age';
    const s = Math.max(0, Math.round((nowMs - snap.generatedMs) / 1000));
    if (s < 60)
        return `${s}s ago`;
    return `${fmtDuration(s)} ago`;
}

/**
 * Everything the panel button needs, as plain data.
 *
 * @param {{kind: 'ok', snap: object} | {kind: 'missing'} | {kind: 'invalid', error: string}} state
 */
export function panelView(state, opts, nowMs) {
    const {format = DEFAULT_FORMAT, warn = DEFAULT_WARN, crit = DEFAULT_CRIT} = opts ?? {};
    if (state.kind === 'missing') {
        return {text: MISSING_TEXT, styleClass: 'opencut-ok', dim: true, estimated: false,
            tooltip: 'ocutd not running'};
    }
    if (state.kind !== 'ok') {
        return {text: MISSING_TEXT, styleClass: 'opencut-crit', dim: true, estimated: false,
            tooltip: `OpenCUT snapshot unreadable: ${state.error}`};
    }
    const snap = state.snap;
    const w = snap.windows.session_5h;
    const stale = isStale(snap, nowMs);
    const parts = [`OpenCUT: ${statusText(snap.status)}`, `updated ${ageText(snap, nowMs)}`];
    if (w)
        parts.push(`5h ${w.pct.toFixed(0)}% (${sourceText(w.source)})`);
    return {
        text: renderTemplate(format, snap, nowMs),
        styleClass: panelClass(snap, warn, crit),
        dim: stale,
        estimated: Boolean(w && w.estimated),
        tooltip: parts.join(' · '),
    };
}

/** One popup bar row as plain data. */
export function windowRow(snap, key, nowMs, warn = DEFAULT_WARN, crit = DEFAULT_CRIT) {
    const w = snap?.windows[key] ?? null;
    if (!w)
        return {known: false, pct: 0, pctText: NA, resetText: NA, styleClass: 'opencut-ok', estimated: false, projText: ''};
    const reset = countdown(w, nowMs);
    let projText = '';
    if (w.projection) {
        projText = `→ ${w.projection.pctAtReset.toFixed(0)}% at reset (${w.projection.confidence})`;
        if (w.projection.hitsLimitMs !== null)
            projText += `, limit in ${fmtDuration((w.projection.hitsLimitMs - nowMs) / 1000)}`;
    }
    return {
        known: true,
        pct: w.pct,
        pctText: `${w.estimated ? '~' : ''}${w.pct.toFixed(0)}%`,
        resetText: w.resetsMs === null ? 'no window open' : `resets in ${reset}`,
        styleClass: pctClass(w.pct, warn, crit),
        estimated: w.estimated,
        projText,
    };
}

/** Popup text lines for tokens, cost and burn rate. */
export function detailLines(snap, showCost) {
    const t = snap.tokens.today;
    const lines = {
        today: `Today: ${fmtTokens(t.total)} tokens (${t.messages} msgs)`,
        cost: null,
        burn: `Burn: ${fmtTokens(snap.burn.tokensPerMin)} tok/min · ${snap.burn.pctPerHour.toFixed(1)}%/h`,
    };
    if (showCost) {
        lines.cost = `Cost today: ${fmtUsd(snap.cost.today)} · week ${fmtUsd(snap.cost.week)}`;
        lines.burn += ` · ${fmtUsd(snap.burn.usdPerHour)}/h`;
    }
    return lines;
}

/** Popup error text: at most MAX_ERRORS lines, then a count of the rest. */
export function errorText(snap) {
    const lines = snap.errors.slice(0, MAX_ERRORS).map(e => `${e.code}: ${e.message}`);
    if (snap.errors.length > MAX_ERRORS)
        lines.push(`… ${snap.errors.length - MAX_ERRORS} more`);
    return lines.join('\n');
}

/** Source/status badge text. */
export function badgeText(snap, nowMs) {
    const w = snap.windows.session_5h;
    const src = w ? sourceText(w.source) : 'no limit data';
    const stale = isStale(snap, nowMs) && snap.status !== 'stale' ? ' · stale' : '';
    return `${statusText(snap.status)} · ${src}${stale} · updated ${ageText(snap, nowMs)}`;
}
