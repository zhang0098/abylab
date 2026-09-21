use abycore::{Item, ModelOptions, SessionSnapshot, SessionStore};

fn seed() -> SessionSnapshot {
    let mut s = SessionSnapshot::new("original system", ModelOptions::default());
    s.items = (0..100)
        .map(|i| Item::user(format!("historical message {i}")))
        .collect();
    s.needs_response = true;
    s
}

#[test]
fn configuration_changes_survive_checkpoint_replay() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let mut s = seed();
    let mut writer = store.create("s", &s).unwrap();
    store.append_checkpoint(&mut writer, 0, &s).unwrap();
    s.model.max_tokens = 1234;
    store.append_checkpoint(&mut writer, 1, &s).unwrap();
    let restored = store.load("s").unwrap().1;
    assert_eq!(
        restored.model.max_tokens, s.model.max_tokens,
        "max_tokens must round-trip"
    );
    assert_eq!(restored.system_prompt, s.system_prompt);
    s.system_prompt = "only the prompt changed".into();
    store.append_checkpoint(&mut writer, 2, &s).unwrap();
    assert_eq!(store.load("s").unwrap().1, s);
}

#[test]
fn colliding_legacy_workspace_names_have_distinct_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a-b/c");
    let b = dir.path().join("a/b-c");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let sa = SessionStore::at(dir.path().join("store"), &a).unwrap();
    let sb = SessionStore::at(dir.path().join("store"), &b).unwrap();
    let s = seed();
    let mut writer = sa.create("s", &s).unwrap();
    sa.append_checkpoint(&mut writer, 0, &s).unwrap();
    assert!(
        sb.load("s").is_err(),
        "workspace B must not load workspace A's session"
    );
    assert!(sb.list().unwrap().is_empty());
    assert_ne!(sa.log_path("s").unwrap(), sb.log_path("s").unwrap());
    let mut other = sb.create_new("s", &s).unwrap();
    sb.append_checkpoint(&mut other, 0, &s).unwrap();
    assert_eq!(sa.list().unwrap().len(), 1);
    assert_eq!(sb.list().unwrap().len(), 1);
}

#[test]
fn invalid_deltas_are_rejected_even_when_a_later_record_repairs_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let mut s = seed();
    let mut writer = store.create("s", &s).unwrap();
    store.append_checkpoint(&mut writer, 0, &s).unwrap();
    s.run_sequence = 1;
    store.append_checkpoint(&mut writer, 1, &s).unwrap();
    drop(writer);
    let path = store.log_path("s").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let repaired = lines.last().unwrap().clone();
    assert_eq!(repaired["type"], "delta");
    lines.last_mut().unwrap()["needs_response"] = false.into();
    std::fs::write(
        &path,
        lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let loaded = store.load("s");
    assert!(
        loaded.is_err(),
        "load accepted a snapshot whose validate() fails: {:?}",
        loaded.as_ref().map(|(_, s)| s.validate())
    );
    lines.push(repaired);
    let invalid = lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>()
        + "unfinished tail";
    std::fs::write(&path, &invalid).unwrap();
    assert!(store.load("s").is_err());
    assert!(store.open_for_resume("s").is_err());
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        invalid,
        "refuse corruption without repairing its tail"
    );
}

#[test]
fn resume_holds_the_writer_and_new_sessions_never_replace_existing_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let mut s = seed();
    let mut writer = store.create_new("s", &s).unwrap();
    store.append_checkpoint(&mut writer, 0, &s).unwrap();
    assert!(store.open_for_resume("s").is_err());
    s.items.push(Item::user("latest committed message"));
    store.append_checkpoint(&mut writer, 1, &s).unwrap();
    drop(writer);
    let (writer, loaded) = store.open_for_resume("s").unwrap();
    assert_eq!(loaded, s);
    assert!(store.create("s", &s).is_err());
    drop(writer);
    assert!(store.create_new("s", &seed()).is_err());
    assert_eq!(store.load("s").unwrap().1, s);
}
