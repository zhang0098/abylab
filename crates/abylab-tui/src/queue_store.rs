//! Durable client queue: prompts typed while a turn was running.
//!
//! deepseek-harness keeps its queue in the host process, so a browser reload
//! never loses it. abylab's UI *is* the host process, so the only way that
//! queue survives anything is to write it down: every mutation persists the
//! session's FIFO under `$ABYLAB_HOME/queued/<session>.json`, and the next
//! start (or the next `/resume` onto that session) paints the items back as
//! queued bubbles.
//!
//! One file per session: two abylab processes on different sessions never
//! read-modify-write the same file, and a drained queue removes its file
//! instead of leaving an empty entry behind.
//!
//! The file is a convenience, never a requirement: an absent, unreadable or
//! malformed store reads as an empty queue, exactly like `settings.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One persisted block: prompt text, or a staged image with its payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Image {
        name: String,
        /// Display path for the no-thumbnail fallback; a clipboard paste has a
        /// synthetic one, which is why the payload is stored too.
        path: String,
        media_type: String,
        /// The encoded bytes, so a restored item is the very prompt the user
        /// queued (a clipboard image has no file to re-read).
        data: Vec<u8>,
    },
}

/// Where one session's queue lives. The session id is user-supplied
/// (`--session-id`), so anything that could name a directory is folded to `_`.
fn path(home: &str, session: &str) -> PathBuf {
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

/// The items waiting behind `session`'s running turn, oldest first.
pub fn load(home: &str, session: &str) -> Vec<Vec<Block>> {
    std::fs::read_to_string(path(home, session))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Replace `session`'s queue with `items`. An empty list removes the file, so a
/// drained queue leaves nothing behind for the next start to resurrect.
pub fn save(home: &str, session: &str, items: &[Vec<Block>]) {
    let path = path(home, session);
    if items.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let Ok(text) = serde_json::to_string_pretty(items) else {
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
            std::env::temp_dir().join(format!("abylab-queue-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch home");
        dir.to_string_lossy().into_owned()
    }

    fn text(value: &str) -> Vec<Block> {
        vec![Block::Text { text: value.into() }]
    }

    #[test]
    fn a_saved_queue_reads_back_in_order() {
        let home = home("roundtrip");
        save(&home, "aby-1", &[text("first"), text("second")]);
        assert_eq!(load(&home, "aby-1"), vec![text("first"), text("second")]);
        assert!(load(&home, "aby-2").is_empty(), "another session is empty");
    }

    #[test]
    fn draining_a_queue_removes_its_file() {
        let home = home("drain");
        save(&home, "aby-1", &[text("only")]);
        assert!(path(&home, "aby-1").exists());
        save(&home, "aby-1", &[]);
        assert!(load(&home, "aby-1").is_empty());
        assert!(
            !path(&home, "aby-1").exists(),
            "the file went with the queue"
        );
    }

    #[test]
    fn two_sessions_do_not_share_a_file() {
        let home = home("sessions");
        save(&home, "aby-1", &[text("one")]);
        save(&home, "aby-2", &[text("two")]);
        assert_eq!(load(&home, "aby-1"), vec![text("one")]);
        assert_eq!(load(&home, "aby-2"), vec![text("two")]);
    }

    #[test]
    fn a_session_id_cannot_escape_the_store_directory() {
        let home = home("escape");
        save(&home, "../outside", &[text("nope")]);
        let dir = std::path::Path::new(&home).join("queued");
        let files: Vec<String> = std::fs::read_dir(&dir)
            .expect("store dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, vec!["___outside.json".to_string()]);
        assert_eq!(load(&home, "../outside"), vec![text("nope")]);
    }

    #[test]
    fn an_image_item_keeps_its_payload() {
        let home = home("image");
        let blocks = vec![
            Block::Text {
                text: "look at this".into(),
            },
            Block::Image {
                name: "shot.png".into(),
                path: "/tmp/shot.png".into(),
                media_type: "image/png".into(),
                data: vec![1, 2, 3, 255],
            },
        ];
        save(&home, "aby-1", std::slice::from_ref(&blocks));
        assert_eq!(load(&home, "aby-1"), vec![blocks]);
    }

    #[test]
    fn a_malformed_store_reads_as_empty_and_is_rewritten() {
        let home = home("malformed");
        let file = path(&home, "aby-1");
        std::fs::create_dir_all(file.parent().expect("store dir")).expect("store dir");
        std::fs::write(&file, "{ not json").expect("write garbage");
        assert!(load(&home, "aby-1").is_empty());
        save(&home, "aby-1", &[text("recovered")]);
        assert_eq!(load(&home, "aby-1"), vec![text("recovered")]);
        let _ = std::fs::remove_dir_all(&home);
    }
}
