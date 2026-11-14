//! Attachments in a message: listing them, and reading one's decoded contents.

use mail_parser::{MessageParser, MimeHeaders};

/// One attachment, as the message lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentInfo {
    /// A file name that's safe to write. Made up from the position if the message gives none.
    pub name: String,
    /// The content type, such as `application/pdf`.
    pub content_type: String,
    /// The decoded size in bytes.
    pub size: usize,
}

/// The attachments of a raw message, in the order they appear.
pub fn list(raw: &[u8]) -> Vec<AttachmentInfo> {
    let Some(m) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    m.attachments()
        .enumerate()
        .map(|(i, part)| AttachmentInfo {
            name: part
                .attachment_name()
                .map(safe_name)
                .unwrap_or_else(|| format!("attachment-{}", i + 1)),
            content_type: part
                .content_type()
                .map(|ct| match ct.subtype() {
                    Some(sub) => format!("{}/{sub}", ct.ctype()),
                    None => ct.ctype().to_string(),
                })
                .unwrap_or_else(|| "application/octet-stream".into()),
            size: part.contents().len(),
        })
        .collect()
}

/// The decoded bytes of attachment `index`, counted as `list` counts them.
pub fn contents(raw: &[u8], index: usize) -> Option<Vec<u8>> {
    let m = MessageParser::default().parse(raw)?;
    m.attachments()
        .nth(index)
        .map(|part| part.contents().to_vec())
}

/// A file name safe to write: no folders, no control characters, no leading dots.
pub fn safe_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = last
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    if cleaned.is_empty() {
        "attachment".into()
    } else {
        cleaned.to_string()
    }
}

/// Whether a file name is a program or script, which can run code when opened.
pub fn is_program(name: &str) -> bool {
    const PROGRAMS: [&str; 16] = [
        "exe", "bat", "cmd", "com", "scr", "msi", "vbs", "vbe", "js", "jse", "wsf", "ps1", "lnk",
        "hta", "jar", "dll",
    ];
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| PROGRAMS.iter().any(|p| ext.eq_ignore_ascii_case(p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGE: &str = "From: a@example.com\r\nSubject: x\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n--b\r\nContent-Type: text/plain; name=\"notes.txt\"\r\nContent-Disposition: attachment; filename=\"notes.txt\"\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--b--\r\n";

    #[test]
    fn lists_and_decodes_an_attachment() {
        let raw = MESSAGE.as_bytes();
        let listed = list(raw);
        assert_eq!(
            listed,
            vec![AttachmentInfo {
                name: "notes.txt".into(),
                content_type: "text/plain".into(),
                size: 5,
            }]
        );
        assert_eq!(contents(raw, 0).as_deref(), Some(&b"hello"[..]));
        assert_eq!(contents(raw, 1), None);
    }

    #[test]
    fn a_message_without_attachments_lists_none() {
        assert!(list(b"From: a@example.com\r\n\r\nJust text.\r\n").is_empty());
    }

    #[test]
    fn names_lose_their_folders_and_unsafe_characters() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\temp\\a:b?.txt"), "a_b_.txt");
        assert_eq!(safe_name("  .hidden  "), "hidden");
        assert_eq!(safe_name(""), "attachment");
    }

    #[test]
    fn program_files_are_flagged() {
        assert!(is_program("setup.EXE"));
        assert!(is_program("invoice.js"));
        assert!(!is_program("report.pdf"));
        assert!(!is_program("exe"));
    }
}
