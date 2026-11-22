//! Plaintext rendering of a raw RFC 5322 message: a header block and the body.
//!
//! Body selection is delegated to `mail-parser`, which prefers a `text/plain`
//! part. When only `text/html` exists, it converts that to text.

use std::fmt::Write;

use mail_parser::{Address, AddressList, Mailbox, MessageParser};

/// Renders `From`, `To`, `Date`, `Subject`, then the plaintext body.
/// Returns `None` if the bytes don't parse as a message at all.
pub fn render_plaintext(raw: &[u8]) -> Option<String> {
    let msg = MessageParser::default().parse(raw)?;
    let mut out = String::new();

    write_addresses(&mut out, "From", msg.from().as_ref());
    write_addresses(&mut out, "To", msg.to().as_ref());
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

/// Every mailbox in an address list, in order, with a group's members in place of the group.
pub fn mailboxes<'m>(list: &AddressList<'m>) -> Vec<Mailbox<'m>> {
    let mut out = Vec::new();
    for addr in list.iter() {
        match addr {
            Address::Mailbox(mailbox) => out.push(mailbox),
            Address::Group(group) => out.extend(group.mailboxes()),
        }
    }
    out
}

/// Writes one `Label: value` line per address. Continuation recipients are
/// indented under the first value.
fn write_addresses(out: &mut String, label: &str, addrs: Option<&AddressList<'_>>) {
    let Some(addrs) = addrs else { return };
    let indent = " ".repeat(label.len() + 2);
    for (i, addr) in mailboxes(addrs).iter().enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_recipients_are_listed_one_by_one() {
        let raw = b"From: Ana <ana@example.com>\r\nTo: Team: bo@example.com, cy@example.com;, Dee <dee@example.com>\r\nSubject: x\r\n\r\nbody\r\n";
        let text = render_plaintext(raw).expect("parses");
        assert!(text.contains("To: bo@example.com"), "{text}");
        assert!(text.contains("cy@example.com"), "{text}");
        assert!(text.contains("Dee <dee@example.com>"), "{text}");
    }
}
