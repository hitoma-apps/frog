//! Warnings for senders who may be posing as someone else.
//!
//! Two things are checked: a display name that is an address other than the real sender's, and
//! a sender domain that looks like another one (the homograph checks `linkcheck` runs on links).

use crate::linkcheck;

/// Reasons the sender may be impersonating someone. Empty means nothing looks wrong.
///
/// `name` is the display name, or the address when the message has no name. `address` is the
/// sender's address as the server reported it.
pub fn sender_warnings(name: &str, address: &str) -> Vec<String> {
    let address = address.trim().to_ascii_lowercase();
    let name = name.trim();
    let mut out = Vec::new();
    if looks_like_address(name) && !name.eq_ignore_ascii_case(&address) {
        out.push(format!(
            "The name shows {name}, but the mail is from {address}."
        ));
    }
    if let Some((_, domain)) = address.rsplit_once('@').filter(|(_, d)| !d.is_empty()) {
        for warning in linkcheck::analyze(&format!("https://{domain}")).warnings {
            out.push(format!("The domain {domain}: {warning}"));
        }
    }
    out
}

/// Whether a display name is an email address: a user part, `@`, and a dotted domain, with no
/// spaces.
fn looks_like_address(s: &str) -> bool {
    match s.rsplit_once('@') {
        Some((user, domain)) => {
            !user.is_empty() && domain.contains('.') && !s.contains(char::is_whitespace)
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_sender_is_not_flagged() {
        assert!(sender_warnings("Alice Smith", "alice@example.com").is_empty());
        assert!(sender_warnings("alice@example.com", "Alice@Example.com").is_empty());
        assert!(sender_warnings("alice@example.com", "alice@example.com").is_empty());
    }

    #[test]
    fn a_name_that_is_another_address_is_flagged() {
        let warnings = sender_warnings("support@paypal.com", "x@evil.example");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("support@paypal.com") && w.contains("x@evil.example")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_lookalike_sender_domain_is_flagged() {
        // The middle letter is Cyrillic а, so the domain mixes two scripts.
        let warnings = sender_warnings("Support", "support@p\u{0430}ypal.com");
        assert!(
            warnings.iter().any(|w| w.contains("The domain")),
            "{warnings:?}"
        );
    }
}
