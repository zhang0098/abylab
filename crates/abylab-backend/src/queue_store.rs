//! Durable host queue: prompts typed while a turn was running.
//!
//! The queue lives in the driver, so a client reload or a second client never
//! loses it — but the driver is still a process, and a crash would take the
//! unsent prompts with it (the session snapshot survives, they would not). So
//! every queue mutation writes the session's FIFO under
//! `$ABYLAB_HOME/queued/<session>.json`, and the next start of that session
//! reads it back as held items.
//!
//! One file per session: two drivers on different sessions never touch the same
//! file, and a drained queue removes its file instead of leaving an empty entry
//! behind. Only the wire form is stored (what the driver will send), which is
//! text today.
//!
//! The file is a convenience, never a requirement: an absent, unreadable or
//! malformed store reads as an empty queue, exactly like `settings.json`.

use std::path::PathBuf;

/// Where one session's queue lives. The session id is user-supplied
/// (`--session-id`), so anything that could name a directory is folded to `_`.
pub fn path(home: &str, session: &str) -> PathBuf {
    let safe: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::path::Path::new(home)
        .join("queued")
        .join(format!("{safe}.json"))
}

/// The items waiting behind `session`'s turn, oldest first.
pub fn load(cfg: &crate::contract::DriverConfig, session: &str) -> Vec<String> {
    let Some(home) = cfg.home.as_deref() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path(home, session)) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    // A file in another shape (or from a future version) is not guessed at.
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return Vec::new();
    }
    value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Replace `session`'s queue with `texts`. An empty list removes the file, so a
/// drained queue leaves nothing behind for the next start to resurrect.
pub fn save(home: &str, session: &str, texts: &[String]) {
    let path = path(home, session);
    if texts.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    // Versioned so a future shape can migrate instead of reinterpreting.
    let store = serde_json::json!({ "version": 1, "items": texts });
    let Ok(text) = serde_json::to_string_pretty(&store) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("abylab-host-queue-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch home");
        dir.to_string_lossy().into_owned()
    }

    fn cfg(home: &str) -> crate::contract::DriverConfig {
        crate::contract::DriverConfig {
            session_id: "s".into(),
            resume: None,
            sessions_root: None,
            home: Some(home.into()),
            workspace: "/tmp".into(),
            model: "deepseek-flash".into(),
            reasoning: "off".into(),
            permission: None,
            max_tokens: None,
            api_key: Some("k".into()),
            base_url: None,
            limits: crate::contract::TurnLimits::default(),
            compaction: None,
        }
    }

    fn read(home: &str, session: &str) -> Vec<String> {
        load(&cfg(home), session)
    }

    #[test]
    fn a_saved_queue_reads_back_in_order() {
        let home = home("roundtrip");
        save(&home, "aby-1", &["first".into(), "second".into()]);
        assert_eq!(read(&home, "aby-1"), vec!["first", "second"]);
        assert!(read(&home, "aby-2").is_empty(), "another session is empty");
    }

    #[test]
    fn draining_a_queue_removes_its_file() {
        let home = home("drain");
        save(&home, "aby-1", &["only".into()]);
        assert!(path(&home, "aby-1").exists());
        save(&home, "aby-1", &[]);
        assert!(read(&home, "aby-1").is_empty());
        assert!(
            !path(&home, "aby-1").exists(),
            "the file went with the queue"
        );
    }

    #[test]
    fn a_session_id_cannot_escape_the_store_directory() {
        let home = home("escape");
        save(&home, "../outside", &["nope".into()]);
        let dir = std::path::Path::new(&home).join("queued");
        let files: Vec<String> = std::fs::read_dir(&dir)
            .expect("store dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, vec!["___outside.json".to_string()]);
        assert_eq!(read(&home, "../outside"), vec!["nope"]);
    }

    #[test]
    fn a_malformed_or_unknown_store_reads_as_empty() {
        let home = home("malformed");
        let file = path(&home, "aby-1");
        std::fs::create_dir_all(file.parent().expect("store dir")).expect("store dir");
        std::fs::write(&file, "{ not json").expect("write garbage");
        assert!(read(&home, "aby-1").is_empty());
        // A file from another shape (or another version) is not guessed at.
        std::fs::write(&file, "[[{\"kind\":\"text\",\"text\":\"old shape\"}]]").expect("write old");
        assert!(read(&home, "aby-1").is_empty());
        save(&home, "aby-1", &["recovered".into()]);
        assert_eq!(read(&home, "aby-1"), vec!["recovered"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_config_without_a_home_keeps_no_queue() {
        let mut config = cfg("/tmp/never-used");
        config.home = None;
        assert!(load(&config, "aby-1").is_empty());
    }
}
