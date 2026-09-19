//! Host credential store over `<aby home>/.credentials.yaml`.
//!
//! Mirrors DeepSeek Harness's `credentials-local`, proportionately:
//!
//! ```text
//! --api-key flag   (one-run override, wins while present)
//! > managed store  (this document; the only durable source, written by /login)
//! ```
//!
//! The environment is never consulted.
//!
//! The document holds nothing but credentials — a strict version-1 layout
//! (`version: 1` + a `refs:` mapping of env-style names to secret strings).
//! Every write re-reads the document and patches only its own entry, so
//! comments and the spelling of untouched entries survive; the file is
//! created and replaced at `0600`, and a document readable beyond its owner
//! is refused before its contents are parsed. Keys are write-only: callers
//! describe them with [`redact`], never the literal value.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Basename of the credentials document inside the aby home.
pub const CREDENTIALS_FILENAME: &str = ".credentials.yaml";

/// The document layout this build reads and writes.
pub const DOCUMENT_VERSION: u32 = 1;

/// The one reference abylab stores today; the layout stays generic.
pub const API_KEY_REF: &str = "DEEPSEEK_API_KEY";

/// Absolute path of the credentials document under the aby home.
pub fn credentials_path(home: &str) -> PathBuf {
    Path::new(home).join(CREDENTIALS_FILENAME)
}

/// Permission bits outside the owner; a credentials document must have none.
#[cfg(unix)]
const GROUP_OTHER_BITS: u32 = 0o077;

/// One parsed document: the reference entries, keyed as written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CredentialRefs {
    pub refs: Vec<(String, String)>,
}

impl CredentialRefs {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.refs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Refuse a credentials document other OS users can read, before its contents
/// are read at all. The store creates and replaces the file at `0600`, but a
/// hand-written one carries whatever umask produced it, and silently serving
/// secrets out of a world-readable file would make the mode the store
/// promises meaningless. POSIX only; other platforms keep whatever the
/// create and replace APIs express.
#[cfg(unix)]
fn assert_owner_only(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(()); // absent: nothing to inspect, the writer enforces 0600
    };
    let mode = meta.permissions().mode() & 0o777;
    let offending = mode & GROUP_OTHER_BITS;
    if offending == 0 {
        return Ok(());
    }
    Err(format!(
        "{} is readable beyond its owner (mode {mode:o}); run \"chmod 600 {}\" before starting again",
        path.display(),
        path.display()
    ))
}

#[cfg(not(unix))]
fn assert_owner_only(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Admit a reference name: env-style POSIX identifiers, which is exactly the
/// constraint a stored reference must satisfy to stay addressable.
fn valid_ref_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Strip one matched pair of quotes; the writer emits bare values, quotes are
/// only tolerated for hand-edited documents.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && bytes[0] == bytes[bytes.len() - 1]
        && (bytes[0] == b'"' || bytes[0] == b'\'')
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// An inline comment (`value # note`) is YAML's; `#` glued to the value is
/// not — API keys never carry " #", so this split never eats a secret.
fn strip_inline_comment(value: &str) -> &str {
    match value.split_once(" #") {
        Some((value, _)) => value,
        None => value,
    }
}

/// Parse one `NAME: value` entry line; `None` when the line is not an entry.
fn parse_entry(line: &str) -> Option<(String, String)> {
    let (name, value) = line.split_once(':')?;
    let name = name.trim();
    if !valid_ref_name(name) {
        return None;
    }
    let value = unquote(strip_inline_comment(value).trim());
    Some((name.to_string(), value.to_string()))
}

/// Top-level key of a line, when it is one of the layout's own keys.
fn top_layout_key(line: &str) -> Option<&str> {
    let (name, _) = line.split_once(':')?;
    match name.trim() {
        "version" | "refs" | "records" => Some(name.trim()),
        _ => None,
    }
}

fn push_ref(
    refs: &mut Vec<(String, String)>,
    name: String,
    value: String,
    where_: &str,
) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!(
            "the value for \"{name}\" at {where_} is empty; remove the key instead"
        ));
    }
    if refs.iter().any(|(key, _)| key == &name) {
        return Err(format!(
            "invalid document at {where_}: duplicate key \"{name}\""
        ));
    }
    refs.push((name, value));
    Ok(())
}

/// Parse the document text. Everything is rejected rather than skipped — an
/// unversioned flat layout is still admitted (the next write migrates it),
/// but an unknown top-level key, a wrong-typed or empty value, a duplicate
/// key, or a future version is refused, because this file holds nothing but
/// credentials and a silently ignored entry reads as "the key I stored has
/// no effect". An empty (or comment-only) document is the empty store.
pub fn parse_document(text: &str) -> Result<CredentialRefs, String> {
    let mut refs: Vec<(String, String)> = Vec::new();
    let mut section: Option<&'static str> = None;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let where_ = format!("line {index}");
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("invalid document at {where_}: expected \"name: value\""))?;
        let name = name.trim();
        if !line.starts_with(char::is_whitespace) {
            // Top-level mapping.
            match name {
                "version" => {
                    let version: u32 =
                        strip_inline_comment(value).trim().parse().map_err(|_| {
                            format!("invalid document at {where_}: version must be an integer")
                        })?;
                    if version != DOCUMENT_VERSION {
                        return Err(format!(
                            "the document declares version {version}; this build reads version {DOCUMENT_VERSION}"
                        ));
                    }
                    section = None;
                }
                "refs" => {
                    if !strip_inline_comment(value).trim().is_empty() {
                        return Err(format!(
                            "invalid document at {where_}: \"refs:\" must open a block mapping"
                        ));
                    }
                    section = Some("refs");
                }
                "records" => {
                    return Err(format!(
                        "invalid document at {where_}: this build does not read a \"records\" section"
                    ));
                }
                _ => {
                    // Pre-release flat layout: an addressable entry at the top.
                    let Some((ref_name, ref_value)) = parse_entry(line) else {
                        return Err(format!(
                            "invalid document at {where_}: unknown top-level key \"{name}\""
                        ));
                    };
                    push_ref(&mut refs, ref_name, ref_value, &where_)?;
                }
            }
            continue;
        }
        // Nested line: only refs entries are known.
        if section != Some("refs") {
            return Err(format!(
                "invalid document at {where_}: nested lines belong under \"refs:\""
            ));
        }
        let Some((ref_name, ref_value)) = parse_entry(line) else {
            return Err(format!(
                "invalid document at {where_}: expected a \"NAME: value\" reference"
            ));
        };
        push_ref(&mut refs, ref_name, ref_value, &where_)?;
    }
    Ok(CredentialRefs { refs })
}

/// Read the document, refusing it before parsing when it is readable beyond
/// its owner. Absent means no stored credentials; every other read failure
/// surfaces.
pub fn load_refs(home: &str) -> Result<CredentialRefs, String> {
    let path = credentials_path(home);
    assert_owner_only(&path)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_document(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(CredentialRefs::default()),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

/// Read one stored reference.
pub fn stored_key(home: &str, name: &str) -> Result<Option<String>, String> {
    Ok(load_refs(home)?.get(name).map(str::to_string))
}

/// Remove one stored reference. Read-modify-write against the document as it
/// stands now: only the matched entry's line disappears, so comments and
/// every sibling entry survive. Returns whether an entry was removed; a
/// missing document or a missing entry is `false`, not an error.
pub fn delete_key(home: &str, name: &str) -> Result<bool, String> {
    let path = credentials_path(home);
    assert_owner_only(&path)?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false); // absent document: nothing stored
    };
    let refs = parse_document(&text)?; // never rewrite a document this build cannot parse
    if refs.get(name).is_none() {
        return Ok(false);
    }
    let next = render_delete(&text, name);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("cannot create {dir:?}: {err}"))?;
    }
    let tmp = path.with_extension(format!(
        "yaml.tmp-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    write_secret_file(&tmp, &next)?;
    if let Err(err) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot replace {}: {err}", path.display()));
    }
    Ok(true)
}

/// Drop the named entry's line (flat or nested under `refs:`); comments and
/// sibling entries survive, and a now-empty `refs:` block stays valid. The
/// layout's own keys (`version`/`refs`/`records`) parse as entries too, so
/// they are excluded from deletion.
fn render_delete(text: &str, name: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if top_layout_key(line).is_none()
            && parse_entry(line).is_some_and(|(ref_name, _)| ref_name == name)
        {
            continue;
        }
        out.push(line.to_string());
    }
    let mut text = out.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Store (or replace) one reference. Read-modify-write against the document
/// as it stands now: only the matched entry's line changes, so comments,
/// blank lines and every sibling entry survive byte for byte; a hand-written
/// flat layout migrates to the version-1 layout in the same write.
///
/// The document is replaced atomically at `0600` (temp file in the same
/// directory, renamed over the target), so a reader never observes a partial
/// write and the mode the store promises is the mode the file carries. The
/// harness also holds a cross-process writer lock; abylab is a
/// single-process TUI whose only writers share this synchronous path, so the
/// read-modify-write window is not narrowed further without a dependency.
pub fn store_key(home: &str, name: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("empty keys are not stored; remove the key instead".into());
    }
    if !valid_ref_name(name) {
        return Err(format!("\"{name}\" is not an env-style reference name"));
    }
    let path = credentials_path(home);
    assert_owner_only(&path)?;

    let text = std::fs::read_to_string(&path).unwrap_or_default();
    parse_document(&text)?; // never overwrite a document this build cannot parse
    let next = if has_version_layout(&text) {
        patch_refs_section(&text, name, value)
    } else {
        patch_refs_section(&migrate_flat(&text), name, value)
    };

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("cannot create {dir:?}: {err}"))?;
    }
    let tmp = path.with_extension(format!(
        "yaml.tmp-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    write_secret_file(&tmp, &next)?;
    if let Err(err) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot replace {}: {err}", path.display()));
    }
    Ok(())
}

/// Create/replace-safe secret write: the temp file is `0600` from the start,
/// so the renamed document never exposes a wider mode window.
fn write_secret_file(path: &Path, text: &str) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("cannot create {path:?}: {err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("cannot protect {path:?}: {err}"))?;
    }
    file.write_all(text.as_bytes())
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|err| format!("cannot write {path:?}: {err}"))?;
    Ok(())
}

/// Whether the document already carries the version-1 envelope.
fn has_version_layout(text: &str) -> bool {
    text.lines().any(|line| {
        !line.starts_with(char::is_whitespace)
            && !line.trim().is_empty()
            && !line.trim().starts_with('#')
            && top_layout_key(line) == Some("version")
    })
}

/// Render the version-1 layout for a pre-release flat (or empty) document:
/// the original lines — comments, blank lines, and each value's spelling —
/// nest verbatim under `refs:` at two spaces' indent, then the new entry
/// follows. A flat entry for the same name is carried too; the patch step
/// below replaces it in place, so no duplicate can be written.
fn migrate_flat(text: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("version: {DOCUMENT_VERSION}\nrefs:\n"));
    for line in text.lines() {
        if line.is_empty() {
            out.push('\n');
        } else {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out
}

/// Replace the named entry's line in place inside the refs block, or append
/// it after the last entry (creating the `refs:` block when absent).
fn patch_refs_section(text: &str, name: &str, value: &str) -> String {
    let entry = format!("  {name}: {value}");
    let mut out: Vec<String> = Vec::new();
    let mut refs_header: Option<usize> = None;
    let mut last_ref_entry: Option<usize> = None;
    let mut replaced = false;

    for line in text.lines() {
        if refs_header.is_none() {
            let trimmed = line.trim();
            if !trimmed.is_empty()
                && !trimmed.starts_with('#')
                && !line.starts_with(char::is_whitespace)
                && top_layout_key(line) == Some("refs")
            {
                refs_header = Some(out.len());
            }
        }
        if refs_header.is_some() && line.starts_with(char::is_whitespace) {
            if let Some((ref_name, _)) = parse_entry(line) {
                if ref_name == name {
                    out.push(entry.clone());
                    replaced = true;
                    continue;
                }
                last_ref_entry = Some(out.len());
            }
        }
        out.push(line.to_string());
    }

    match (replaced, refs_header) {
        (true, _) => {}
        (false, Some(header)) => {
            let at = last_ref_entry.map(|i| i + 1).unwrap_or(header + 1);
            out.insert(at, entry);
        }
        (false, None) => {
            out.push("refs:".to_string());
            out.push(entry);
        }
    }
    let mut text = out.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Redacted descriptor for confirmations and pickers: never the literal key.
pub fn redact(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 8 {
        return "••••".into();
    }
    let prefix: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("aby-credentials-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A hand-written fixture at 0600: `std::fs::write` follows the umask,
    /// and a wide document is exactly what the loader refuses.
    fn write_fixture(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn store_then_read_round_trips() {
        let home = tmp_home("roundtrip");
        store_key(home.to_str().unwrap(), API_KEY_REF, "sk-abc123").unwrap();
        assert_eq!(
            stored_key(home.to_str().unwrap(), API_KEY_REF).unwrap(),
            Some("sk-abc123".into())
        );
        // The document is exactly the version-1 layout, owner-only.
        let text = std::fs::read_to_string(credentials_path(home.to_str().unwrap())).unwrap();
        assert_eq!(text, "version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-abc123\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(credentials_path(home.to_str().unwrap()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the document is never world-readable");
        }
    }

    #[test]
    fn store_patches_only_its_own_entry() {
        let home = tmp_home("patch");
        let path = credentials_path(home.to_str().unwrap());
        write_fixture(
            &path,
            "# keys live here\nversion: 1\nrefs:\n  OTHER_TOKEN: keep-me\n",
        );
        store_key(home.to_str().unwrap(), API_KEY_REF, "sk-new").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# keys live here\nversion: 1\nrefs:\n  OTHER_TOKEN: keep-me\n  DEEPSEEK_API_KEY: sk-new\n"
        );

        // Replacing keeps the sibling verbatim and drops the stale value.
        store_key(home.to_str().unwrap(), API_KEY_REF, "sk-newer").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("OTHER_TOKEN: keep-me"), "{text}");
        assert!(text.contains("DEEPSEEK_API_KEY: sk-newer"), "{text}");
        assert!(!text.contains("sk-new\n"), "{text}");
    }

    #[test]
    fn store_migrates_a_flat_document_in_the_same_write() {
        let home = tmp_home("flat");
        let path = credentials_path(home.to_str().unwrap());
        write_fixture(&path, "# pre-release\nDEEPSEEK_API_KEY: sk-old\n");
        store_key(home.to_str().unwrap(), API_KEY_REF, "sk-migrated").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "version: 1\nrefs:\n  # pre-release\n  DEEPSEEK_API_KEY: sk-migrated\n"
        );
        let refs = parse_document(&text).unwrap();
        assert_eq!(refs.get(API_KEY_REF), Some("sk-migrated"));
    }

    #[test]
    fn parse_rejects_unknown_and_broken_documents() {
        assert!(parse_document("version: 2\nrefs:\n")
            .unwrap_err()
            .contains("declares version 2"));
        assert!(parse_document("bad-name: x\n")
            .unwrap_err()
            .contains("unknown top-level key"));
        assert!(parse_document("version: 1\nrefs:\n  KEY:\n")
            .unwrap_err()
            .contains("is empty"));
        assert!(parse_document("version: 1\nrefs:\n  KEY: a\n  KEY: b\n")
            .unwrap_err()
            .contains("duplicate"));
        assert!(parse_document("version: 1\n  KEY: stray\n")
            .unwrap_err()
            .contains("nested lines belong"));
        // An empty/comment-only document is the empty store.
        assert_eq!(
            parse_document("# nothing\n").unwrap(),
            CredentialRefs::default()
        );
        // A pre-release flat layout is admitted.
        assert_eq!(
            parse_document("DEEPSEEK_API_KEY: sk-flat\n")
                .unwrap()
                .get(API_KEY_REF),
            Some("sk-flat")
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_refuses_group_or_other_readable_documents() {
        use std::os::unix::fs::PermissionsExt;

        let home = tmp_home("wide");
        let path = credentials_path(home.to_str().unwrap());
        std::fs::write(&path, "version: 1\nrefs:\n  KEY: sk-1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = stored_key(home.to_str().unwrap(), API_KEY_REF).unwrap_err();
        assert!(err.contains("chmod 600"), "{err}");

        // Store refuses too: the write path must not bless a wide document.
        assert!(store_key(home.to_str().unwrap(), API_KEY_REF, "sk-2").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "version: 1\nrefs:\n  KEY: sk-1\n"
        );
    }

    #[test]
    fn redact_never_carries_the_whole_key() {
        assert_eq!(redact("sk-abcdefgh1234"), "sk-…1234");
        assert_eq!(redact("short"), "••••");
        assert!(!redact("sk-abcdefgh1234").contains("abcdefgh"));
    }

    #[test]
    fn delete_removes_only_the_named_entry() {
        let home = tmp_home("delete");
        let path = credentials_path(home.to_str().unwrap());
        write_fixture(
            &path,
            "# keys live here\nversion: 1\nrefs:\n  OTHER_TOKEN: keep-me\n  DEEPSEEK_API_KEY: sk-gone\n",
        );

        assert!(delete_key(home.to_str().unwrap(), API_KEY_REF).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# keys live here\nversion: 1\nrefs:\n  OTHER_TOKEN: keep-me\n"
        );
        let refs = parse_document(&text).unwrap();
        assert_eq!(refs.get(API_KEY_REF), None);
        assert_eq!(refs.get("OTHER_TOKEN"), Some("keep-me"));

        // A second delete has nothing to remove.
        assert!(!delete_key(home.to_str().unwrap(), API_KEY_REF).unwrap());
    }

    #[test]
    fn delete_on_an_absent_or_unparseable_document_is_safe() {
        let home = tmp_home("delete-empty");
        // Absent: fine, nothing is created.
        assert!(!delete_key(home.to_str().unwrap(), API_KEY_REF).unwrap());
        assert!(!credentials_path(home.to_str().unwrap()).exists());

        // Unparseable: refused, never rewritten.
        let path = credentials_path(home.to_str().unwrap());
        write_fixture(&path, "version: 9\nrefs:\n");
        assert!(delete_key(home.to_str().unwrap(), API_KEY_REF).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "version: 9\nrefs:\n"
        );
    }

    #[test]
    fn delete_also_clears_a_flat_entry() {
        let home = tmp_home("delete-flat");
        let path = credentials_path(home.to_str().unwrap());
        write_fixture(&path, "DEEPSEEK_API_KEY: sk-flat\nOTHER: x\n");
        assert!(delete_key(home.to_str().unwrap(), API_KEY_REF).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "OTHER: x\n");
        assert!(parse_document(&text).unwrap().get(API_KEY_REF).is_none());
    }
}
