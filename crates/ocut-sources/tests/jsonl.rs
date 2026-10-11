use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use ocut_core::event::sum_tokens;
use ocut_sources::jsonl::{
    JsonlScanner, Offsets, default_offsets_path, discover_files, parse_line,
};
use proptest::prelude::*;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/jsonl/projects")
}

/// Fresh copy of the fixture tree so tests can mutate files.
fn temp_projects(name: &str) -> PathBuf {
    let dst = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("jsonl-{name}"));
    let _ = fs::remove_dir_all(&dst);
    copy_dir(&fixture_root(), &dst);
    dst
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            fs::copy(e.path(), &to).unwrap();
        }
    }
}

const NEW_LINE: &str = r#"{"type":"assistant","sessionId":"s","requestId":"req_new","timestamp":"2026-10-11T12:00:00Z","message":{"id":"msg_new","model":"claude-opus-5-5","usage":{"input_tokens":5,"output_tokens":6}}}"#;

#[test]
fn parses_assistant_usage_with_cache_split() {
    let line = fs::read_to_string(fixture_root().join("-home-user-demo/sess-a.jsonl")).unwrap();
    let ev = parse_line(line.lines().nth(2).unwrap()).unwrap().unwrap();
    assert_eq!(ev.model, "claude-opus-5-5");
    assert_eq!(ev.dedup_key().as_deref(), Some("msg_1:req_1"));
    assert_eq!(ev.session_id.as_deref(), Some("sess-a"));
    assert_eq!(
        (ev.input_tokens, ev.output_tokens, ev.cache_read_tokens),
        (10, 100, 5000)
    );
    assert_eq!(
        (ev.cache_write_5m_tokens, ev.cache_write_1h_tokens),
        (400, 600)
    );
    assert_eq!(ev.cost_usd, None);
}

#[test]
fn undivided_cache_write_is_5m_and_cost_kept() {
    let ev = parse_line(
        r#"{"type":"assistant","timestamp":"2026-10-11T09:10:00Z","costUSD":0.25,"message":{"model":"m","usage":{"cache_creation_input_tokens":300}}}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (ev.cache_write_5m_tokens, ev.cache_write_1h_tokens),
        (300, 0)
    );
    assert_eq!(ev.cost_usd, Some(0.25));
    assert_eq!(ev.dedup_key(), None);
}

#[test]
fn non_usage_lines_are_none() {
    for l in [
        "",
        "   ",
        r#"{"type":"user","message":{"content":"x"}}"#,
        r#"{"type":"assistant","message":{"content":[]}}"#,
        r#"{"type":"assistant","message":{"model":"<synthetic>","usage":{}}}"#,
        r#"{"no_type":true}"#,
    ] {
        assert_eq!(parse_line(l), Ok(None), "{l}");
    }
    assert!(parse_line("{").is_err());
    assert!(parse_line(r#"{"type":"assistant","message":{"usage":{}}}"#).is_err());
}

#[test]
fn full_scan_dedupes_and_counts() {
    let root = fixture_root();
    let mut s = JsonlScanner::new(vec![root], Offsets::default());
    let r = s.scan().unwrap();
    assert_eq!(r.files, 3);
    assert_eq!(r.parse_errors, 1);
    // msg_1 appears twice in sess-a and again in sess-c: counted once.
    assert_eq!(r.events.len(), 4);
    let t = sum_tokens(&r.events);
    assert_eq!(t.input, 10 + 20 + 7 + 1);
    assert_eq!(t.output, 100 + 200 + 3 + 2);
    assert_eq!(t.cache_write, 1000 + 300);
    assert_eq!(t.cache_read, 5000);
    assert_eq!(
        r.last_activity.unwrap().to_rfc3339(),
        "2026-10-11T11:30:00+00:00"
    );
    let projects: Vec<_> = r.events.iter().map(|e| e.project.as_deref()).collect();
    assert!(projects.contains(&Some("-home-user-demo")));
    assert!(projects.contains(&Some("-home-user-other")));

    // Second scan reads nothing new.
    let r2 = s.scan().unwrap();
    assert!(r2.events.is_empty());
    assert_eq!(r2.parse_errors, 0);
}

#[test]
fn incremental_append_and_partial_line() {
    let dir = temp_projects("append");
    let file = dir.join("-home-user-other/sess-c.jsonl");
    let mut s = JsonlScanner::new(vec![dir.clone()], Offsets::default());
    s.scan().unwrap();
    let before = s.offsets.files[&file].offset;

    // A half-written line is left for the next scan.
    let (head, tail) = NEW_LINE.split_at(40);
    let mut f = OpenOptions::new().append(true).open(&file).unwrap();
    f.write_all(head.as_bytes()).unwrap();
    let r = s.scan().unwrap();
    assert!(r.events.is_empty());
    assert_eq!(r.parse_errors, 0);
    assert_eq!(s.offsets.files[&file].offset, before);

    writeln!(f, "{tail}").unwrap();
    let r = s.scan().unwrap();
    assert_eq!(r.events.len(), 1);
    assert_eq!(r.events[0].message_id.as_deref(), Some("msg_new"));
    assert_eq!(
        s.offsets.files[&file].offset,
        fs::metadata(&file).unwrap().len()
    );
}

#[test]
fn offsets_persist_across_restart() {
    let dir = temp_projects("persist");
    let path = dir.join("../offsets-persist/offsets.json");
    let mut s = JsonlScanner::new(vec![dir.clone()], Offsets::default());
    assert_eq!(s.scan().unwrap().events.len(), 4);
    s.offsets.save(&path).unwrap();

    let mut s2 = JsonlScanner::new(vec![dir.clone()], Offsets::load(&path));
    assert!(s2.scan().unwrap().events.is_empty());

    // Corrupt offsets file: full rescan, no error.
    fs::write(&path, "garbage").unwrap();
    let mut s3 = JsonlScanner::new(vec![dir], Offsets::load(&path));
    assert_eq!(s3.scan().unwrap().events.len(), 4);
}

#[test]
fn truncation_restarts_from_zero() {
    let dir = temp_projects("truncate");
    let file = dir.join("-home-user-other/sess-c.jsonl");
    let mut s = JsonlScanner::new(vec![dir], Offsets::default());
    s.scan().unwrap();
    fs::write(&file, format!("{NEW_LINE}\n")).unwrap();
    let r = s.scan().unwrap();
    assert_eq!(r.events.len(), 1);
    assert_eq!(r.events[0].message_id.as_deref(), Some("msg_new"));
}

#[test]
fn rotation_new_inode_restarts_from_zero() {
    let dir = temp_projects("rotate");
    let file = dir.join("-home-user-other/sess-c.jsonl");
    let mut s = JsonlScanner::new(vec![dir.clone()], Offsets::default());
    s.scan().unwrap();
    // Replace with a longer file under a new inode.
    let original = fs::read_to_string(&file).unwrap();
    let tmp = dir.join("-home-user-other/rotated.tmp");
    fs::write(&tmp, format!("{original}{NEW_LINE}\n{original}")).unwrap();
    fs::rename(&tmp, &file).unwrap();
    let r = s.scan().unwrap();
    // Old lines are re-read but deduped; only the new message is emitted.
    assert_eq!(r.events.len(), 1);
    assert_eq!(r.events[0].message_id.as_deref(), Some("msg_new"));
}

#[test]
fn deleted_files_are_forgotten() {
    let dir = temp_projects("delete");
    let file = dir.join("-home-user-other/sess-c.jsonl");
    let mut s = JsonlScanner::new(vec![dir], Offsets::default());
    s.scan().unwrap();
    fs::remove_file(&file).unwrap();
    let r = s.scan().unwrap();
    assert_eq!(r.files, 2);
    assert!(!s.offsets.files.contains_key(&file));
}

#[test]
fn discovery_skips_missing_dirs_and_other_files() {
    let dir = temp_projects("discover");
    fs::write(dir.join("-home-user-other/notes.txt"), "x").unwrap();
    let files = discover_files(&[dir.clone(), dir.join("does-not-exist"), dir.clone()]).unwrap();
    assert_eq!(files.len(), 3);
    assert!(files.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn offsets_path() {
    assert_eq!(
        default_offsets_path(None, Some(Path::new("/h"))),
        Some(PathBuf::from("/h/.cache/opencut/offsets.json"))
    );
    assert_eq!(
        default_offsets_path(Some(Path::new("/x")), Some(Path::new("/h"))),
        Some(PathBuf::from("/x/opencut/offsets.json"))
    );
}

/// Valid-looking assistant lines with fields of random JSON types.
fn json_leaf() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("null".to_string()),
        any::<bool>().prop_map(|b| b.to_string()),
        any::<i64>().prop_map(|n| n.to_string()),
        any::<u64>().prop_map(|n| n.to_string()),
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(|f| format!("{f:e}")),
        "[a-z0-9 :.-]{0,12}".prop_map(|s| format!("\"{s}\"")),
        Just("[]".to_string()),
        Just("{}".to_string()),
        Just("[1,{\"a\":2}]".to_string()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn random_bytes_never_panic(s in any::<String>()) {
        let _ = parse_line(&s);
    }

    #[test]
    fn mutated_real_line_never_panics(
        cut in 0usize..400,
        insert in "[\\[\\]{}\":,0-9a-z\\\\]{0,6}",
    ) {
        let mut l = NEW_LINE.to_string();
        let mut i = cut.min(l.len());
        while !l.is_char_boundary(i) { i -= 1; }
        l.insert_str(i, &insert);
        let _ = parse_line(&l);
        let _ = parse_line(&l[..i]);
    }

    #[test]
    fn random_field_types_never_panic(
        ty in prop_oneof![Just("\"assistant\"".to_string()), json_leaf()],
        ts in json_leaf(), msg_id in json_leaf(), model in json_leaf(),
        input in json_leaf(), output in json_leaf(), cw in json_leaf(),
        split in json_leaf(), cost in json_leaf(), req in json_leaf(),
    ) {
        let line = format!(
            r#"{{"type":{ty},"timestamp":{ts},"requestId":{req},"costUSD":{cost},"message":{{"id":{msg_id},"model":{model},"usage":{{"input_tokens":{input},"output_tokens":{output},"cache_creation_input_tokens":{cw},"cache_creation":{{"ephemeral_5m_input_tokens":{split},"ephemeral_1h_input_tokens":{split}}}}}}}}}"#
        );
        if let Ok(Some(ev)) = parse_line(&line) {
            // Sums must not overflow downstream.
            let _ = ev.total_tokens();
            prop_assert!(ev.cost_usd.is_none_or(|c| c.is_finite() && c >= 0.0));
        }
    }
}
