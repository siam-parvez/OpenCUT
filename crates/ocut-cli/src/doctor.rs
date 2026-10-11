//! `ocut doctor`: file modes, source health and the Claude Code version.
//! Prints paths, modes, counts and ages only: never a token, an email,
//! message content or settings values.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;
use ocut_core::{Config, OAuthConsent, Snapshot};
use ocut_sources::credentials::{CredentialsError, check_credentials_file, read_credentials};
use ocut_sources::fsutil::mode_of;
use ocut_sources::jsonl::JsonlScanner;
use ocut_sources::paths::Paths;
use ocut_sources::sessions::count_active_sessions;
use ocut_sources::statusline::{fmt_duration, read_cache};

struct Report {
    problems: u32,
    home: String,
}

impl Report {
    fn line(&mut self, level: &str, what: &str, detail: impl AsRef<str>) {
        if level == "FAIL" {
            self.problems += 1;
        }
        let detail = detail.as_ref().replace(&self.home, "~");
        println!("[{level:<4}] {what:<14} {detail}");
    }
}

fn age(at: chrono::DateTime<Utc>) -> String {
    fmt_duration((Utc::now() - at).num_seconds())
}

fn private_mode(path: &Path, want: u32) -> (String, &'static str) {
    match mode_of(path) {
        Some(m) => (
            format!("{m:04o}"),
            if m & !want == 0 { "ok" } else { "WARN" },
        ),
        None => ("missing".into(), "info"),
    }
}

/// `claude --version`, with a timeout.
fn claude_version() -> Option<String> {
    let mut child = Command::new("claude")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    while child.try_wait().ok()?.is_none() {
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let v = s.lines().next()?.trim();
    (!v.is_empty() && v.len() < 80).then(|| v.to_owned())
}

pub fn run(paths: &Paths, cfg: &Config, config_err: Option<&str>) -> u32 {
    let mut r = Report {
        problems: 0,
        home: paths.home.display().to_string(),
    };
    println!("OpenCUT doctor (ocut {})", env!("CARGO_PKG_VERSION"));
    let cf = paths.config_file.display().to_string();
    match (config_err, paths.config_file.exists()) {
        (Some(e), _) => r.line("FAIL", "config", format!("{cf}: {e}")),
        (None, false) => r.line("ok", "config", format!("{cf} (absent, using defaults)")),
        (None, true) => {
            let (m, lvl) = private_mode(&paths.config_file, 0o644);
            r.line(lvl, "config", format!("{cf} (mode {m})"));
        }
    }

    // Source A.
    let consent = cfg.sources.oauth_api;
    let consent_s = match consent {
        OAuthConsent::Ask => "ask (off until you answer the prompt in a terminal)",
        OAuthConsent::On => "on",
        OAuthConsent::Off => "off",
    };
    r.line("info", "oauth_api", consent_s);
    let cp = paths.credentials.display().to_string();
    match check_credentials_file(&paths.credentials) {
        Ok(mode) => r.line(
            "ok",
            "credentials",
            format!("{cp} (mode {mode:04o}, owner ok)"),
        ),
        Err(CredentialsError::NotFound) => {
            let lvl = if consent == OAuthConsent::On {
                "FAIL"
            } else {
                "info"
            };
            r.line(lvl, "credentials", format!("{cp} not found"));
        }
        Err(e) => {
            let lvl = if consent == OAuthConsent::On {
                "FAIL"
            } else {
                "WARN"
            };
            r.line(lvl, "credentials", format!("{cp}: {e}"));
        }
    }
    if consent == OAuthConsent::On {
        match read_credentials(&paths.credentials) {
            Ok(c) => {
                let plan = serde_json::to_value(c.plan()).unwrap_or_default();
                let exp = match c.expires_at {
                    Some(t) if t <= Utc::now() => {
                        "token expired: run `claude` once to refresh it".to_owned()
                    }
                    Some(t) => format!(
                        "token valid for {}",
                        fmt_duration((t - Utc::now()).num_seconds())
                    ),
                    None => "token expiry unknown".into(),
                };
                let lvl = if c.is_expired(Utc::now()) {
                    "WARN"
                } else {
                    "ok"
                };
                r.line(
                    lvl,
                    "oauth token",
                    format!("plan {}, {exp}", plan.as_str().unwrap_or("?")),
                );
            }
            Err(e) => r.line("FAIL", "oauth token", e.to_string()),
        }
    }

    // Snapshot.
    let sp = paths.snapshot.display().to_string();
    match std::fs::read(&paths.snapshot) {
        Ok(b) => {
            let (m, lvl) = private_mode(&paths.snapshot, 0o600);
            let dir = paths.snapshot.parent().unwrap_or(Path::new("/"));
            let (dm, dlvl) = private_mode(dir, 0o700);
            let lvl = if lvl == "ok" { dlvl } else { lvl };
            match serde_json::from_slice::<Snapshot>(&b) {
                Ok(s) => r.line(
                    lvl,
                    "snapshot",
                    format!(
                        "{sp} (mode {m}, dir {dm}, {} old, status {})",
                        age(s.generated_at),
                        crate::format::status_name(s.status)
                    ),
                ),
                Err(_) => r.line("FAIL", "snapshot", format!("{sp} is not a valid snapshot")),
            }
        }
        Err(_) => r.line(
            "info",
            "snapshot",
            format!("{sp} absent (ocutd not running?)"),
        ),
    }

    // Source B.
    let lp = paths.statusline_cache.display().to_string();
    match read_cache(&paths.statusline_cache) {
        Ok(Some(c)) => {
            let (m, lvl) = private_mode(&paths.statusline_cache, 0o600);
            let fresh = if c.is_stale(Utc::now()) {
                "stale"
            } else {
                "fresh"
            };
            let has_limits = c.readings.session_5h.is_some() || c.readings.weekly_all.is_some();
            r.line(
                lvl,
                "statusline",
                format!(
                    "{lp} (mode {m}, last push {} ago, {fresh}, rate_limits {})",
                    age(c.received_at),
                    if has_limits { "present" } else { "missing" }
                ),
            );
        }
        Ok(None) => r.line(
            "info",
            "statusline",
            "no push yet (see `ocut install statusline`)",
        ),
        Err(e) => r.line("WARN", "statusline", format!("{lp}: {e}")),
    }
    let hook = std::fs::read_to_string(&paths.claude_settings)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .map(
            |v| match v.pointer("/statusLine/command").and_then(|c| c.as_str()) {
                Some(c) if c.contains("ocut") => "installed",
                Some(_) => "another statusLine is configured",
                None => "not installed",
            },
        )
        .unwrap_or("not installed");
    r.line("info", "hook", hook);

    // Source C.
    let start = Instant::now();
    let mut scanner = JsonlScanner::new(paths.project_dirs.clone(), Default::default());
    let existing = paths.project_dirs.iter().filter(|d| d.is_dir()).count();
    match scanner.scan() {
        Ok(s) => r.line(
            if s.files > 0 { "ok" } else { "WARN" },
            "local_logs",
            format!(
                "{existing}/{} dirs, {} files, {} usage events, {} parse errors, {} ms",
                paths.project_dirs.len(),
                s.files,
                s.events.len(),
                s.parse_errors,
                start.elapsed().as_millis()
            ),
        ),
        Err(e) => r.line("FAIL", "local_logs", e.to_string()),
    }
    match count_active_sessions(&paths.sessions_dir, Utc::now()) {
        Ok(n) => r.line("ok", "sessions", format!("{n} active")),
        Err(e) => r.line("WARN", "sessions", e.to_string()),
    }

    match claude_version() {
        Some(v) => r.line("ok", "claude", v),
        None => r.line("WARN", "claude", "`claude --version` failed or not on PATH"),
    }
    r.problems
}
