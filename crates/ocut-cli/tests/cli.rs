//! End-to-end runs of the `ocut` binary against sanitized fixtures in a
//! temporary HOME. Never touches the real ~/.claude and never hits the network.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use serde_json::Value;

const SCHEMA: &str = include_str!("../../../schema/usage.v1.json");

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

/// A HOME with fixture logs, no credentials and oauth_api explicitly off.
fn home() -> tempfile::TempDir {
    let h = tempfile::tempdir().unwrap();
    copy_dir(
        &fixtures().join("jsonl/projects"),
        &h.path().join(".claude/projects"),
    );
    let cfg = h.path().join(".config/opencut");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.toml"), "[sources]\noauth_api = \"off\"\n").unwrap();
    h
}

fn ocut(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ocut"));
    c.env_clear()
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null());
    c
}

fn assert_valid(v: &Value) {
    let schema: Value = serde_json::from_str(SCHEMA).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:#?}");
}

#[test]
fn no_daemon_json_is_schema_valid() {
    let h = home();
    let out = ocut(h.path())
        .args(["--no-daemon", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_valid(&v);
    assert_eq!(v["schema"], "opencut/usage@1");
    assert_eq!(v["sources"]["oauth_api"]["enabled"], false);
    assert!(v["sources"]["local_logs"]["files"].as_u64().unwrap() >= 3);
    assert!(v["windows"]["weekly_all"].is_null());
}

#[test]
fn format_and_waybar() {
    let h = home();
    let out = ocut(h.path())
        .args(["--format", "{5h.pct:.0}|{7d.pct}|{status}"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.trim_end().ends_with("|n/a|ok"), "{s}");

    let out = ocut(h.path()).arg("--waybar").output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["text"].is_string() && v["tooltip"].is_string());
    assert!(["ok", "warn", "crit", "stale"].contains(&v["class"].as_str().unwrap()));

    let out = ocut(h.path())
        .args(["--format", "{bogus}"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn reads_daemon_snapshot_when_fresh() {
    let h = home();
    let example = std::fs::read_to_string(fixtures().join("snapshot/usage.v1.example.json"))
        .unwrap()
        .replace(
            "\"generated_at\": \"2026-10-11T12:30:05Z\"",
            &format!("\"generated_at\": \"{}\"", chrono::Utc::now().to_rfc3339()),
        );
    std::fs::create_dir_all(h.path().join("run/opencut")).unwrap();
    std::fs::write(h.path().join("run/opencut/usage.json"), example).unwrap();
    let start = Instant::now();
    let out = ocut(h.path())
        .args(["--format", "{5h.source}"])
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "oauth_api");
    eprintln!("cached call took {elapsed:?}");
    // --no-daemon ignores it.
    let out = ocut(h.path())
        .args(["--no-daemon", "--format", "{5h.source}"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        "local_estimate"
    );
}

fn hook(home: &Path, stdin: &str) -> (String, std::time::Duration) {
    let start = Instant::now();
    let mut child = ocut(home)
        .args(["hook", "statusline"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    (String::from_utf8(out.stdout).unwrap(), start.elapsed())
}

#[test]
fn hook_statusline_writes_cache_and_prints_line() {
    let h = home();
    let input = std::fs::read_to_string(fixtures().join("statusline/full.json")).unwrap();
    let (line, took) = hook(h.path(), &input);
    assert!(line.contains("5h"), "{line}");
    assert!(took.as_millis() < 500, "hook took {took:?}");
    eprintln!("hook took {took:?}");
    let cache = h.path().join(".cache/opencut/statusline.json");
    let body = std::fs::read_to_string(&cache).unwrap();
    assert!(!body.contains("transcript") && !body.contains("session_id"));

    let input = std::fs::read_to_string(fixtures().join("statusline/no_rate_limits.json")).unwrap();
    let (line, _) = hook(h.path(), &input);
    assert!(line.contains("n/a"), "{line}");
    let (line, _) = hook(h.path(), "not json");
    assert!(line.contains("n/a"), "{line}");

    // The cached statusline now feeds the snapshot (source B).
    let out = ocut(h.path())
        .args(["--no-daemon", "--json"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_valid(&v);
    assert!(v["sources"]["statusline"]["last_push"].is_string());
}

#[test]
fn install_statusline_previews_only() {
    let h = home();
    let settings = h.path().join(".claude/settings.json");
    std::fs::write(&settings, r#"{"env":{"SECRET_KEY":"canary-123"}}"#).unwrap();
    let out = ocut(h.path())
        .args(["install", "statusline"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.contains("hook statusline") && s.contains("Nothing was written"));
    assert!(!s.contains("canary-123"));
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        r#"{"env":{"SECRET_KEY":"canary-123"}}"#
    );
}

#[test]
fn install_apply_refuses_existing_statusline() {
    let h = home();
    let settings = h.path().join(".claude/settings.json");
    let orig = r#"{"statusLine":{"type":"command","command":"mine"}}"#;
    std::fs::write(&settings, orig).unwrap();
    let out = ocut(h.path())
        .args(["install", "statusline", "--apply"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), orig);
}

#[test]
fn doctor_never_prints_secrets() {
    let h = home();
    let creds = h.path().join(".claude/.credentials.json");
    std::fs::write(
        &creds,
        r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-CANARY","refreshToken":"sk-ant-ort01-CANARY","expiresAt":1,"subscriptionType":"pro"}}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::write(
        h.path().join(".config/opencut/config.toml"),
        "[sources]\noauth_api = \"on\"\n",
    )
    .unwrap();
    let out = ocut(h.path()).arg("doctor").output().unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!all.contains("CANARY"), "{all}");
    assert!(all.contains("credentials") && all.contains("0600"), "{all}");
    assert!(all.contains("token expired"), "{all}");
    assert!(all.contains("local_logs"));
}
