//! Links into the Gmail web UI.

/// Gmail web URL for the conversation that contains a message.
///
/// Takes `X-GM-THRID` and formats it as hex, as Gmail's `#all/` route
/// expects. It opens the default account (`/u/0/`) and loads the full UI.
pub fn gmail_thread_url(thrid: u64) -> String {
    format!("https://mail.google.com/mail/u/0/#all/{thrid:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_url_uses_hex_thread_id() {
        assert_eq!(
            gmail_thread_url(1881704175591005469),
            "https://mail.google.com/mail/u/0/#all/1a1d27fa5e789d1d"
        );
    }
}

/// Gmail's "View original" page for one message, by its `X-GM-MSGID`. `session_key`
/// is the `ik` value from a Gmail URL. It's optional, and kept out of the URL when absent.
pub fn gmail_original_url(msgid: u64, session_key: Option<&str>) -> String {
    let ik = session_key.map(|k| format!("ik={k}&")).unwrap_or_default();
    format!("https://mail.google.com/mail/u/0/?{ik}view=om&permmsgid=msg-f%3A{msgid}")
}

#[cfg(test)]
mod original_tests {
    use super::*;

    #[test]
    fn original_url_matches_gmail_format() {
        assert_eq!(
            gmail_original_url(1881334941542781295, Some("2214ddb3cb")),
            "https://mail.google.com/mail/u/0/?ik=2214ddb3cb&view=om&permmsgid=msg-f%3A1881334941542781295"
        );
        assert_eq!(
            gmail_original_url(42, None),
            "https://mail.google.com/mail/u/0/?view=om&permmsgid=msg-f%3A42"
        );
    }
}
