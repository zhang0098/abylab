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
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<abycore::ContentPart>>,
}

impl QueuedRecord {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
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
    parse_store(&text, &file)
}

fn parse_store(text: &str, file: &Path) -> Result<Vec<QueuedRecord>, String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| format!("queue file is invalid ({}): {error}", file.display()))?;
    let version = value.get("version").and_then(serde_json::Value::as_u64);
    if !matches!(version, Some(1 | 2)) {
        return Err(format!("unsupported queue version ({})", file.display()));
    }
    let rows = value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("queue file has no items array ({})", file.display()))?;
    rows.iter()
        .enumerate()
        .map(|(index, row)| match version {
            Some(1) => row
                .as_str()
                .map(QueuedRecord::text)
                .ok_or_else(|| format!("queue item {index} is invalid ({})", file.display())),
            Some(2) => serde_json::from_value::<QueuedRecord>(row.clone()).map_err(|error| {
                format!(
                    "queue item {index} is invalid ({}): {error}",
                    file.display()
                )
            }),
            _ => unreachable!("version checked above"),
        })
        .collect()
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
    match std::fs::remove_file(path(home, workspace, session)) {
        Ok(()) => Ok(()),
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
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    // Versioned so a future shape can migrate instead of reinterpreting.
    let store = serde_json::json!({ "version": 2, "items": items });
    let text = serde_json::to_vec_pretty(&store)?;
    let dir = path.parent().expect("queue path has a parent");
    std::fs::create_dir_all(dir)?;
    let mut stage = tempfile::NamedTempFile::new_in(dir)?;
    stage.write_all(&text)?;
    stage.as_file().sync_all()?;
    stage.persist(path).map_err(|error| error.error)?;
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
        assert_eq!(load(&config, "a/b"), vec![QueuedRecord::text("one")]);
        assert_eq!(load(&config, "a_b"), vec![QueuedRecord::text("two")]);
        config.workspace = "/two".into();
        assert_eq!(load(&config, "a/b"), vec![QueuedRecord::text("three")]);
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
}
