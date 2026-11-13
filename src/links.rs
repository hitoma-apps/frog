//! Links into the Gmail web UI.

/// Gmail web search for one message, by its `Message-ID` header.
///
/// `message_id` is given without angle brackets, as `mail-parser` returns it.
/// Search by Message-ID works for any message, so it doesn't depend on
/// mapping Gmail's internal IDs into web URLs. It opens the default account
/// (`/u/0/`).
pub fn gmail_open_url(message_id: &str) -> String {
    let bare = message_id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>');
    let query = format!("rfc822msgid:<{bare}>");
    format!(
        "https://mail.google.com/mail/u/0/#search/{}",
        percent_encode(&query)
    )
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_wraps_message_id_in_brackets_and_encodes() {
        assert_eq!(
            gmail_open_url("abc@mail.gmail.com"),
            "https://mail.google.com/mail/u/0/#search/rfc822msgid%3A%3Cabc%40mail.gmail.com%3E"
        );
    }

    #[test]
    fn accepts_ids_already_in_brackets() {
        assert_eq!(gmail_open_url("<x@y>"), gmail_open_url("x@y"));
    }
}
