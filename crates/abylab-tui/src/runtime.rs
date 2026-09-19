//! Shared launch/display configuration.

use std::path::{Path, PathBuf};

/// `$ABYLAB_HOME`, else `~/.abylab`.
pub fn aby_home_from(abylab_home: Option<&str>, user_home: &str) -> PathBuf {
    if let Some(home) = abylab_home.filter(|value| !value.is_empty()) {
        return PathBuf::from(home);
    }
    Path::new(user_home).join(".abylab")
}

/// How the agent's API key was resolved. The managed store (`/login`) is the
/// only durable source; `--api-key` is an explicit one-run override that
/// wins while it is present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOrigin {
    Flag,
    Stored,
}

pub fn aby_home() -> PathBuf {
    let abylab = std::env::var("ABYLAB_HOME").ok();
    let user = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    aby_home_from(abylab.as_deref(), &user)
}

pub fn default_sessions_root() -> PathBuf {
    aby_home().join("sessions")
}

pub fn settings_path(home: &str) -> PathBuf {
    Path::new(home).join("settings.json")
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub workspace: String,
    /// aby home directory: `settings.json` and the modes cache live here.
    pub home: String,
    /// Shared abycore session store root (`~/.abylab/sessions` by default);
    /// partitions live under per-workspace slugs.
    pub sessions_root: String,
    pub provider: String,
    pub model: String,
    pub max_tokens: Option<u64>,
    pub base_url: Option<String>,
    /// The fully resolved API key (`--api-key` override, else the `/login`
    /// store), ready for [`crate::controller`] to hand to the driver.
    pub api_key: Option<String>,
    /// How [`Self::api_key`] was resolved; `None` when no key is present.
    pub key_origin: Option<KeyOrigin>,
}

impl RuntimeConfig {
    /// Resolve the API key: the `--api-key` launch override wins while it is
    /// present, otherwise the key saved through `/login`. The store is still
    /// validated (loud on boot, like the Harness document reader) even when
    /// an override wins. The environment is not consulted — `/login` is the
    /// only durable source of a key.
    pub fn resolve_credentials(
        flag: Option<&str>,
        home: &str,
    ) -> Result<(Option<String>, Option<KeyOrigin>), String> {
        // Loud on boot: a document this build cannot prove it understands is
        // surfaced even when this run's key comes from the launch override.
        let stored = crate::credentials::stored_key(home, crate::credentials::API_KEY_REF)?;
        let (key, origin) = if let Some(key) = flag.map(str::trim).filter(|key| !key.is_empty()) {
            (Some(key.to_string()), Some(KeyOrigin::Flag))
        } else if let Some(key) = stored {
            (Some(key), Some(KeyOrigin::Stored))
        } else {
            (None, None)
        };
        Ok((key, origin))
    }

    pub fn has_credentials(&self) -> bool {
        self.api_key.is_some()
    }

    /// Human description of where the API key comes from. Never the value.
    pub fn credential_source(&self) -> Option<String> {
        match self.key_origin {
            Some(KeyOrigin::Flag) => Some("--api-key flag".into()),
            Some(KeyOrigin::Stored) => Some(format!(
                "stored · {}",
                crate::credentials::credentials_path(&self.home).display()
            )),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fresh_home(tag: &str) -> String {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "aby-runtime-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn the_login_store_is_the_only_durable_key_source() {
        let home = fresh_home("none");
        let (key, origin) = RuntimeConfig::resolve_credentials(None, &home).unwrap();
        assert_eq!(key, None);
        assert_eq!(origin, None);

        crate::credentials::store_key(&home, crate::credentials::API_KEY_REF, "sk-stored0001")
            .unwrap();
        let (key, origin) = RuntimeConfig::resolve_credentials(None, &home).unwrap();
        assert_eq!(key.as_deref(), Some("sk-stored0001"));
        assert_eq!(origin, Some(KeyOrigin::Stored));
    }

    #[test]
    fn the_launch_override_wins_over_the_login_store() {
        let home = fresh_home("flag");
        crate::credentials::store_key(&home, crate::credentials::API_KEY_REF, "sk-stored0001")
            .unwrap();
        let (key, origin) =
            RuntimeConfig::resolve_credentials(Some("sk-flag0000001"), &home).unwrap();
        assert_eq!(key.as_deref(), Some("sk-flag0000001"));
        assert_eq!(origin, Some(KeyOrigin::Flag));

        // A blank override is not an override: the stored key still runs.
        let (key, origin) = RuntimeConfig::resolve_credentials(Some("   "), &home).unwrap();
        assert_eq!(key.as_deref(), Some("sk-stored0001"));
        assert_eq!(origin, Some(KeyOrigin::Stored));
    }
}
