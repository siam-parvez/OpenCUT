//! `--format` templates and `--waybar` output.
//!
//! A template is text with `{key}` or `{key:.N}` placeholders; `{{` and `}}`
//! are literal braces. Unknown values (no window, no reading) render `n/a`.

use chrono::{DateTime, Local, Utc};
use ocut_core::Snapshot;
use ocut_core::snapshot::{Status, Window};
use ocut_sources::statusline::fmt_duration;
use serde_json::json;

/// Template keys, for `--help` and error messages.
pub const KEYS: &str = "5h|7d|opus|sonnet .{pct,reset_in,resets_at,source,est,proj}, \
    cost.{5h,today,week}, tokens.{5h,today,week}, burn.{tok_min,usd_h,pct_h}, \
    sessions, status, plan";

enum Val {
    Num(f64, usize),
    Int(u64),
    Text(String),
    Na,
}

fn window<'a>(s: &'a Snapshot, name: &str) -> Option<Option<&'a Window>> {
    let w = &s.windows;
    Some(match name {
        "5h" => w.session_5h.as_ref(),
        "7d" | "week" => w.weekly_all.as_ref(),
        "opus" => w.weekly_opus.as_ref(),
        "sonnet" => w.weekly_sonnet.as_ref(),
        _ => return None,
    })
}

fn value(s: &Snapshot, key: &str, now: DateTime<Utc>) -> Option<Val> {
    let (head, field) = key.split_once('.').unwrap_or((key, ""));
    if let Some(w) = window(s, head) {
        let Some(w) = w else {
            return matches!(
                field,
                "pct" | "reset_in" | "resets_at" | "source" | "est" | "proj"
            )
            .then_some(Val::Na);
        };
        return Some(match field {
            "pct" => Val::Num(w.pct, 0),
            "reset_in" => w.resets_at.map_or(Val::Na, |r| {
                Val::Text(fmt_duration((r - now).num_seconds()))
            }),
            "resets_at" => w.resets_at.map_or(Val::Na, |r| {
                Val::Text(r.with_timezone(&Local).format("%a %H:%M").to_string())
            }),
            "source" => Val::Text(source_name(w).into()),
            "est" => Val::Text(if w.estimated { "~" } else { "" }.into()),
            "proj" => w
                .projection
                .as_ref()
                .map_or(Val::Na, |p| Val::Num(p.pct_at_reset, 0)),
            _ => return None,
        });
    }
    let t = &s.tokens;
    Some(match key {
        "cost.5h" => Val::Num(s.cost.session_5h_usd, 2),
        "cost.today" => Val::Num(s.cost.today_usd, 2),
        "cost.week" => Val::Num(s.cost.week_usd, 2),
        "tokens.5h" => Val::Int(t.session_5h.total),
        "tokens.today" => Val::Int(t.today.total),
        "tokens.week" => Val::Int(t.week.total),
        "burn.tok_min" => Val::Num(s.burn_rate.tokens_per_min, 0),
        "burn.usd_h" => Val::Num(s.burn_rate.usd_per_hour, 2),
        "burn.pct_h" => Val::Num(s.burn_rate.pct_per_hour, 1),
        "sessions" => Val::Int(u64::from(s.active_sessions)),
        "status" => Val::Text(status_name(s.status).into()),
        "plan" => Val::Text(
            serde_json::to_value(s.account.plan)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
        ),
        _ => return None,
    })
}

fn source_name(w: &Window) -> &'static str {
    match w.source {
        ocut_core::WindowSource::OauthApi => "oauth_api",
        ocut_core::WindowSource::Statusline => "statusline",
        ocut_core::WindowSource::LocalEstimate => "local_estimate",
    }
}

pub fn status_name(s: Status) -> &'static str {
    match s {
        Status::Ok => "ok",
        Status::Stale => "stale",
        Status::Degraded => "degraded",
        Status::Offline => "offline",
        Status::TokenExpired => "token_expired",
        Status::Error => "error",
    }
}

/// Renders `template`. Errors name the first unknown key or bad spec.
pub fn render(template: &str, s: &Snapshot, now: DateTime<Utc>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        if tail.starts_with('}') {
            return Err("unmatched `}` in format".into());
        }
        let end = tail.find('}').ok_or("unclosed `{` in format")?;
        let inner = &tail[1..end];
        let (key, spec) = inner.split_once(':').unwrap_or((inner, ""));
        let prec = match spec {
            "" => None,
            s => Some(
                s.strip_prefix('.')
                    .and_then(|p| p.parse::<usize>().ok())
                    .filter(|p| *p <= 6)
                    .ok_or_else(|| format!("bad format spec `{s}` (use `.N`)"))?,
            ),
        };
        let v = value(s, key.trim(), now)
            .ok_or_else(|| format!("unknown format key `{key}`; keys: {KEYS}"))?;
        out.push_str(&match v {
            Val::Num(x, d) => format!("{x:.*}", prec.unwrap_or(d)),
            Val::Int(n) => match prec {
                Some(p) => format!("{:.*}", p, n as f64),
                None => n.to_string(),
            },
            Val::Text(t) => t,
            Val::Na => "n/a".into(),
        });
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Highest pct among the 5h and weekly windows.
fn max_pct(s: &Snapshot) -> Option<f64> {
    let w = &s.windows;
    [
        &w.session_5h,
        &w.weekly_all,
        &w.weekly_opus,
        &w.weekly_sonnet,
    ]
    .into_iter()
    .flatten()
    .map(|w| w.pct)
    .reduce(f64::max)
}

/// `crit` from 90%, `warn` from 75%, else the status (`ok`, `stale`, ...).
pub fn css_class(s: &Snapshot) -> &'static str {
    match max_pct(s) {
        Some(p) if p >= 90.0 => "crit",
        Some(p) if p >= 75.0 => "warn",
        _ if s.status == Status::TokenExpired || s.status == Status::Error => "crit",
        _ => status_name(s.status),
    }
}

/// Multi-line plain-text summary (Waybar tooltip).
pub fn tooltip(s: &Snapshot, now: DateTime<Utc>) -> String {
    let win = |label: &str, w: Option<&Window>| match w {
        None => format!("{label}: n/a"),
        Some(w) => {
            let mut l = format!(
                "{label}: {}{:.0}%",
                if w.estimated { "~" } else { "" },
                w.pct
            );
            if let Some(r) = w.resets_at {
                l.push_str(&format!(
                    ", resets in {}",
                    fmt_duration((r - now).num_seconds())
                ));
            }
            if let Some(p) = &w.projection {
                l.push_str(&format!(", ~{:.0}% at reset", p.pct_at_reset));
            }
            l.push_str(&format!(" ({})", source_name(w)));
            l
        }
    };
    let w = &s.windows;
    let mut lines = vec![
        win("5h session", w.session_5h.as_ref()),
        win("Weekly", w.weekly_all.as_ref()),
    ];
    if let Some(o) = &w.weekly_opus {
        lines.push(win("Weekly Opus", Some(o)));
    }
    if let Some(o) = &w.weekly_sonnet {
        lines.push(win("Weekly Sonnet", Some(o)));
    }
    lines.push(format!(
        "Today: {} tokens, ${:.2} (API-equivalent)",
        s.tokens.today.total, s.cost.today_usd
    ));
    lines.push(format!(
        "Burn: {:.0} tok/min, ${:.2}/h",
        s.burn_rate.tokens_per_min, s.burn_rate.usd_per_hour
    ));
    lines.push(format!("Active sessions: {}", s.active_sessions));
    lines.push(format!("Status: {}", status_name(s.status)));
    for e in &s.errors {
        lines.push(format!("! {}: {}", e.code, e.message));
    }
    lines.join("\n")
}

/// One line of Waybar `custom` module JSON.
pub fn waybar(s: &Snapshot, template: &str, now: DateTime<Utc>) -> Result<String, String> {
    let v = json!({
        "text": render(template, s, now)?,
        "tooltip": tooltip(s, now),
        "class": css_class(s),
        "percentage": s.windows.session_5h.as_ref().map_or(0, |w| w.pct.round() as u32),
    });
    Ok(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> (Snapshot, DateTime<Utc>) {
        let s: Snapshot = serde_json::from_str(include_str!(
            "../../../tests/fixtures/snapshot/usage.v1.example.json"
        ))
        .unwrap();
        let now = s.generated_at;
        (s, now)
    }

    #[test]
    fn default_template() {
        let (s, now) = snap();
        assert_eq!(
            render("{5h.pct:.0}% ↻{5h.reset_in}", &s, now).unwrap(),
            "62% ↻1h29m"
        );
    }

    #[test]
    fn keys_precision_and_escapes() {
        let (s, now) = snap();
        assert_eq!(
            render(
                "{{x}} {5h.pct:.1} {cost.week} {sonnet.pct} {sessions}",
                &s,
                now
            )
            .unwrap(),
            "{x} 62.0 9.80 n/a 1"
        );
        assert_eq!(render("{5h.source}{5h.est}", &s, now).unwrap(), "oauth_api");
        assert_eq!(render("{plan}/{status}", &s, now).unwrap(), "pro/ok");
        assert!(render("{nope}", &s, now).unwrap_err().contains("unknown"));
        assert!(render("{5h.pct:x}", &s, now).is_err());
        assert!(render("{5h.pct", &s, now).is_err());
        assert!(render("a}b", &s, now).is_err());
    }

    #[test]
    fn waybar_classes() {
        let (mut s, now) = snap();
        let v: serde_json::Value =
            serde_json::from_str(&waybar(&s, "{5h.pct}%", now).unwrap()).unwrap();
        assert_eq!(v["text"], "62%");
        assert_eq!(v["class"], "ok");
        assert_eq!(v["percentage"], 62);
        assert!(v["tooltip"].as_str().unwrap().contains("Weekly: 34%"));
        s.windows.session_5h.as_mut().unwrap().pct = 80.0;
        assert_eq!(css_class(&s), "warn");
        s.windows.weekly_all.as_mut().unwrap().pct = 95.0;
        assert_eq!(css_class(&s), "crit");
        s.windows = Default::default();
        s.status = Status::Stale;
        assert_eq!(css_class(&s), "stale");
    }
}
