//! Source C: `~/.claude/projects/**/*.jsonl` (plus `$CLAUDE_CONFIG_DIR` and
//! `~/.config/claude/projects`).
//!
//! Keep `type == "assistant"` lines with `message.usage`; dedupe on
//! `message.id + requestId`; prefer logged cost (`costUSD`); ignore unknown
//! keys; never read message content. Tail incrementally using [`Offsets`]
//! (persisted to `~/.cache/opencut/offsets.json`), restarting a file from 0
//! when it is truncated or its inode changes. The line parser never panics.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::marker::PhantomData;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ocut_core::time::parse_timestamp;
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{Source, UsageEvent};

/// Per-field cap so absurd logged values cannot overflow later sums.
const MAX_TOKENS: u64 = u32::MAX as u64;

#[derive(Debug, thiserror::Error)]
pub enum JsonlError {
    #[error("log IO: {0}")]
    Io(#[from] std::io::Error),
}

/// Why a line was rejected. Never quotes line content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unparseable JSONL line: {0}")]
pub struct LineError(pub String);

/// Top-level line. Only the fields we need; `message.content` and every
/// other key are skipped by serde without being stored.
#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type", default)]
    kind: Option<Value>,
    #[serde(default)]
    timestamp: Option<Value>,
    #[serde(rename = "requestId", default)]
    request_id: Option<Value>,
    #[serde(rename = "sessionId", default)]
    session_id: Option<Value>,
    #[serde(rename = "costUSD", default)]
    cost_usd: Option<Value>,
    #[serde(default)]
    message: Option<MapOr<RawMessage>>,
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    model: Option<Value>,
    #[serde(default)]
    usage: Option<MapOr<Value>>,
}

/// `Some(T)` when the JSON value is an object, `None` for any other shape,
/// so an unexpected `message` type is ignored rather than buffered.
struct MapOr<T>(Option<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for MapOr<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = MapOr<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(|t| MapOr(Some(t)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(MapOr(None))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
                Ok(MapOr(None))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
                Ok(MapOr(None))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
                Ok(MapOr(None))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
                Ok(MapOr(None))
            }
            fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
                Ok(MapOr(None))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(MapOr(None))
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

fn as_str(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Lenient non-negative integer: numbers, numeric strings; else 0.
fn tokens(v: Option<&Value>) -> u64 {
    let n = match v {
        Some(Value::Number(n)) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|x| x.is_finite() && *x >= 0.0)
                .map(|x| x.min(MAX_TOKENS as f64) as u64)
        }),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    n.unwrap_or(0).min(MAX_TOKENS)
}

/// Parses one line. `Ok(None)` for valid lines that carry no usage
/// (user turns, summaries, assistant lines without `message.usage`).
pub fn parse_line(line: &str) -> Result<Option<UsageEvent>, LineError> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let raw: RawLine = serde_json::from_str(line)
        .map_err(|e| LineError(format!("{:?} at column {}", e.classify(), e.column())))?;
    if raw.kind.as_ref().and_then(Value::as_str) != Some("assistant") {
        return Ok(None);
    }
    let Some(msg) = raw.message.and_then(|m| m.0) else {
        return Ok(None);
    };
    let Some(usage) = msg.usage.and_then(|u| u.0) else {
        return Ok(None);
    };
    let model = as_str(msg.model.as_ref()).unwrap_or_else(|| "unknown".into());
    if model == "<synthetic>" {
        return Ok(None);
    }
    let timestamp = raw
        .timestamp
        .as_ref()
        .and_then(parse_timestamp)
        .ok_or_else(|| LineError("missing or invalid timestamp".into()))?;

    let cache_total = tokens(usage.get("cache_creation_input_tokens"));
    let split = usage.get("cache_creation").filter(|c| c.is_object());
    let (cw5, cw1) = match split {
        Some(c) => {
            let m5 = tokens(c.get("ephemeral_5m_input_tokens"));
            let h1 = tokens(c.get("ephemeral_1h_input_tokens"));
            // Anything in the undivided total not covered by the split is 5m.
            (m5 + cache_total.saturating_sub(m5 + h1), h1)
        }
        None => (cache_total, 0),
    };
    let cost_usd = raw
        .cost_usd
        .as_ref()
        .and_then(Value::as_f64)
        .filter(|c| c.is_finite() && *c >= 0.0);

    Ok(Some(UsageEvent {
        timestamp,
        model,
        message_id: as_str(msg.id.as_ref()),
        request_id: as_str(raw.request_id.as_ref()),
        session_id: as_str(raw.session_id.as_ref()),
        project: None,
        input_tokens: tokens(usage.get("input_tokens")),
        output_tokens: tokens(usage.get("output_tokens")),
        cache_write_5m_tokens: cw5,
        cache_write_1h_tokens: cw1,
        cache_read_tokens: tokens(usage.get("cache_read_input_tokens")),
        cost_usd,
    }))
}

/// Candidate `projects` dirs, existing or not, without duplicates.
/// `claude_config_dir` may hold several comma-separated dirs.
pub fn default_project_dirs(
    home: &Path,
    claude_config_dir: Option<&Path>,
    xdg_config_home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(cfg) = claude_config_dir.and_then(Path::to_str) {
        for d in cfg.split(',').map(str::trim).filter(|d| !d.is_empty()) {
            push(PathBuf::from(d).join("projects"));
        }
    }
    push(home.join(".claude").join("projects"));
    let xdg = xdg_config_home
        .filter(|p| p.is_absolute())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"));
    push(xdg.join("claude").join("projects"));
    out
}

/// All `*.jsonl` files under `dirs`, recursively, sorted. Missing dirs are
/// skipped; symlinked directories are not followed (no loops). Only an
/// unreadable top-level dir is an error; an unreadable subdirectory or entry
/// is skipped so one bad project cannot hide every other log.
pub fn discover_files(dirs: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>, top: bool) -> std::io::Result<()> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if top => return Err(e),
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                walk(&path, out, false)?;
            } else if path.extension().is_some_and(|e| e == "jsonl")
                && (ft.is_file() || path.is_file())
            {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    for d in dirs {
        walk(d, &mut out, true)?;
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Read position for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FileOffset {
    pub inode: u64,
    /// Byte offset just after the last complete line consumed.
    pub offset: u64,
}

/// Contents of `offsets.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Offsets {
    pub files: BTreeMap<PathBuf, FileOffset>,
}

impl Offsets {
    /// Loads `offsets.json`; a missing or corrupt file yields empty offsets
    /// (a full rescan is always safe).
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Writes atomically with mode 0600 (it lists project paths), creating
    /// the parent dir 0700 if missing.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let body = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        crate::fsutil::write_private_atomic(path, &body)
    }
}

/// `$XDG_CACHE_HOME/opencut/offsets.json`, else `~/.cache/opencut/offsets.json`.
pub fn default_offsets_path(xdg_cache_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    xdg_cache_home
        .filter(|p| p.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(".cache")))
        .map(|d| d.join("opencut").join("offsets.json"))
}

/// Result of one incremental scan.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// Events from bytes not seen before, deduped against earlier scans.
    pub events: Vec<UsageEvent>,
    /// Number of files tracked.
    pub files: u64,
    /// Lines rejected by [`parse_line`] in this scan.
    pub parse_errors: u64,
    /// Newest event timestamp seen in this scan (activity signal for polling).
    pub last_activity: Option<DateTime<Utc>>,
    /// Files skipped in this scan because they could not be read.
    pub unreadable: u64,
}

/// Incremental tailer over all project dirs.
///
/// Dedupe state lives in memory only: after a restart with persisted
/// offsets, old bytes are not re-read, so only new duplicates can appear.
#[derive(Debug, Default)]
pub struct JsonlScanner {
    pub dirs: Vec<PathBuf>,
    pub offsets: Offsets,
    seen: HashSet<String>,
}

impl JsonlScanner {
    pub fn new(dirs: Vec<PathBuf>, offsets: Offsets) -> Self {
        Self {
            dirs,
            offsets,
            seen: HashSet::new(),
        }
    }

    /// Parses only bytes appended since the last scan.
    pub fn scan(&mut self) -> Result<ScanResult, JsonlError> {
        let files = discover_files(&self.dirs)?;
        let mut res = ScanResult {
            files: files.len() as u64,
            ..Default::default()
        };
        // Forget files that disappeared.
        let live: HashSet<&PathBuf> = files.iter().collect();
        self.offsets.files.retain(|p, _| live.contains(p));

        for path in &files {
            // A file can vanish between discovery and open, or be unreadable.
            // Skip it rather than fail the scan: other files' offsets have
            // already advanced, so failing would lose their events. A file
            // that fails mid-read keeps its old offset and is retried.
            if let Err(e) = self.scan_file(path, &mut res) {
                if e.kind() == std::io::ErrorKind::NotFound {
                    self.offsets.files.remove(path);
                } else {
                    res.unreadable += 1;
                }
            }
        }
        res.last_activity = res.events.iter().map(|e| e.timestamp).max();
        Ok(res)
    }

    fn scan_file(&mut self, path: &Path, res: &mut ScanResult) -> std::io::Result<()> {
        let mut file = File::open(path)?;
        let meta = file.metadata()?;
        let prev = self.offsets.files.get(path).copied().unwrap_or_default();
        // Rotated (new inode) or truncated: start over.
        let start = if prev.inode == meta.ino() && prev.offset <= meta.len() {
            prev.offset
        } else {
            0
        };
        file.seek(SeekFrom::Start(start))?;
        let project = self.project_of(path);

        let mut reader = BufReader::new(file);
        let mut offset = start;
        let mut buf = Vec::new();
        let result = loop {
            buf.clear();
            let n = match reader.read_until(b'\n', &mut buf) {
                Ok(n) => n,
                Err(e) => break Err(e),
            };
            if n == 0 || buf.last() != Some(&b'\n') {
                break Ok(()); // EOF or partial trailing line: retry next scan.
            }
            offset += n as u64;
            let parsed = std::str::from_utf8(&buf)
                .map_err(|_| LineError("invalid UTF-8".into()))
                .and_then(parse_line);
            match parsed {
                Ok(Some(mut ev)) => {
                    if ev.dedup_key().is_none_or(|k| self.seen.insert(k)) {
                        ev.project.clone_from(&project);
                        res.events.push(ev);
                    }
                }
                Ok(None) => {}
                Err(_) => res.parse_errors += 1,
            }
        };
        // Record progress even after a read error, so lines already counted
        // are not read (and counted) again.
        self.offsets.files.insert(
            path.to_path_buf(),
            FileOffset {
                inode: meta.ino(),
                offset,
            },
        );
        result
    }

    /// First path component under the matching projects dir.
    fn project_of(&self, path: &Path) -> Option<String> {
        self.dirs.iter().find_map(|d| {
            let rel = path.strip_prefix(d).ok()?;
            let mut comps = rel.components();
            let first = comps.next()?;
            // A file directly in the projects dir has no project.
            comps.next()?;
            Some(first.as_os_str().to_string_lossy().into_owned())
        })
    }
}

impl Source for JsonlScanner {
    type Output = ScanResult;
    type Error = JsonlError;

    fn name(&self) -> &'static str {
        "local_logs"
    }

    fn poll(&mut self, _now: DateTime<Utc>) -> Result<ScanResult, JsonlError> {
        self.scan()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_object_message_is_ignored() {
        assert_eq!(
            parse_line(r#"{"type":"assistant","message":"hi"}"#),
            Ok(None)
        );
        assert_eq!(
            parse_line(r#"{"type":"assistant","message":{"usage":[1,2]}}"#),
            Ok(None)
        );
    }

    #[test]
    fn error_does_not_quote_content() {
        let e = parse_line(r#"{"type":"assistant","secret-text"#).unwrap_err();
        assert!(!e.to_string().contains("secret"));
    }

    #[test]
    fn unreadable_file_or_dir_does_not_fail_scan() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores modes
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let line = r#"{"type":"assistant","timestamp":"2026-10-11T12:00:00Z","requestId":"r1","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":1,"output_tokens":2}}}"#;
        for p in ["a", "b", "c"] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
        std::fs::write(root.join("a/s.jsonl"), format!("{line}\n")).unwrap();
        let locked = root.join("b/s.jsonl");
        std::fs::write(&locked, format!("{line}\n")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(root.join("c"), std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut s = JsonlScanner::new(vec![root.clone()], Offsets::default());
        let r = s.scan().unwrap();
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.unreadable, 1);
        std::fs::set_permissions(root.join("c"), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn offsets_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("opencut/offsets.json");
        Offsets::default().save(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(Offsets::load(&p), Offsets::default());
    }

    #[test]
    fn dirs_default_and_dedup() {
        let home = Path::new("/h");
        let d = default_project_dirs(home, Some(Path::new("/c1, /h/.claude")), None);
        assert_eq!(
            d,
            vec![
                PathBuf::from("/c1/projects"),
                PathBuf::from("/h/.claude/projects"),
                PathBuf::from("/h/.config/claude/projects"),
            ]
        );
        let d = default_project_dirs(home, None, Some(Path::new("/x")));
        assert_eq!(d[1], PathBuf::from("/x/claude/projects"));
    }
}
