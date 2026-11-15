//! Saving a conversation to files: each message as an `.eml`, or the whole thread as text.
//!
//! Files are written to the user's Downloads folder and created owner-only on Unix, since they
//! hold mail.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// One message to export: its header fields for the text form, and its raw RFC 822 bytes.
pub struct Item<'a> {
    pub from: &'a str,
    pub subject: &'a str,
    pub date: &'a str,
    pub raw: &'a [u8],
}

/// The user's Downloads folder, if the system has one.
pub fn downloads_dir() -> Option<PathBuf> {
    directories::UserDirs::new()?
        .download_dir()
        .map(Path::to_path_buf)
}

/// Makes a name safe to use as a file or folder name on any system: path separators and
/// characters Windows refuses are replaced, and the result is short.
pub fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let trimmed: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = trimmed.chars().take(80).collect();
    let short = short.trim_end_matches(['.', ' ']).to_string();
    if short.is_empty() {
        "thread".to_string()
    } else {
        short
    }
}

/// Writes `bytes` to `path`, replacing any file there. A file it creates is owner-only.
pub fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// Writes `bytes` into `dir` as `name`, or as `name (2)` and so on if that is taken. The file is
/// owner-only. Returns its path.
pub fn write_download_to(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    let mut n = 1;
    loop {
        let file = if n == 1 {
            format!("{stem}{ext}")
        } else {
            format!("{stem} ({n}){ext}")
        };
        let path = dir.join(file);
        match new_private_file(&path) {
            Ok(mut out) => {
                out.write_all(bytes)?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

/// The whole thread as plain text: each message's headers, then its text, oldest first.
pub fn thread_text(items: &[Item]) -> String {
    let mut out = String::new();
    for item in items {
        out.push_str(&format!(
            "From: {}\nDate: {}\nSubject: {}\n\n",
            item.from, item.date, item.subject
        ));
        let body = crate::render::render_plaintext(item.raw)
            .unwrap_or_else(|| "(could not parse message)".to_string());
        out.push_str(body.trim_end());
        out.push_str("\n\n----\n\n");
    }
    out
}

/// Writes each message as its own `.eml`, numbered in order, in a new folder under `parent`.
/// The bytes are written exactly as the server sent them.
pub fn write_eml(parent: &Path, subject: &str, items: &[Item]) -> io::Result<PathBuf> {
    let folder = create_unique_dir(parent, &safe_file_name(subject))?;
    for (i, item) in items.iter().enumerate() {
        let path = folder.join(format!("{:03}.eml", i + 1));
        write_private(&path, item.raw)?;
    }
    Ok(folder)
}

/// Writes the thread as one text file under `parent`. Returns the file's path.
pub fn write_text(parent: &Path, subject: &str, items: &[Item]) -> io::Result<PathBuf> {
    let (path, mut file) = create_unique_file(parent, &safe_file_name(subject), "txt")?;
    file.write_all(thread_text(items).as_bytes())?;
    Ok(path)
}

/// A folder named `stem`, or `stem (2)`, `stem (3)`, ... if that one exists.
fn create_unique_dir(parent: &Path, stem: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(parent)?;
    let mut n = 1;
    loop {
        let name = if n == 1 {
            stem.to_string()
        } else {
            format!("{stem} ({n})")
        };
        let path = parent.join(name);
        match std::fs::create_dir(&path) {
            Ok(()) => {
                restrict_dir(&path)?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

/// A new file named `stem.ext`, or `stem (2).ext` and so on if that one exists. The file is
/// created owner-only and opened for writing.
fn create_unique_file(
    parent: &Path,
    stem: &str,
    ext: &str,
) -> io::Result<(PathBuf, std::fs::File)> {
    std::fs::create_dir_all(parent)?;
    let mut n = 1;
    loop {
        let name = if n == 1 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem} ({n}).{ext}")
        };
        let path = parent.join(name);
        match new_private_file(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

/// Writes a new owner-only file. Refuses to overwrite one.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    new_private_file(path)?.write_all(bytes)
}

fn new_private_file(path: &Path) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &[u8] = b"From: a@example.com\r\nSubject: Hi\r\n\r\nHello there\r\n";

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("export-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn downloads_never_overwrite_and_are_owner_only() {
        let dir = temp("download");
        let first = write_download_to(&dir, "notes.txt", b"one").unwrap();
        let second = write_download_to(&dir, "notes.txt", b"two").unwrap();
        assert_eq!(first.file_name().unwrap(), "notes.txt");
        assert_eq!(second.file_name().unwrap(), "notes (2).txt");
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        let third = write_download_to(&dir, "README", b"x").unwrap();
        assert_eq!(third.file_name().unwrap(), "README");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&first).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_file_name("a/b:c?"), "a_b_c_");
        assert_eq!(safe_file_name("  spaced   out  "), "spaced out");
        assert_eq!(safe_file_name("   "), "thread");
        assert_eq!(safe_file_name("end."), "end");
        assert_eq!(safe_file_name(&"x".repeat(200)).chars().count(), 80);
    }

    #[test]
    fn text_has_headers_and_the_message_text() {
        let items = [Item {
            from: "a@example.com",
            subject: "Hi",
            date: "10 Nov 2026",
            raw: RAW,
        }];
        let text = thread_text(&items);
        assert!(text.contains("From: a@example.com"));
        assert!(text.contains("Subject: Hi"));
        assert!(text.contains("Hello there"));
    }

    #[test]
    fn eml_files_hold_the_raw_bytes_in_order() {
        let parent = temp("eml");
        let items = [
            Item {
                from: "",
                subject: "Hi",
                date: "",
                raw: RAW,
            },
            Item {
                from: "",
                subject: "Re: Hi",
                date: "",
                raw: b"Subject: Re: Hi\r\n\r\nBack\r\n",
            },
        ];
        let folder = write_eml(&parent, "Hi", &items).unwrap();
        assert_eq!(std::fs::read(folder.join("001.eml")).unwrap(), RAW);
        assert_eq!(
            std::fs::read(folder.join("002.eml")).unwrap(),
            b"Subject: Re: Hi\r\n\r\nBack\r\n"
        );
        // A second export of the same thread goes to a new folder, not over the first.
        let again = write_eml(&parent, "Hi", &items).unwrap();
        assert_ne!(again, folder);
        std::fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn text_exports_do_not_overwrite_each_other() {
        let parent = temp("text");
        let items = [Item {
            from: "a@example.com",
            subject: "Hi",
            date: "",
            raw: RAW,
        }];
        let first = write_text(&parent, "Hi", &items).unwrap();
        let second = write_text(&parent, "Hi", &items).unwrap();
        assert_eq!(first.file_name().unwrap(), "Hi.txt");
        assert_eq!(second.file_name().unwrap(), "Hi (2).txt");
        std::fs::remove_dir_all(&parent).ok();
    }
}
