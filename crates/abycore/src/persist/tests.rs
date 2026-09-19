//! Offline persistence tests: temp workspaces, no network.

use super::*;
use crate::{Item, ModelOptions};
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
