//! User settings that persist between runs: the last email address, whether to
//! remember the password, the check interval, and the page size.
//!
//! Passwords are not stored here. The app keeps them in the OS credential store.

use std::io;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Seconds between automatic checks.
pub const CHECK_INTERVAL_RANGE: RangeInclusive<u64> = 30..=3600;
/// Messages loaded per page.
pub const PAGE_SIZE_RANGE: RangeInclusive<usize> = 50..=5000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The last address signed in with, pre-filled on the sign-in form.
    pub email: Option<String>,
    /// Whether the password is kept in the OS credential store.
    pub remember_password: bool,
    /// Seconds between automatic checks, when live updates aren't running.
    pub check_interval_secs: u64,
    /// Messages to load on each page.
    pub page_size: usize,
    /// Show a desktop notification when new mail arrives.
    pub notifications: bool,
    /// Raw label names pinned to the top of the sidebar, in the order they were added.
    pub favorite_labels: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            email: None,
            remember_password: false,
            check_interval_secs: 60,
            page_size: 500,
            notifications: true,
            favorite_labels: Vec::new(),
        }
    }
}

impl Settings {
    /// Brings values into their allowed ranges, so a hand-edited file can't break the app.
    pub fn clamped(mut self) -> Self {
        self.check_interval_secs = self
            .check_interval_secs
            .clamp(*CHECK_INTERVAL_RANGE.start(), *CHECK_INTERVAL_RANGE.end());
        self.page_size = self
            .page_size
            .clamp(*PAGE_SIZE_RANGE.start(), *PAGE_SIZE_RANGE.end());
        self
    }

    /// Reads settings from `path`. A missing or unreadable file gives the defaults.
    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Settings>(&text).ok())
            .unwrap_or_default()
            .clamped()
    }

    /// Writes settings to `path`, replacing the file in one step.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

/// Where settings live: `<config dir>/settings.json`. `None` if the platform has no config directory.
pub fn default_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "gmail-labels-proto")?;
    Some(dirs.config_dir().join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gmail-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn defaults_when_there_is_no_file() {
        let path = temp_file("missing.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(Settings::load_from(&path), Settings::default());
    }

    #[test]
    fn round_trips() {
        let path = temp_file("roundtrip.json");
        let s = Settings {
            email: Some("me@example.com".into()),
            remember_password: true,
            check_interval_secs: 120,
            page_size: 200,
            notifications: false,
            favorite_labels: vec!["\\Starred".into(), "Work".into()],
        };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), s);
    }

    #[test]
    fn corrupt_file_gives_defaults() {
        let path = temp_file("corrupt.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let s = Settings {
            check_interval_secs: 1,
            page_size: 1_000_000,
            ..Settings::default()
        }
        .clamped();
        assert_eq!(s.check_interval_secs, 30);
        assert_eq!(s.page_size, 5000);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let path = temp_file("unknown.json");
        std::fs::write(&path, r#"{"page_size": 300, "future_setting": true}"#).unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.page_size, 300);
        assert_eq!(s.check_interval_secs, 60);
    }
}
