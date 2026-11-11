//! Plaintext rendering of a raw RFC 5322 message: a header block and the body.
//!
//! Body selection is delegated to `mail-parser`, which prefers a `text/plain`
//! part. When only `text/html` exists, it converts that to text.

use std::fmt::Write;

use mail_parser::{Address, MessageParser};

/// Renders `From`, `To`, `Date`, `Subject`, then the plaintext body.
/// Returns `None` if the bytes don't parse as a message at all.
pub fn render_plaintext(raw: &[u8]) -> Option<String> {
    let msg = MessageParser::default().parse(raw)?;
    let mut out = String::new();

    write_addresses(&mut out, "From", msg.from());
    write_addresses(&mut out, "To", msg.to());
    if let Some(date) = msg.date() {
        let _ = writeln!(out, "Date: {}", date.to_rfc3339());
    }
    if let Some(subject) = msg.subject() {
        let _ = writeln!(out, "Subject: {subject}");
    }
    out.push('\n');

    match msg.body_text(0) {
        Some(body) => out.push_str(normalize_newlines(&body).as_ref()),
        None => out.push_str("(no text body)\n"),
    }
    Some(out)
}

/// Writes one `Label: value` line per address. Continuation recipients are
/// indented under the first value.
fn write_addresses(out: &mut String, label: &str, addrs: Option<&Address<'_>>) {
    let Some(addrs) = addrs else { return };
    let indent = " ".repeat(label.len() + 2);
    for (i, addr) in addrs.iter().enumerate() {
        if i == 0 {
            let _ = write!(out, "{label}: ");
        } else {
            out.push_str(&indent);
        }
        match (addr.name(), addr.address()) {
            (Some(name), Some(email)) => {
                let _ = writeln!(out, "{name} <{email}>");
            }
            (None, Some(email)) => {
                let _ = writeln!(out, "{email}");
            }
            (Some(name), None) => {
                let _ = writeln!(out, "{name}");
            }
            (None, None) => {
                let _ = writeln!(out, "(unknown)");
            }
        }
    }
}

/// Converts CRLF to LF. Borrows when no conversion is needed.
fn normalize_newlines(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains('\r') {
        std::borrow::Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}
