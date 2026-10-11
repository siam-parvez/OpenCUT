//! `ocutd --once` against sanitized fixtures: writes a private snapshot.

use std::path::PathBuf;
use std::process::Command;

#[cfg(unix)]
#[test]
fn once_writes_private_snapshot() {
    use std::os::unix::fs::PermissionsExt;
    let h = tempfile::tempdir().unwrap();
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let cfg = h.path().join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            "[sources]\noauth_api = \"off\"\nclaude_dirs = [{:?}]\n",
            fixtures.join("jsonl")
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ocutd"))
        .env_clear()
        .env("HOME", h.path())
        .env("XDG_RUNTIME_DIR", h.path().join("run"))
        .args(["--once", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snap = h.path().join("run/opencut/usage.json");
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&snap), 0o600);
    assert_eq!(mode(snap.parent().unwrap()), 0o700);
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&snap).unwrap()).unwrap();
    assert_eq!(v["schema"], "opencut/usage@1");
    assert!(v["sources"]["local_logs"]["files"].as_u64().unwrap() >= 3);
    assert_eq!(v["sources"]["oauth_api"]["enabled"], false);
}
