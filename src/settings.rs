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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Searches saved in the sidebar, in the order they were saved.
    #[serde(default)]
    pub saved_searches: Vec<String>,
    /// The main window's size and position, and the sidebar and list sizes, from last time.
    #[serde(default)]
    pub window: WindowLayout,
}

/// Where the main window was and how its panels were sized. Pixels, in logical units.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowLayout {
    pub size: [f32; 2],
    /// `None` until the window has been placed. The system then chooses.
    pub position: Option<[f32; 2]>,
    pub sidebar_width: f32,
    /// Height of the thread list, above the reader.
    pub list_height: f32,
}

impl Default for WindowLayout {
    fn default() -> Self {
        Self {
            size: [1280.0, 800.0],
            position: None,
            sidebar_width: 200.0,
            list_height: 260.0,
        }
    }
}

/// Most searches kept in the sidebar.
pub const MAX_SAVED_SEARCHES: usize = 10;

impl Default for Settings {
    fn default() -> Self {
        Self {
            email: None,
            remember_password: false,
            check_interval_secs: 60,
            page_size: 500,
            notifications: true,
            favorite_labels: Vec::new(),
            saved_searches: Vec::new(),
            window: WindowLayout::default(),
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
        // A hand-edited file can't keep more searches than the sidebar shows, or repeats.
        let mut kept: Vec<String> = Vec::new();
        for search in self.saved_searches.drain(..) {
            if !search.trim().is_empty() && !kept.contains(&search) {
                kept.push(search);
            }
        }
        kept.truncate(MAX_SAVED_SEARCHES);
        self.saved_searches = kept;
        self.window.size = [
            self.window.size[0].max(480.0),
            self.window.size[1].max(320.0),
        ];
        self.window.sidebar_width = self.window.sidebar_width.clamp(120.0, 600.0);
        self.window.list_height = self.window.list_height.clamp(80.0, 2000.0);
        self
    }

    /// Saves a search. Returns `false` if it's blank, already saved, or the list is full.
    pub fn save_search(&mut self, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty()
            || self.saved_searches.len() >= MAX_SAVED_SEARCHES
            || self.saved_searches.iter().any(|s| s == text)
        {
            return false;
        }
        self.saved_searches.push(text.to_string());
        true
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
            window: WindowLayout::default(),
            email: Some("me@example.com".into()),
            remember_password: true,
            check_interval_secs: 120,
            page_size: 200,
            notifications: false,
            favorite_labels: vec!["\\Starred".into(), "Work".into()],
            saved_searches: Vec::new(),
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

    #[test]
    fn saved_searches_are_unique_and_capped_at_ten() {
        let mut s = Settings::default();
        assert!(s.save_search("  invoice  "));
        assert!(!s.save_search("invoice"), "a repeat is refused");
        assert!(!s.save_search("   "), "a blank search is refused");
        for i in 0..MAX_SAVED_SEARCHES - 1 {
            assert!(s.save_search(&format!("search {i}")));
        }
        assert_eq!(s.saved_searches.len(), MAX_SAVED_SEARCHES);
        assert!(!s.save_search("one too many"), "the list is full");
    }

    #[test]
    fn a_hand_edited_file_is_brought_back_into_range() {
        let s = Settings {
            saved_searches: (0..15)
                .map(|i| format!("s{}", i % 12))
                .chain(std::iter::once(String::new()))
                .collect(),
            ..Settings::default()
        }
        .clamped();
        assert_eq!(s.saved_searches.len(), MAX_SAVED_SEARCHES);
        let unique: std::collections::BTreeSet<_> = s.saved_searches.iter().collect();
        assert_eq!(unique.len(), s.saved_searches.len());
    }
}

#[cfg(test)]
mod window_layout_tests {
    use super::*;

    #[test]
    fn layout_round_trips_through_the_settings_file() {
        let dir = std::env::temp_dir().join(format!("layout-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let s = Settings {
            window: WindowLayout {
                size: [1500.0, 900.0],
                position: Some([40.0, 60.0]),
                sidebar_width: 240.0,
                list_height: 300.0,
            },
            ..Settings::default()
        };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).window, s.window);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_without_a_layout_gets_the_default_and_bad_sizes_are_clamped() {
        let dir = std::env::temp_dir().join(format!("layout-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"email":"a@example.com"}"#).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded.window, WindowLayout::default());
        let mut s = loaded;
        s.window.size = [10.0, 10.0];
        s.window.sidebar_width = 9000.0;
        let s = s.clamped();
        assert_eq!(s.window.size, [480.0, 320.0]);
        assert_eq!(s.window.sidebar_width, 600.0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
