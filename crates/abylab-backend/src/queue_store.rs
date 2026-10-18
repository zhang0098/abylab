//! Durable host queue: prompts typed while a turn was running.
//!
//! The queue lives in the driver, so a client reload or a second client never
//! loses it — but the driver is still a process, and a crash would take the
//! unsent prompts with it (the session snapshot survives, they would not). So
//! every queue mutation writes the session's FIFO under a workspace-specific
//! path in `$ABYLAB_HOME/queued`, and the next start reads it as held items.
//!
//! A drained queue removes its file. Records preserve text and image blocks.
//! Missing stores are empty; damaged stores are reported to the caller.

use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueuedRecord {
    /// Stable across process restarts; also stored as the user message id.
    #[serde(default)]
    pub input_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<abycore::ContentPart>>,
}

impl QueuedRecord {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            input_id: abycore::UserInput::text("").id,
            text: text.into(),
            parts: None,
        }
    }
}

impl From<&str> for QueuedRecord {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

impl From<String> for QueuedRecord {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

/// Queues are isolated by workspace and losslessly escaped session id.
pub fn path(home: &str, workspace: &str, session: &str) -> PathBuf {
    let workspace_hash = Sha256::digest(workspace.as_bytes());
    let workspace_key: String = workspace_hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let safe: String = session
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
                (byte as char).to_string()
            } else {
                format!("~{byte:02X}")
            }
        })
        .collect();
    std::path::Path::new(home)
        .join("queued")
        .join(workspace_key)
        .join(format!("{safe}.json"))
}

fn legacy_path(home: &str, session: &str) -> PathBuf {
    let safe: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
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
pub fn load_checked(
    cfg: &crate::contract::DriverConfig,
    session: &str,
) -> Result<Vec<QueuedRecord>, String> {
    let Some(home) = cfg.home.as_deref() else {
        return Ok(Vec::new());
    };
    let file = path(home, &cfg.workspace, session);
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let legacy = legacy_path(home, session);
            if legacy.exists() {
                return Err(format!(
                    "legacy queue found at {}; its workspace cannot be determined, so move it to {} after checking its contents",
                    legacy.display(),
                    file.display()
                ));
            }
            return Ok(Vec::new());
        }
        Err(error) => return Err(format!("queue load failed ({}): {error}", file.display())),
    };
    let mut records = parse_store(&text, &file)?;
    let version = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value["version"].as_u64());
    if version != Some(3) {
        for record in &mut records {
            if record.input_id.is_empty() {
                record.input_id = abycore::UserInput::text("").id;
            }
        }
        // Migration must commit identities before any record can be delivered.
        save(home, &cfg.workspace, session, &records)
            .map_err(|error| format!("queue migration failed: {error}"))?;
    }
    Ok(records)
}

fn parse_store(text: &str, file: &Path) -> Result<Vec<QueuedRecord>, String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| format!("queue file is invalid ({}): {error}", file.display()))?;
    let version = value.get("version").and_then(serde_json::Value::as_u64);
    if !matches!(version, Some(1..=3)) {
        return Err(format!("unsupported queue version ({})", file.display()));
    }
    let rows = value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("queue file has no items array ({})", file.display()))?;
    let records: Vec<QueuedRecord> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| match version {
            Some(1) => row
                .as_str()
                .map(QueuedRecord::text)
                .ok_or_else(|| format!("queue item {index} is invalid ({})", file.display())),
            Some(2 | 3) => serde_json::from_value::<QueuedRecord>(row.clone()).map_err(|error| {
                format!(
                    "queue item {index} is invalid ({}): {error}",
                    file.display()
                )
            }),
            _ => unreachable!("version checked above"),
        })
        .collect::<Result<_, _>>()?;
    if version == Some(3) {
        let mut ids = std::collections::HashSet::new();
        if records.iter().any(|record| {
            record.input_id.is_empty()
                || record.input_id.len() > 128
                || !ids.insert(&record.input_id)
        }) {
            return Err(format!(
                "queue has invalid or duplicate input ids ({})",
                file.display()
            ));
        }
    }
    Ok(records)
}

#[cfg(test)]
pub fn load(cfg: &crate::contract::DriverConfig, session: &str) -> Vec<QueuedRecord> {
    load_checked(cfg, session).unwrap_or_default()
}

/// Remove `session`'s persisted queue. A missing file is a no-op; unlike
/// [`save`] with an empty list, a malformed store is removed too — the
/// session it belongs to is being deleted, and refusing would leave the file
/// behind forever.
pub fn remove(home: &str, workspace: &str, session: &str) -> std::io::Result<()> {
    let file = path(home, workspace, session);
    match std::fs::remove_file(&file) {
        Ok(()) => sync_dir(file.parent().expect("queue parent")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Replace `session`'s queue with `items`. An empty list removes the file, so a
/// drained queue leaves nothing behind for the next start to resurrect.
pub fn save(
    home: &str,
    workspace: &str,
    session: &str,
    items: &[QueuedRecord],
) -> std::io::Result<()> {
    let path = path(home, workspace, session);
    // A malformed existing store may still be recoverable by hand. Refuse to
    // replace or delete it while accepting later queue mutations.
    match std::fs::read_to_string(&path) {
        Ok(existing) => {
            parse_store(&existing, &path)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if items.is_empty() {
        return remove(home, workspace, session);
    }
    // Versioned so a future shape can migrate instead of reinterpreting.
    let store = serde_json::json!({ "version": 3, "items": items });
    let text = serde_json::to_vec_pretty(&store)?;
    parse_store(std::str::from_utf8(&text).expect("JSON is UTF-8"), &path)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let dir = path.parent().expect("queue path has a parent");
    create_dir_synced(dir)?;
    let mut stage = tempfile::NamedTempFile::new_in(dir)?;
    stage.write_all(&text)?;
    stage.as_file().sync_all()?;
    stage.persist(&path).map_err(|error| error.error)?;
    sync_dir(dir)
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn create_dir_synced(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        create_dir_synced(parent)?;
        match std::fs::create_dir(path) {
            Ok(()) => sync_dir(parent)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
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
            messages: Default::default(),
            web: Default::default(),
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
            .into_iter()
            .map(|record| record.text)
            .collect()
    }

    #[test]
    fn a_saved_queue_reads_back_in_order() {
        let home = home("roundtrip");
        save(&home, "/tmp", "aby-1", &["first".into(), "second".into()]).unwrap();
        assert_eq!(read(&home, "aby-1"), vec!["first", "second"]);
        assert!(read(&home, "aby-2").is_empty(), "another session is empty");
    }

    #[test]
    fn draining_a_queue_removes_its_file() {
        let home = home("drain");
        save(&home, "/tmp", "aby-1", &["only".into()]).unwrap();
        assert!(path(&home, "/tmp", "aby-1").exists());
        save(&home, "/tmp", "aby-1", &[]).unwrap();
        assert!(read(&home, "aby-1").is_empty());
        assert!(
            !path(&home, "/tmp", "aby-1").exists(),
            "the file went with the queue"
        );
    }

    /// `/delete` removes the queue file even when it is malformed: the
    /// session is gone, and `save`'s recovery guard must not keep the file.
    #[test]
    fn remove_deletes_a_queue_file_whatever_its_shape() {
        let home = home("remove");
        save(&home, "/tmp", "aby-1", &["kept".into()]).unwrap();
        remove(&home, "/tmp", "aby-1").unwrap();
        assert!(!path(&home, "/tmp", "aby-1").exists());
        assert!(read(&home, "aby-1").is_empty());
        remove(&home, "/tmp", "aby-1").unwrap(); // missing is a no-op

        let file = path(&home, "/tmp", "aby-2");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{ not json").unwrap();
        remove(&home, "/tmp", "aby-2").unwrap();
        assert!(!file.exists(), "a broken store still goes with its session");
    }

    #[test]
    fn a_session_id_cannot_escape_the_store_directory() {
        let home = home("escape");
        save(&home, "/tmp", "../outside", &["nope".into()]).unwrap();
        let file = path(&home, "/tmp", "../outside");
        assert!(file.starts_with(std::path::Path::new(&home).join("queued")));
        assert_eq!(file.file_name().unwrap(), "~2E~2E~2Foutside.json");
        assert_eq!(read(&home, "../outside"), vec!["nope"]);
    }

    #[test]
    fn queues_do_not_collide_across_workspaces_or_session_spellings() {
        let home = home("isolation");
        save(&home, "/one", "a/b", &["one".into()]).unwrap();
        save(&home, "/one", "a_b", &["two".into()]).unwrap();
        save(&home, "/two", "a/b", &["three".into()]).unwrap();
        assert_ne!(path(&home, "/one", "a/b"), path(&home, "/one", "a_b"));
        assert_ne!(path(&home, "/one", "a/b"), path(&home, "/two", "a/b"));
        let mut config = cfg(&home);
        config.workspace = "/one".into();
        assert_eq!(load(&config, "a/b")[0].text, "one");
        assert_eq!(load(&config, "a_b")[0].text, "two");
        config.workspace = "/two".into();
        assert_eq!(load(&config, "a/b")[0].text, "three");
    }

    #[test]
    fn a_malformed_or_unknown_store_is_reported() {
        let home = home("malformed");
        let file = path(&home, "/tmp", "aby-1");
        std::fs::create_dir_all(file.parent().expect("store dir")).expect("store dir");
        std::fs::write(&file, "{ not json").expect("write garbage");
        assert!(load_checked(&cfg(&home), "aby-1").is_err());
        // A file from another shape (or another version) is not guessed at.
        std::fs::write(&file, "[[{\"kind\":\"text\",\"text\":\"old shape\"}]]").expect("write old");
        assert!(load_checked(&cfg(&home), "aby-1").is_err());
        assert!(save(&home, "/tmp", "aby-1", &["recovered".into()]).is_err());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "[[{\"kind\":\"text\",\"text\":\"old shape\"}]]"
        );
        std::fs::remove_file(&file).unwrap();
        save(&home, "/tmp", "aby-1", &["recovered".into()]).unwrap();
        assert_eq!(read(&home, "aby-1"), vec!["recovered"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn legacy_text_rows_and_image_blocks_restore() {
        let home = home("versions");
        let file = path(&home, "/tmp", "aby-1");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, r#"{"version":1,"items":["first","second"]}"#).unwrap();
        assert_eq!(read(&home, "aby-1"), vec!["first", "second"]);
        let record = QueuedRecord {
            input_id: "image-input".into(),
            text: "look".into(),
            parts: Some(vec![
                abycore::ContentPart::InputText {
                    text: "look".into(),
                },
                abycore::ContentPart::InputImage {
                    media_type: "image/png".into(),
                    data: "AA==".into(),
                },
            ]),
        };
        save(&home, "/tmp", "aby-1", std::slice::from_ref(&record)).unwrap();
        assert_eq!(load_checked(&cfg(&home), "aby-1").unwrap(), vec![record]);
    }

    #[test]
    fn legacy_queue_is_reported_instead_of_silently_dropped() {
        let home = home("legacy-location");
        let legacy = legacy_path(&home, "aby-1");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, r#"{"version":1,"items":["unsent"]}"#).unwrap();
        let error = load_checked(&cfg(&home), "aby-1").unwrap_err();
        assert!(error.contains("legacy queue found"));
        assert!(error.contains(&legacy.display().to_string()));
    }

    #[test]
    fn a_config_without_a_home_keeps_no_queue() {
        let mut config = cfg("/tmp/never-used");
        config.home = None;
        assert!(load(&config, "aby-1").is_empty());
    }
    #[test]
    fn migration_commits_stable_ids_and_rejects_duplicate_receipts() {
        let home = home("receipt-migration");
        let cfg = cfg(&home);
        for (session, old) in [
            ("v1", serde_json::json!({"version":1,"items":["text"]})),
            (
                "v2",
                serde_json::json!({"version":2,"items":[{"text":"image","parts":[{"type":"input_image","media_type":"image/png","data":"AA=="}]}]}),
            ),
        ] {
            let file = path(&home, &cfg.workspace, session);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, old.to_string()).unwrap();
            let first = load_checked(&cfg, session).unwrap();
            assert!(!first[0].input_id.is_empty());
            assert_eq!(first, load_checked(&cfg, session).unwrap());
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&file).unwrap())
                    .unwrap()["version"],
                3
            );
            assert!(
                save(
                    &home,
                    &cfg.workspace,
                    session,
                    &[first[0].clone(), first[0].clone()]
                )
                .is_err()
            );
            assert_eq!(
                first,
                load_checked(&cfg, session).unwrap(),
                "invalid write did not replace the accepted queue"
            );
        }
    }
}
