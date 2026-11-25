//! The app's log: timestamped lines, each tagged with the subsystem that wrote it, such as IMAP,
//! CACHE or WORKER. Lines are kept in memory for the Log window, and appended to a file in the
//! data folder (owner-only), which rolls over when it grows.
//!
//! Lines never include mail content: no subjects, addresses or bodies. They describe what the
//! app did and how long it took.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// How many lines the Log window keeps in memory. The log file has every line.
const KEEP: usize = 2000;
/// The log file rolls over to `<name>.1` once it's this big.
const ROLL_BYTES: u64 = 2 * 1024 * 1024;

struct Log {
    lines: VecDeque<String>,
    file: Option<(PathBuf, File)>,
}

static LOG: OnceLock<Mutex<Log>> = OnceLock::new();

fn state() -> &'static Mutex<Log> {
    LOG.get_or_init(|| {
        Mutex::new(Log {
            lines: VecDeque::with_capacity(KEEP),
            file: None,
        })
    })
}

/// Writes lines to `path` as well, from now on. Creates the file owner-only.
pub fn write_to(path: &Path) {
    let mut log = state().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(file) = open_private(path) {
        log.file = Some((path.to_path_buf(), file));
    }
}

fn open_private(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Records one line for `subsystem`, such as "IMAP" or "CACHE".
pub fn log(subsystem: &str, text: impl AsRef<str>) {
    let at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let line = format!("{at} - {subsystem} - {}", text.as_ref());
    let mut log = state().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((path, file)) = &mut log.file {
        let _ = writeln!(file, "{line}");
        if file.metadata().is_ok_and(|m| m.len() > ROLL_BYTES) {
            let mut rolled = path.clone().into_os_string();
            rolled.push(".1");
            let _ = std::fs::rename(&*path, &rolled);
            let path = path.clone();
            log.file = open_private(&path).ok().map(|f| (path, f));
        }
    }
    if log.lines.len() == KEEP {
        log.lines.pop_front();
    }
    log.lines.push_back(line);
}

/// The lines in memory, oldest first.
pub fn snapshot() -> Vec<String> {
    let log = state().lock().unwrap_or_else(|e| e.into_inner());
    log.lines.iter().cloned().collect()
}

/// Runs `f` with the lines in memory, oldest first, without copying them. The logger waits
/// while `f` runs, so keep it short.
pub fn with_lines<R>(f: impl FnOnce(&VecDeque<String>) -> R) -> R {
    let log = state().lock().unwrap_or_else(|e| e.into_inner());
    f(&log.lines)
}

/// Runs `f`, and records how long it took.
pub fn timed<T>(subsystem: &str, what: &str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let out = f();
    log(subsystem, format!("{what} took {:.2?}", started.elapsed()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lines_can_be_read_in_place() {
        log("TEST", "in place");
        let found = with_lines(|lines| lines.iter().any(|l| l.contains("in place")));
        assert!(found);
    }

    #[test]
    fn lines_carry_a_timestamp_and_subsystem() {
        log("TEST", "hello");
        let line = snapshot()
            .into_iter()
            .rfind(|l| l.contains("hello"))
            .expect("the line is kept");
        let (at, rest) = line.split_once(" - ").expect("timestamp separator");
        assert!(at.ends_with('Z') && at.len() == 24, "{at}");
        assert!(rest.starts_with("TEST - hello"), "{rest}");
    }
}
