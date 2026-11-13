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
