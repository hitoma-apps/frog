//! The summary that Gmail's "Show original" view puts above a message's source.

use mail_parser::{AddressList, MessageParser};

/// The summary fields, in display order, from a message's header block. A field the
/// message doesn't have is left out.
pub fn summary(headers: &[u8]) -> Vec<(&'static str, String)> {
    let Some(m) = MessageParser::default().parse(headers) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(id) = m.message_id() {
        out.push(("Message ID", format!("<{id}>")));
    }
    if let Some(date) = m.date() {
        out.push(("Date", date.to_rfc822()));
    }
    if let Some(from) = m.from() {
        out.push(("From", addresses(&from)));
    }
    if let Some(to) = m.to() {
        out.push(("To", addresses(&to)));
    }
    if let Some(subject) = m.subject() {
        out.push(("Subject", subject.to_string()));
    }
    out
}

/// "Name <address>" for each address, joined with commas.
fn addresses(list: &AddressList) -> String {
    crate::render::mailboxes(list)
        .into_iter()
        .filter_map(|a| match (a.name(), a.address()) {
            (Some(name), Some(email)) => Some(format!("{name} <{email}>")),
            (None, Some(email)) => Some(email.to_string()),
            (Some(name), None) => Some(name.to_string()),
            (None, None) => None,
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarises_the_headers_in_order() {
        let headers = b"Message-ID: <abc@example.com>\r\nDate: Tue, 10 Nov 2026 09:30:00 +0000\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.org>, carol@example.net\r\nSubject: Hello there\r\n\r\n";
        let s = summary(headers);
        let names: Vec<&str> = s.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["Message ID", "Date", "From", "To", "Subject"]);
        assert_eq!(s[0].1, "<abc@example.com>");
        assert_eq!(s[2].1, "Alice <alice@example.com>");
        assert_eq!(s[3].1, "Bob <bob@example.org>, carol@example.net");
        assert_eq!(s[4].1, "Hello there");
    }

    #[test]
    fn missing_fields_are_left_out() {
        let s = summary(b"Subject: Only this\r\n\r\n");
        assert_eq!(s, vec![("Subject", "Only this".to_string())]);
    }
}
