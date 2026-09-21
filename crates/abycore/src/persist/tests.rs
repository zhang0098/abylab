//! Offline persistence tests: temp workspaces, no network.

use super::*;
use crate::{Item, ModelOptions, RequestPurpose, RequestRecord, Usage};
fn temp_workspace(name: &str) -> PathBuf {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "abycore-persist-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp workspace");
    dir
}

#[test]
fn lazy_materialization_and_load_roundtrip() {
    let workspace = temp_workspace("roundtrip");
    let store = SessionStore::new(&workspace).expect("store");

    // No sessions yet: the data dir may not exist on disk.
    assert!(store.list().unwrap().is_empty());

    let id = "s-1";
    let mut writer = store.create(id, &store_snapshot("seed")).expect("writer");
    assert!(
        !store.log_path(id).unwrap().exists(),
        "lazy materialization: no file before the first append"
    );

    let snapshot = store_snapshot("hello abycli");
    store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    store.set_title(&mut writer, "demo session").unwrap();
    let mut snapshot2 = store_snapshot("hello abycli");
    snapshot2.run_sequence = 1;
    store.append_checkpoint(&mut writer, 1, &snapshot2).unwrap();
    drop(writer);

    let summaries = store.list().unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, id);
    assert_eq!(summaries[0].title.as_deref(), Some("demo session"));
    assert_eq!(summaries[0].preview, "hello abycli");

    let (header, loaded) = store.load(id).unwrap();
    assert_eq!(header.id, id);
    assert_eq!(loaded.items.len(), 1);
    assert_eq!(loaded.items[0], Item::user("hello abycli"));
    assert_eq!(loaded.run_sequence, 1, "latest checkpoint wins");
}

#[test]
fn torn_tail_is_dropped_on_load() {
    let workspace = temp_workspace("torn");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("torn", &store_snapshot("x")).unwrap();
    let snapshot = store_snapshot("first user");
    store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    // Simulate a torn final line (no trailing newline, invalid JSON).
    let file = writer.path().to_path_buf();
    drop(writer);
    append_bytes(
        &file,
        b"{\"type\":\"snapshot\",\"seq\":1,\"snapshot\":{\"to",
    );
    let (_, loaded) = store.load("torn").unwrap();
    assert_eq!(loaded.run_sequence, 0);
    assert_eq!(loaded.items[0], Item::user("first user"));
    let mut writer = store.create("torn", &loaded).unwrap();
    let mut next = loaded;
    next.run_sequence = 1;
    store.append_checkpoint(&mut writer, 1, &next).unwrap();
    drop(writer);
    assert_eq!(store.load("torn").unwrap().1, next);
}

fn append_bytes(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[test]
fn dropping_writer_unlocks_even_while_a_duplicate_descriptor_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let snapshot = store_snapshot("seed");
    let writer = store.create("s", &snapshot).unwrap();
    // A duplicate shares the flock just like a descriptor inherited across fork.
    let duplicate = writer.lock.try_clone().unwrap();
    assert!(store.create("s", &snapshot).is_err());
    drop(writer);
    let next = store.create("s", &snapshot).unwrap();
    drop(duplicate);
    assert!(store.create("s", &snapshot).is_err());
    drop(next);
    assert!(store.create("s", &snapshot).is_ok());
}

#[test]
fn valid_json_without_newline_is_uncommitted_for_reader_and_writer() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let first = store_snapshot("first");
    let mut writer = store.create("s", &first).unwrap();
    store.append_checkpoint(&mut writer, 0, &first).unwrap();
    let file = writer.path().to_owned();
    let committed_len = fs::metadata(&file).unwrap().len();
    drop(writer);
    append_bytes(
        &file,
        serde_json::json!({"type":"snapshot", "seq":1, "snapshot":store_snapshot("uncommitted")})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(store.load("s").unwrap().1, first);
    let mut writer = store.create("s", &first).unwrap();
    assert_eq!(fs::metadata(&file).unwrap().len(), committed_len);
    assert_eq!(writer.truncate_to_last_line().unwrap(), committed_len);
    assert_eq!(store.load("s").unwrap().1, first);
    store
        .append_checkpoint(&mut writer, 1, &store_snapshot("next"))
        .unwrap();
    assert_eq!(
        store.load("s").unwrap().1.items,
        store_snapshot("next").items
    );
}

#[test]
fn complete_corrupt_or_unknown_records_never_restore_older_state() {
    let mut unknown = serde_json::to_value(store_snapshot("newer")).unwrap();
    unknown["new_required_field"] = serde_json::json!(true);
    let mut invalid_snapshot = store_snapshot("invalid");
    invalid_snapshot.needs_response = false;
    let records = [
        b"{broken}\n".to_vec(),
        b"{\"type\":\"snapshot\"}\n".to_vec(),
        b"{\"type\":\"new_event\"}\n".to_vec(),
        b"{\"type\":\"title\",\"title\":\"\xff\"}\n".to_vec(),
        format!(
            "{}\n",
            serde_json::json!({"type":"snapshot", "seq":1, "snapshot":unknown})
        )
        .into_bytes(),
        format!(
            "{}\n",
            serde_json::json!({"type":"snapshot", "seq":1, "snapshot":invalid_snapshot})
        )
        .into_bytes(),
    ];
    for record in records {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path()).unwrap();
        let first = store_snapshot("first");
        let mut writer = store.create("s", &first).unwrap();
        store.append_checkpoint(&mut writer, 0, &first).unwrap();
        let file = writer.path().to_owned();
        drop(writer);
        append_bytes(&file, &record);
        let before = fs::read(&file).unwrap();
        assert!(store.load("s").is_err());
        assert!(store.create("s", &first).is_err());
        assert_eq!(
            fs::read(&file).unwrap(),
            before,
            "complete corruption must not be truncated"
        );
    }
}

#[test]
fn incomplete_or_empty_initial_header_can_be_retried() {
    for bytes in [b"".as_slice(), b"{\"type\":\"session\""] {
        for reopen in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = SessionStore::new(dir.path()).unwrap();
            let first = store_snapshot("first");
            let mut writer = store.create("s", &first).unwrap();
            fs::write(writer.path(), bytes).unwrap();
            if reopen {
                drop(writer);
                writer = store.create("s", &first).unwrap();
            }
            store.append_checkpoint(&mut writer, 0, &first).unwrap();
            assert_eq!(store.load("s").unwrap().1, first);
            assert_eq!(
                fs::read_to_string(writer.path()).unwrap().lines().count(),
                2
            );
        }
    }
}

#[test]
fn dot_ids_are_distinct_and_discoverable_inside_sessions_root() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    for id in [".", "..", "~2E", "~2E~2E", "a/b", "中"] {
        let snapshot = store_snapshot(id);
        let mut writer = store.create(id, &snapshot).unwrap();
        store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
        let actual = fs::canonicalize(writer.path()).unwrap();
        assert_eq!(actual.parent().unwrap().parent().unwrap(), store.root());
        assert_eq!(store.load(id).unwrap().1, snapshot);
    }
    assert_eq!(store.list().unwrap().len(), 6);
}

#[test]
fn listing_orders_log_activity_instead_of_directory_activity() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    for (id, log_time, dir_time) in [("recent", 20, 1), ("older", 10, 30)] {
        let snapshot = store_snapshot(id);
        let mut writer = store.create(id, &snapshot).unwrap();
        store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
        let file = writer.path().to_owned();
        drop(writer);
        fs::File::open(&file)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(log_time)),
            )
            .unwrap();
        fs::File::open(file.parent().unwrap())
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(dir_time)),
            )
            .unwrap();
    }
    let summaries = store.list().unwrap();
    assert_eq!(
        summaries.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["recent", "older"]
    );
    for summary in summaries {
        assert_eq!(
            summary.modified,
            fs::metadata(summary.file).unwrap().modified().unwrap()
        );
    }
}

#[test]
fn discovery_rebuilds_missing_invalid_and_stale_indexes_without_losing_titles() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let snapshot = store_snapshot("preview");
    let mut writer = store.create("s", &snapshot).unwrap();
    store.set_title(&mut writer, "before checkpoint").unwrap();
    assert_eq!(store.title_of("s").as_deref(), Some("before checkpoint"));
    store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    let file = writer.path().to_owned();
    let dir = file.parent().unwrap();
    drop(writer);
    fs::remove_file(dir.join("summary.json")).unwrap();
    let first = store.list().unwrap().remove(0);
    assert_eq!(first.preview, "preview");
    assert_eq!(first.title.as_deref(), Some("before checkpoint"));
    assert!(dir.join("summary.json").exists());
    fs::write(dir.join("summary.json"), b"broken").unwrap();
    assert_eq!(store.list().unwrap()[0].preview, "preview");
    append_bytes(
        &file,
        b"{\"type\":\"title\",\"title\":\"changed externally\"}\n",
    );
    assert_eq!(
        store.list().unwrap()[0].title.as_deref(),
        Some("changed externally")
    );
    assert_eq!(store.title_of("s").as_deref(), Some("changed externally"));
}

#[test]
fn cached_discovery_does_not_parse_transcript_but_load_still_validates_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let snapshot = store_snapshot("preview");
    let mut writer = store.create("s", &snapshot).unwrap();
    store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    let file = writer.path().to_owned();
    let summary = writer.discovery.clone();
    drop(writer);
    // A matching disposable index isolates listing from the body; it must never
    // make the full restore path trust that body's validity.
    append_bytes(&file, b"invalid body\n");
    index::write(
        file.parent().unwrap(),
        &fs::metadata(&file).unwrap(),
        &summary,
    )
    .unwrap();
    assert_eq!(store.list().unwrap()[0].preview, "preview");
    assert!(store.load("s").is_err());
}

#[test]
fn summary_write_failure_does_not_undo_a_durable_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let snapshot = store_snapshot("preview");
    let mut writer = store.create("s", &snapshot).unwrap();
    fs::create_dir(writer.path().parent().unwrap().join("summary.json")).unwrap();
    store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    assert_eq!(store.load("s").unwrap().1, snapshot);
    assert_eq!(store.list().unwrap()[0].preview, "preview");
}

#[test]
fn invalid_appends_do_not_materialize_a_new_log() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let snapshot = store_snapshot("preview");
    let mut writer = store.create("s", &snapshot).unwrap();
    for line in ["{broken}", "{}", "{\"type\":\"future\"}", "bad\nline"] {
        assert!(writer.append_line(line).is_err());
        assert!(!writer.path().exists());
    }
    store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    assert_eq!(store.load("s").unwrap().1, snapshot);
}

#[test]
fn failed_late_log_validation_cannot_be_bypassed_by_retrying_append() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let snapshot = store_snapshot("preview");
    let mut writer = store.create("s", &snapshot).unwrap();
    fs::write(writer.path(), b"bad header\n").unwrap();
    assert!(store.append_checkpoint(&mut writer, 0, &snapshot).is_err());
    assert!(store.append_checkpoint(&mut writer, 0, &snapshot).is_err());
    assert_eq!(fs::read(writer.path()).unwrap(), b"bad header\n");
}

#[test]
fn failed_append_rolls_back_to_a_clean_length() {
    let workspace = temp_workspace("rollback");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("rb", &store_snapshot("x")).unwrap();
    store
        .append_checkpoint(&mut writer, 0, &store_snapshot("user"))
        .unwrap();
    // A newline-bearing line must be refused, never half-written.
    assert!(writer.append_line("bad\nline").is_err());
    let (_, loaded) = store.load("rb").unwrap();
    assert_eq!(loaded.run_sequence, 0);
    assert_eq!(loaded.items.len(), 1);
}

#[test]
fn ids_escape_into_safe_path_segments() {
    let workspace = temp_workspace("escape");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("a/b c..d", &store_snapshot("x")).unwrap();
    store
        .append_checkpoint(&mut writer, 0, &store_snapshot("esc"))
        .unwrap();
    drop(writer);
    let summaries = store.list().unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, "a/b c..d");
    assert_eq!(summaries[0].preview, "esc");
    // The literal id never appears as a raw directory name.
    assert!(!workspace.join(".abycore/sessions/a").exists());
}

#[test]
fn single_writer_is_enforced_by_flock() {
    let workspace = temp_workspace("flock");
    let store = SessionStore::new(&workspace).unwrap();
    let _w1 = store.create("locked", &store_snapshot("x")).unwrap();
    let err = store.create("locked", &store_snapshot("x"));
    assert!(err.is_err(), "second writer must fail on flock");
}

#[test]
fn future_format_refuses_to_load() {
    let workspace = temp_workspace("future");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("fut", &store_snapshot("x")).unwrap();
    store
        .append_checkpoint(&mut writer, 0, &store_snapshot("user"))
        .unwrap();
    drop(writer);
    // Rewrite the header version upward.
    let file = store
        .data_dir()
        .join("sessions")
        .join("fut")
        .join("session.jsonl");
    let mut text = fs::read_to_string(&file).unwrap();
    text = text.replacen("\"version\":1", "\"version\":99", 1);
    fs::write(&file, text).unwrap();
    let err = match store.load("fut") {
        Err(error) => error,
        Ok(_) => panic!("must refuse"),
    };
    assert!(
        format!("{err}").contains("upgrade"),
        "future format reports upgrade semantics: {err}"
    );
}

#[test]
fn shared_root_partitions_workspaces_by_slug() {
    let store_root = temp_workspace("shared-root");
    let ws_a = temp_workspace("shared-a");
    let ws_b = temp_workspace("shared-b");
    let store_a = SessionStore::at(&store_root, &ws_a).expect("store a");
    let store_b = SessionStore::at(&store_root, &ws_b).expect("store b");

    let mut writer = store_a.create("s-a", &store_snapshot("from a")).unwrap();
    store_a
        .append_checkpoint(&mut writer, 0, &store_snapshot("from a"))
        .unwrap();
    drop(writer);

    let mut writer = store_b.create("s-b", &store_snapshot("from b")).unwrap();
    store_b
        .append_checkpoint(&mut writer, 0, &store_snapshot("from b"))
        .unwrap();
    drop(writer);

    // Each workspace sees only its own partition.
    let a: Vec<String> = store_a
        .list()
        .unwrap()
        .iter()
        .map(|s| s.id.clone())
        .collect();
    assert_eq!(a, ["s-a"]);
    let b: Vec<String> = store_b
        .list()
        .unwrap()
        .iter()
        .map(|s| s.id.clone())
        .collect();
    assert_eq!(b, ["s-b"]);

    // Both partitions live under the shared root, in per-workspace slugs.
    let a_dir = store_a.log_path("s-a").unwrap();
    let b_dir = store_b.log_path("s-b").unwrap();
    assert_ne!(
        a_dir.parent().unwrap().parent().unwrap(),
        b_dir.parent().unwrap().parent().unwrap()
    );
    assert_eq!(
        a_dir.parent().unwrap().parent().unwrap().parent().unwrap(),
        store_root
    );

    // The other workspace cannot load a foreign session id.
    assert!(store_b.load("s-a").is_err());
}

#[test]
fn legacy_partitions_remain_resumable_only_in_their_recorded_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let a = tmp.path().join("a-b/c");
    let b = tmp.path().join("a/b-c");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let store_a = SessionStore::at(&root, &a).unwrap();
    let store_b = SessionStore::at(&root, &b).unwrap();
    let snapshot = store_snapshot("legacy history");
    let mut writer = store_a.create_new("s", &snapshot).unwrap();
    store_a
        .append_checkpoint(&mut writer, 0, &snapshot)
        .unwrap();
    store_a.set_title(&mut writer, "custom title").unwrap();
    drop(writer);
    let original = store_a.dir_of("s").unwrap();
    let legacy = store_a.legacy_partition().unwrap().join("s");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::rename(&original, &legacy).unwrap();

    assert_eq!(store_a.log_path("s").unwrap(), legacy.join("session.jsonl"));
    assert_eq!(store_a.list().unwrap().len(), 1);
    assert_eq!(store_a.title_of("s").as_deref(), Some("custom title"));
    assert_eq!(store_a.load("s").unwrap().1, snapshot);
    let (writer, loaded) = store_a.open_for_resume("s").unwrap();
    assert_eq!(loaded, snapshot);
    // Legacy and new clients contend on the very same lock inode.
    let header = SessionHeader::from_snapshot("s", &a.to_string_lossy(), &snapshot);
    assert!(SessionWriter::open(legacy.clone(), header).is_err());
    assert!(store_b.list().unwrap().is_empty());
    assert!(store_b.load("s").is_err());
    assert!(store_b.title_of("s").is_none());
    let mut other = store_b.create_new("s", &snapshot).unwrap();
    store_b.append_checkpoint(&mut other, 0, &snapshot).unwrap();
    assert_ne!(writer.path(), other.path());
    assert_eq!(store_b.list().unwrap().len(), 1);
}

#[test]
fn a_foreign_header_is_rejected_before_any_log_repair() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    let sa = SessionStore::new(&a).unwrap();
    let sb = SessionStore::new(&b).unwrap();
    let snapshot = store_snapshot("foreign");
    let mut writer = sa.create_new("s", &snapshot).unwrap();
    sa.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    drop(writer);
    let dest = sb.log_path("s").unwrap();
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::copy(sa.log_path("s").unwrap(), &dest).unwrap();
    append_bytes(&dest, b"torn tail");
    let before = fs::read(&dest).unwrap();
    assert!(sb.load("s").is_err());
    assert!(sb.list().unwrap().is_empty());
    assert!(sb.title_of("s").is_none());
    assert!(sb.create("s", &snapshot).is_err());
    assert!(sb.open_for_resume("s").is_err());
    assert_eq!(fs::read(dest).unwrap(), before);
}

#[test]
fn checkpoints_rewrite_to_header_title_and_latest_snapshot() {
    let workspace = temp_workspace("compact-rewrite");
    let store = SessionStore::new(&workspace).expect("store");
    let mut writer = store.create("s", &store_snapshot("seed")).expect("writer");
    store.set_title(&mut writer, "first title").unwrap();
    for seq in 0..10u64 {
        let mut snapshot = store_snapshot(&format!("turn {seq}"));
        snapshot.run_sequence = seq;
        // Grow the transcript so rewrites must be replacing, not accumulating.
        snapshot.items = vec![Item::user(format!("turn {seq}"))];
        store
            .append_checkpoint(&mut writer, seq, &snapshot)
            .unwrap();
    }
    store.set_title(&mut writer, "final title").unwrap();
    store
        .append_checkpoint(&mut writer, 10, &store_snapshot("turn 10"))
        .unwrap();
    drop(writer);

    let file = store.log_path("s").unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "header + title + one snapshot");
    assert!(lines[0].contains("\"type\":\"session\""), "{}", lines[0]);
    assert!(
        lines[1].contains("\"title\":\"final title\"") && lines[1].contains("\"time\":"),
        "title line carries a timestamp: {}",
        lines[1]
    );
    assert!(lines[2].contains("\"type\":\"snapshot\""));
    assert!(lines[2].contains("\"time\":"));
    assert!(lines[2].contains("turn 10"));

    let (header, loaded) = store.load("s").unwrap();
    assert_eq!(header.id, "s");
    assert_eq!(loaded.items[0], Item::user("turn 10"));
    let summaries = store.list().unwrap();
    assert_eq!(summaries[0].title.as_deref(), Some("final title"));
    // The preview is the first user prompt of the session, not the latest.
    assert_eq!(summaries[0].preview, "turn 0");
}

#[test]
fn rewriting_a_legacy_multi_snapshot_log_compacts_it() {
    let workspace = temp_workspace("legacy-compact");
    let store = SessionStore::new(&workspace).unwrap();
    // Hand-assemble a legacy append-mode log: header plus several snapshots.
    let dir = store.log_path("s").unwrap();
    fs::create_dir_all(dir.parent().unwrap()).unwrap();
    let header_line = format!(
        "{{\"version\":1,\"id\":\"s\",\"created_at\":\"0\",\"cwd\":\"{}\",\"protocol\":\"deepseek-messages\",\"model\":\"deepseek-flash\",\"system_prompt\":\"system\",\"type\":\"session\"}}\n",
        workspace.to_string_lossy()
    );
    let snap = |text: &str, seq: u64| {
        let mut snapshot = store_snapshot(text);
        snapshot.run_sequence = seq;
        format!(
            "{}\n",
            serde_json::json!({"type":"snapshot", "seq":seq, "snapshot":snapshot})
        )
    };
    fs::write(
        &dir,
        format!("{}{}{}", header_line, snap("first", 0), snap("second", 1)),
    )
    .unwrap();
    assert_eq!(fs::read(&dir).unwrap().lines().count(), 3);

    // Reopen and checkpoint again: the rewrite adopts the newest state and
    // collapses the historical snapshots.
    let (header, loaded) = store.load("s").unwrap();
    assert_eq!(loaded.items[0], Item::user("second"));
    let mut writer = store.create("s", &loaded).unwrap();
    assert_eq!(header.id, "s", "the original header survives reopen");
    let mut next = store_snapshot("third");
    next.run_sequence = 1;
    store.append_checkpoint(&mut writer, 1, &next).unwrap();
    drop(writer);
    let text = fs::read_to_string(&dir).unwrap();
    assert_eq!(text.lines().count(), 2, "header + newest snapshot");
    let (_, loaded) = store.load("s").unwrap();
    assert_eq!(loaded.items[0], Item::user("third"));
    assert_eq!(loaded.run_sequence, 1);
}

#[test]
fn failed_commit_keeps_the_previous_log_intact() {
    // A commit whose temp file cannot be synced leaves the previous content
    // untouched: simulate by making the temp path unwritable via a read-only
    // check is unreliable across filesystems, so assert the rename atomicity
    // contract directly: content before == content after a refused newline line.
    let workspace = temp_workspace("commit-fail");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("s", &store_snapshot("first")).unwrap();
    store
        .append_checkpoint(&mut writer, 0, &store_snapshot("committed"))
        .unwrap();
    let before = fs::read(writer.path()).unwrap();
    assert!(writer.append_line("bad\nline").is_err());
    assert_eq!(fs::read(writer.path()).unwrap(), before);
    let (_, loaded) = store.load("s").unwrap();
    assert_eq!(loaded.items[0], Item::user("committed"));
    // The writer stays usable after a refused (pre-I/O) rejection.
    store
        .append_checkpoint(&mut writer, 1, &store_snapshot("next"))
        .unwrap();
    assert_eq!(store.load("s").unwrap().1.items[0], Item::user("next"));
}

#[test]
fn deltas_append_between_anchors_and_fold_to_the_latest_state() {
    let workspace = temp_workspace("delta-append");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("s", &store_snapshot("x")).unwrap();
    // A large first anchor: later one-item deltas are far below the
    // anchor/2 demotion threshold, so they must take the delta path.
    let mut seed = store_snapshot("seed");
    seed.items = (0..200).map(|i| Item::user(format!("item {i}"))).collect();
    store.append_checkpoint(&mut writer, 0, &seed).unwrap();
    for seq in 1..10u64 {
        let mut snapshot = seed.clone();
        snapshot.run_sequence = seq;
        snapshot.items = (0..200 + seq)
            .map(|i| Item::user(format!("item {i}")))
            .collect();
        store
            .append_checkpoint(&mut writer, seq, &snapshot)
            .unwrap();
    }
    drop(writer);

    let text = fs::read_to_string(store.log_path("s").unwrap()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 11, "header + anchor + 9 deltas");
    assert!(
        lines
            .iter()
            .skip(1)
            .filter(|l| l.contains("\"type\":\"snapshot\""))
            .count()
            == 1
    );
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("\"type\":\"delta\""))
            .count(),
        9
    );

    let (_, loaded) = store.load("s").unwrap();
    assert_eq!(loaded.run_sequence, 9);
    assert_eq!(loaded.items.len(), 209);
    assert_eq!(loaded.items[208], Item::user("item 208"));
}

#[test]
fn in_place_record_updates_survive_the_delta_fold() {
    let workspace = temp_workspace("delta-update");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("s", &store_snapshot("x")).unwrap();
    // A large anchor keeps one-item deltas on the incremental path.
    let mut snapshot = store_snapshot("user");
    snapshot.items = (0..200).map(|i| Item::user(format!("item {i}"))).collect();
    // A committed record whose usage fields fill in later (reserve pushes an
    // empty record; record() rewrites it in place).
    snapshot.requests.push(RequestRecord {
        purpose: RequestPurpose::Conversation,
        attempt: 1,
        response_id: None,
        status: None,
        usage: None,
        request_bytes: None,
    });
    store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    let mut snapshot = snapshot.clone();
    snapshot.run_sequence = 1;
    snapshot.requests[0].usage = Some(Usage {
        input_tokens: Some(10),
        output_tokens: Some(2),
        cached_tokens: None,
        reasoning_tokens: None,
        uncached_input_tokens: Some(10),
        cache_creation_tokens: None,
        total_tokens: Some(12),
        consistent: true,
    });
    snapshot.requests[0].status = Some("Completed".into());
    store.append_checkpoint(&mut writer, 1, &snapshot).unwrap();
    drop(writer);

    let text = fs::read_to_string(store.log_path("s").unwrap()).unwrap();
    assert!(
        text.contains("\"type\":\"delta\""),
        "the second checkpoint is a delta"
    );
    let (_, loaded) = store.load("s").unwrap();
    assert_eq!(
        loaded.requests[0]
            .usage
            .as_ref()
            .and_then(|u| u.input_tokens),
        Some(10),
        "the in-place update must reach the folded state"
    );
}

#[test]
fn anchoring_recurs_after_the_delta_interval() {
    let workspace = temp_workspace("delta-interval");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("s", &store_snapshot("x")).unwrap();
    let mut seed = store_snapshot("seed");
    seed.items = (0..200).map(|i| Item::user(format!("item {i}"))).collect();
    store.append_checkpoint(&mut writer, 0, &seed).unwrap();
    for seq in 1..70u64 {
        let mut snapshot = seed.clone();
        snapshot.run_sequence = seq;
        snapshot.items.push(Item::user(format!("tail {seq}")));
        store
            .append_checkpoint(&mut writer, seq, &snapshot)
            .unwrap();
    }
    drop(writer);
    let text = fs::read_to_string(store.log_path("s").unwrap()).unwrap();
    // Exactly one re-anchor happens at the 64-delta interval (seq 65), which
    // discards the earlier anchor and its deltas, then four deltas follow.
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("\"type\":\"snapshot\""))
            .count(),
        1,
        "each re-anchor replaces the previous one"
    );
    assert_eq!(lines.len(), 6, "header + re-anchor + 4 remaining deltas");

    let (_, loaded) = store.load("s").unwrap();
    assert_eq!(loaded.run_sequence, 69);
    assert_eq!(loaded.items.last(), Some(&Item::user("tail 69")));
}

#[test]
fn oversized_delta_demotes_to_an_anchor() {
    let workspace = temp_workspace("delta-demotion");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("s", &store_snapshot("x")).unwrap();
    store
        .append_checkpoint(&mut writer, 0, &store_snapshot("tiny"))
        .unwrap();
    let mut snapshot = store_snapshot("tiny");
    snapshot.run_sequence = 1;
    snapshot.items = vec![Item::user("tiny"), Item::user("huge ".repeat(200_000))];
    store.append_checkpoint(&mut writer, 1, &snapshot).unwrap();
    drop(writer);
    let text = fs::read_to_string(store.log_path("s").unwrap()).unwrap();
    assert!(
        !text.contains("\"type\":\"delta\""),
        "a giant item demotes to an anchor"
    );
    assert_eq!(text.lines().count(), 2);
    assert_eq!(store.load("s").unwrap().1.items.len(), 2);
}

#[test]
fn external_delta_lines_are_rejected() {
    let workspace = temp_workspace("delta-external");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("s", &store_snapshot("x")).unwrap();
    let delta = serde_json::json!({
        "type":"delta", "seq":1,
        "items":{"from":0, "added":[]},
        "requests":{"from":0, "added":[]},
        "pending":[], "needs_response":false, "run_sequence":1,
        "todos":null, "compactions":[], "prune":null, "goal":null
    })
    .to_string();
    assert!(writer.append_line(&delta).is_err());
}

#[test]
fn delta_before_any_anchor_is_rejected_on_load() {
    let workspace = temp_workspace("delta-first");
    let store = SessionStore::new(&workspace).unwrap();
    let dir = store.log_path("s").unwrap();
    fs::create_dir_all(dir.parent().unwrap()).unwrap();
    let header_line = format!(
        "{{\"version\":1,\"id\":\"s\",\"created_at\":\"0\",\"cwd\":\"{}\",\"protocol\":\"deepseek-messages\",\"model\":\"deepseek-flash\",\"system_prompt\":\"system\",\"type\":\"session\"}}\n",
        workspace.to_string_lossy()
    );
    let delta = serde_json::json!({
        "type":"delta", "seq":1,
        "items":{"from":0, "added":[]},
        "requests":{"from":0, "added":[]},
        "pending":[], "needs_response":false, "run_sequence":1,
        "todos":null, "compactions":[], "prune":null, "goal":null
    })
    .to_string();
    fs::write(&dir, format!("{header_line}{delta}\n")).unwrap();
    assert!(store.load("s").is_err());
}

#[test]
fn torn_delta_tail_repairs_and_resumes_delta_writes() {
    let workspace = temp_workspace("delta-torn");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("s", &store_snapshot("x")).unwrap();
    let mut seed = store_snapshot("seed");
    seed.items = (0..200).map(|i| Item::user(format!("item {i}"))).collect();
    store.append_checkpoint(&mut writer, 0, &seed).unwrap();
    let mut snapshot = seed.clone();
    snapshot.run_sequence = 1;
    snapshot.items.push(Item::user("delta one"));
    store.append_checkpoint(&mut writer, 1, &snapshot).unwrap();
    let file = writer.path().to_owned();
    drop(writer);
    // Simulate a torn delta append.
    append_bytes(
        &file,
        b"{\"type\":\"delta\",\"seq\":2,\"items\":{\"from\":201,\"added\":[{\"to",
    );
    let (_, loaded) = store.load("s").unwrap();
    assert_eq!(loaded.run_sequence, 1, "the torn delta is ignored");
    let mut writer = store.create("s", &loaded).unwrap();
    let mut next = loaded.clone();
    next.run_sequence = 2;
    next.items.push(Item::user("delta two"));
    store.append_checkpoint(&mut writer, 2, &next).unwrap();
    drop(writer);
    let text = fs::read_to_string(&file).unwrap();
    assert_eq!(
        text.lines().count(),
        4,
        "repair keeps the anchor and the first delta, then appends the next"
    );
    let (_, loaded) = store.load("s").unwrap();
    assert_eq!(loaded.run_sequence, 2);
    assert_eq!(loaded.items.last(), Some(&Item::user("delta two")));
}

#[test]
fn records_without_timestamps_still_parse() {
    // A log written before the time field existed must stay loadable.
    let workspace = temp_workspace("no-time");
    let store = SessionStore::new(&workspace).unwrap();
    let mut writer = store.create("s", &store_snapshot("x")).unwrap();
    store
        .append_checkpoint(&mut writer, 0, &store_snapshot("first"))
        .unwrap();
    drop(writer);
    let file = store.log_path("s").unwrap();
    let text = fs::read_to_string(&file).unwrap();
    // Strip every "time" key from the committed records, then reload.
    let stripped: Vec<String> = text
        .lines()
        .map(|line| {
            let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(object) = value.as_object_mut() {
                object.remove("time");
            }
            value.to_string()
        })
        .collect();
    fs::write(&file, format!("{}\n", stripped.join("\n"))).unwrap();
    let (_, loaded) = store.load("s").unwrap();
    assert_eq!(loaded.items[0], Item::user("first"));
}

// --- helpers --------------------------------------------------------------

fn store_snapshot(user_text: &str) -> SessionSnapshot {
    let mut snapshot = SessionSnapshot::new(
        "system",
        ModelOptions {
            model: "deepseek-flash".into(),
            ..ModelOptions::default()
        },
    );
    snapshot.items = vec![Item::user(user_text)];
    snapshot.needs_response = true;
    snapshot
}
