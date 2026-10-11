//! Active session count from `~/.claude/sessions/*.json`.
//!
//! Each file describes one running Claude Code process: `pid`, `procStart`
//! (the process start time in clock ticks, field 22 of `/proc/<pid>/stat`)
//! and `updatedAt` (epoch ms). Only those fields are read.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use ocut_core::time::parse_timestamp;
use serde::Deserialize;
use serde_json::Value;

use crate::Source;

/// Without `/proc`, a session counts as live if updated this recently.
pub const FALLBACK_FRESHNESS: Duration = Duration::minutes(15);

#[derive(Deserialize, Default)]
#[serde(default)]
struct SessionMeta {
    pid: Option<Value>,
    #[serde(rename = "procStart")]
    proc_start: Option<Value>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<Value>,
}

/// Counts sessions considered live at `now`. Reads only metadata needed to
/// decide liveness (pid / timestamps), never content. A missing directory
/// counts as zero.
pub fn count_active_sessions(sessions_dir: &Path, now: DateTime<Utc>) -> std::io::Result<u32> {
    let has_proc = Path::new("/proc/self/stat").exists();
    count_with(sessions_dir, now, |pid, start| {
        has_proc.then(|| proc_alive(pid, start))
    })
}

/// `is_alive(pid, procStart)` returns `None` when liveness can't be checked,
/// in which case `updatedAt` freshness decides.
fn count_with(
    dir: &Path,
    now: DateTime<Utc>,
    is_alive: impl Fn(u32, Option<&str>) -> Option<bool>,
) -> std::io::Result<u32> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut n = 0;
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(meta) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SessionMeta>(&b).ok())
        else {
            continue;
        };
        let pid = meta
            .pid
            .as_ref()
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok())
            .filter(|p| *p > 0);
        let start = match &meta.proc_start {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        let alive = pid.and_then(|p| is_alive(p, start.as_deref()));
        let live = alive.unwrap_or_else(|| {
            meta.updated_at
                .as_ref()
                .and_then(parse_timestamp)
                .is_some_and(|t| now - t <= FALLBACK_FRESHNESS)
        });
        if live {
            n += 1;
        }
    }
    Ok(n)
}

/// True if `/proc/<pid>` exists and, when known, its start time matches
/// (guards against pid reuse).
fn proc_alive(pid: u32, proc_start: Option<&str>) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(want) = proc_start else {
        return true;
    };
    // The comm field may contain spaces; fields after the last ')' are fixed.
    // Field 22 (starttime) is the 20th field after it.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .is_some_and(|got| got == want.trim())
}

#[derive(Debug, Clone)]
pub struct SessionsSource {
    pub dir: PathBuf,
}

impl Source for SessionsSource {
    type Output = u32;
    type Error = std::io::Error;

    fn name(&self) -> &'static str {
        "sessions"
    }

    fn poll(&mut self, now: DateTime<Utc>) -> std::io::Result<u32> {
        count_active_sessions(&self.dir, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sessions")
    }

    #[test]
    fn counts_by_pid_check() {
        let now = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        // Fixture pids: 1001 (start 500), 1002 (start 600), 1003 (no procStart).
        let n = count_with(&fixtures(), now, |pid, start| {
            Some(matches!((pid, start), (1001, Some("500")) | (1003, None)))
        })
        .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn falls_back_to_updated_at() {
        // 1001 updated 5 min before `now`, others older or missing.
        let now = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        assert_eq!(count_with(&fixtures(), now, |_, _| None).unwrap(), 1);
    }

    #[test]
    fn missing_dir_is_zero() {
        let now = Utc::now();
        assert_eq!(
            count_active_sessions(Path::new("/nonexistent/opencut"), now).unwrap(),
            0
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn own_process_is_alive() {
        let pid = std::process::id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let start = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .to_owned();
        assert!(proc_alive(pid, Some(&start)));
        assert!(proc_alive(pid, None));
        assert!(!proc_alive(pid, Some("1")));
    }
}
