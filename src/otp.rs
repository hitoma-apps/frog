//! One-time login codes in messages: finding them in a message's text, so they can be copied,
//! and recognising the subjects of code emails, so they can be cleaned up.
//!
//! The detection is by wording and shape, not by sender. A code is a run of 4 to 8 digits, which
//! may be split into groups by one space or hyphen ("123 456", "123-456"), that appears within a
//! short distance after a phrase such as "verification code". Years, numbers inside words and
//! numbers in links are skipped. A miss or a false match is possible, so the app always shows the
//! code for the user to check before using it.

/// Phrases that introduce a code, lowercase.
const PHRASES: [&str; 15] = [
    "verification code",
    "your code",
    "security code",
    "login code",
    "sign-in code",
    "sign in code",
    "authentication code",
    "auth code",
    "one-time code",
    "one time code",
    "one-time password",
    "one time password",
    "passcode",
    "two-factor",
    "2fa",
];

/// How far after a phrase a code may start, in characters.
const REACH: usize = 60;

/// Codes in `text`, in the order they appear, without duplicates. At most three.
pub fn find(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut found: Vec<String> = Vec::new();
    for phrase in PHRASES {
        for (at, _) in lower.match_indices(phrase) {
            let tail: String = lower[at + phrase.len()..].chars().take(REACH).collect();
            if let Some(code) = first_code(&tail)
                && !found.contains(&code)
            {
                found.push(code);
                if found.len() == 3 {
                    return found;
                }
            }
        }
    }
    found
}

/// Whether a subject reads like a code email, so it's worth showing in the cleanup list.
pub fn is_code_subject(subject: &str) -> bool {
    let lower = subject.to_lowercase();
    PHRASES.iter().any(|p| lower.contains(p))
        || lower.contains("verification")
        || lower.contains("otp")
}

/// The first code-shaped token in `s`, if any.
fn first_code(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // A code starts after the start of the text, a space, or punctuation such as ':'. Not
        // after '=', '/' or '?', so a number in a link is skipped.
        let starts_here = i == 0
            || matches!(
                chars[i - 1],
                ' ' | '\t' | '\n' | ':' | '.' | ',' | '(' | '"' | '\''
            );
        if chars[i].is_ascii_digit() && starts_here {
            if let Some((code, _end)) = code_at(&chars, i) {
                return Some(code);
            }
            // Skip the rest of this digit run, so "1234567890" isn't read from its middle.
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    None
}

/// A code starting at `start`: 4 to 8 digits, optionally split once into groups of 3 or 4 by a
/// space or hyphen. Returns the digits and the index just past the code.
fn code_at(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut digits = String::new();
    let mut i = start;
    while i < chars.len() && chars[i].is_ascii_digit() {
        digits.push(chars[i]);
        i += 1;
    }
    // A split code: "123 456" or "123-456".
    if digits.len() >= 3
        && digits.len() <= 4
        && i + 1 < chars.len()
        && (chars[i] == ' ' || chars[i] == '-')
        && chars[i + 1].is_ascii_digit()
    {
        let mut j = i + 1;
        let mut second = String::new();
        while j < chars.len() && chars[j].is_ascii_digit() {
            second.push(chars[j]);
            j += 1;
        }
        if (3..=4).contains(&second.len()) && digits.len() + second.len() <= 8 {
            digits.push_str(&second);
            i = j;
        }
    }
    // Not part of a longer word or number, such as an ID or a link.
    if i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '/' || chars[i] == '=') {
        return None;
    }
    if !(4..=8).contains(&digits.len()) {
        return None;
    }
    // Years are not codes.
    if digits.len() == 4 && (digits.starts_with("19") || digits.starts_with("20")) {
        return None;
    }
    Some((digits, i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_codes_after_a_phrase() {
        assert_eq!(find("Your verification code is 482913."), vec!["482913"]);
        assert_eq!(find("Login code: 1234"), vec!["1234"]);
        assert_eq!(
            find("Your code is 123 456 - do not share it."),
            vec!["123456"]
        );
        assert_eq!(
            find("Use 555-123 as your one-time code"),
            Vec::<String>::new()
        );
        assert_eq!(find("Your 2FA code: 9087"), vec!["9087"]);
    }

    #[test]
    fn skips_years_links_and_words() {
        assert!(find("Your security code expires in 2026, come back soon.").is_empty());
        assert!(find("verification code: https://example.com/confirm?token=998877").is_empty());
        assert!(find("verification code: abc123456def").is_empty());
    }

    #[test]
    fn ignores_text_without_a_phrase() {
        assert!(find("Your invoice number is 482913.").is_empty());
        assert!(find("Meet at 7 pm, room 1234").is_empty());
    }

    #[test]
    fn recognises_code_subjects() {
        assert!(is_code_subject("Your verification code"));
        assert!(is_code_subject("482913 is your sign-in code"));
        assert!(!is_code_subject("Lunch on Friday?"));
    }
}
