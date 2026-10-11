//! `ocut install statusline`: previews the Claude Code `statusLine` entry.
//! Only `--apply` writes, after a backup, and never over an existing
//! `statusLine`. Only the `statusLine` key is ever printed, so other
//! settings (which may hold secrets in `env`) never reach the terminal.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

#[derive(Debug, PartialEq)]
pub enum Plan {
    /// `statusLine` already runs `ocut hook statusline`.
    AlreadyInstalled,
    /// Some other `statusLine` exists; never overwritten.
    Conflict,
    /// The settings with `statusLine` added.
    Add(Map<String, Value>),
}

pub fn snippet(ocut: &Path) -> Value {
    json!({
        "type": "command",
        "command": format!("{} hook statusline", ocut.display()),
        "padding": 0
    })
}

/// Decides what `--apply` would do. `settings` is the file content, `None`
/// when the file does not exist.
pub fn plan(settings: Option<&str>, ocut: &Path) -> Result<Plan, String> {
    let mut obj = match settings.map(str::trim) {
        None | Some("") => Map::new(),
        Some(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(m)) => m,
            Ok(_) => return Err("settings.json is not a JSON object".into()),
            Err(e) => {
                return Err(format!(
                    "settings.json is not valid JSON (line {}, column {})",
                    e.line(),
                    e.column()
                ));
            }
        },
    };
    if let Some(existing) = obj.get("statusLine") {
        let ours = existing
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| c.contains("ocut") && c.contains("hook statusline"));
        return Ok(if ours {
            Plan::AlreadyInstalled
        } else {
            Plan::Conflict
        });
    }
    obj.insert("statusLine".into(), snippet(ocut));
    Ok(Plan::Add(obj))
}

/// A diff limited to the added key.
pub fn diff(display_path: &str, ocut: &Path) -> String {
    let body =
        serde_json::to_string_pretty(&json!({ "statusLine": snippet(ocut) })).expect("serialises");
    let mut out =
        format!("--- {display_path}\n+++ {display_path} (with OpenCUT)\n@@ top level @@\n");
    let inner: Vec<&str> = body.lines().collect();
    out.push_str(" {\n");
    for l in &inner[1..inner.len() - 1] {
        out.push_str(&format!("+{l}\n"));
    }
    out.push_str("   ...your other settings, unchanged...\n }\n");
    out
}

/// Writes the planned settings: backs up the original (0600) and replaces
/// the file atomically, keeping its permission bits. A symlinked
/// `settings.json` (e.g. from a dotfiles repo) is written through, so the
/// link survives. Returns the backup path, if a file existed.
pub fn apply(path: &Path, settings: &Map<String, Value>) -> std::io::Result<Option<PathBuf>> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path = target.as_path();
    let dir = path.parent().unwrap_or(Path::new("."));
    let backup = if path.exists() {
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let b = dir.join(format!("settings.json.opencut-backup-{stamp}"));
        let mut o = fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        o.open(&b)?.write_all(&fs::read(path)?)?;
        Some(b)
    } else {
        None
    };
    let perms = fs::metadata(path).ok().map(|m| m.permissions());
    let tmp = dir.join(format!(".settings.json.opencut-{}.tmp", std::process::id()));
    let mut body = serde_json::to_string_pretty(&Value::Object(settings.clone()))
        .map_err(std::io::Error::other)?;
    body.push('\n');
    let r = (|| {
        // Private until renamed: the settings may hold secrets in `env`.
        let mut o = fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = o.open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        if let Some(p) = perms {
            fs::set_permissions(&tmp, p)?;
        }
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r.map(|()| backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OCUT: &str = "/usr/bin/ocut";

    #[test]
    fn adds_and_keeps_order() {
        let Plan::Add(m) = plan(Some(r#"{"z":1,"a":{"b":2}}"#), Path::new(OCUT)).unwrap() else {
            panic!()
        };
        let keys: Vec<_> = m.keys().cloned().collect();
        assert_eq!(keys, ["z", "a", "statusLine"]);
        assert_eq!(m["statusLine"]["command"], "/usr/bin/ocut hook statusline");
        assert!(matches!(
            plan(None, Path::new(OCUT)).unwrap(),
            Plan::Add(m) if m.len() == 1
        ));
    }

    #[test]
    fn never_overwrites() {
        let other = r#"{"statusLine":{"type":"command","command":"~/bin/mine"}}"#;
        assert_eq!(plan(Some(other), Path::new(OCUT)).unwrap(), Plan::Conflict);
        let ours = r#"{"statusLine":{"type":"command","command":"/x/ocut hook statusline"}}"#;
        assert_eq!(
            plan(Some(ours), Path::new(OCUT)).unwrap(),
            Plan::AlreadyInstalled
        );
    }

    #[test]
    fn invalid_json_is_not_echoed() {
        let e = plan(Some(r#"{"env":{"KEY":"secret-value"#), Path::new(OCUT)).unwrap_err();
        assert!(!e.contains("secret"));
        assert!(plan(Some("[1]"), Path::new(OCUT)).is_err());
    }

    #[test]
    fn diff_shows_only_statusline() {
        let d = diff("~/.claude/settings.json", Path::new(OCUT));
        assert!(d.contains("+  \"statusLine\": {"));
        assert!(d.contains("hook statusline"));
    }

    #[cfg(unix)]
    #[test]
    fn apply_backs_up_and_keeps_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("settings.json");
        fs::write(&p, r#"{"model":"x"}"#).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        let orig = fs::read_to_string(&p).unwrap();
        let Plan::Add(m) = plan(Some(&orig), Path::new(OCUT)).unwrap() else {
            panic!()
        };
        let backup = apply(&p, &m).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&backup).unwrap(), orig);
        assert_eq!(
            fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let new: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(new["model"], "x");
        assert!(new["statusLine"].is_object());
        // A second run sees our entry and refuses to add another.
        assert_eq!(
            plan(Some(&fs::read_to_string(&p).unwrap()), Path::new(OCUT)).unwrap(),
            Plan::AlreadyInstalled
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_writes_through_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("dotfiles-settings.json");
        let link = tmp.path().join("settings.json");
        fs::write(&real, r#"{"model":"x"}"#).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let Plan::Add(m) = plan(Some(r#"{"model":"x"}"#), Path::new(OCUT)).unwrap() else {
            panic!()
        };
        apply(&link, &m).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let new: Value = serde_json::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
        assert!(new["statusLine"].is_object());
        // New file without a previous one is created private.
        let fresh = tmp.path().join("new").join("settings.json");
        fs::create_dir(fresh.parent().unwrap()).unwrap();
        apply(&fresh, &m).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            plan(Some(&fs::read_to_string(&link).unwrap()), Path::new(OCUT)).unwrap(),
            Plan::AlreadyInstalled
        );
    }
}
