//! The search box: plain words and `name:value` operators, turned into a query.
//!
//! Criteria apply to each message, and a thread is listed through its matching messages. Quotes
//! group a value that has spaces, as in `label:"Work stuff"`. Quoted phrases, negation and OR are
//! not supported yet (issues track them), so their characters are read as plain words.

use chrono::{Local, NaiveDate};

/// What a search asks for. Every part must hold for the same message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Words that must each match the sender or the subject.
    pub words: Vec<String>,
    /// Words that must each match the sender (`from:`).
    pub from: Vec<String>,
    /// Words that must each match the subject (`subject:`).
    pub subject: Vec<String>,
    /// Label names, decoded, each of which the message must carry (`label:`).
    pub labels: Vec<String>,
    /// `Some(true)` for `is:unread`, `Some(false)` for `is:read`.
    pub unread: Option<bool>,
    /// `Some(true)` for `has:attachment`.
    pub attachment: Option<bool>,
    /// Received at or after this Unix time, the start of a local day (`after:`).
    pub after: Option<i64>,
    /// Received before this Unix time, the start of a local day (`before:`).
    pub before: Option<i64>,
}

impl Query {
    /// Whether the query asks for nothing, so no message can be searched for it.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
            && self.from.is_empty()
            && self.subject.is_empty()
            && self.labels.is_empty()
            && self.unread.is_none()
            && self.attachment.is_none()
            && self.after.is_none()
            && self.before.is_none()
    }
}

/// A parsed search box: the query, and anything that was ignored, for the user to see.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub query: Query,
    pub warnings: Vec<String>,
}

/// Parses the text of the search box.
pub fn parse(text: &str) -> Parsed {
    let mut out = Parsed::default();
    for token in tokens(text) {
        match split_operator(&token) {
            Some((key, value)) => apply(&mut out, key, value),
            None => push_words(&mut out.query.words, &token),
        }
    }
    out
}

/// Splits on whitespace, except inside double quotes, which stay in the token.
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// `key:value` for a known key with a value. Anything else is plain text.
fn split_operator(token: &str) -> Option<(&str, &str)> {
    let (key, value) = token.split_once(':')?;
    let known = ["from", "subject", "label", "is", "has", "before", "after"];
    let key = key.to_ascii_lowercase();
    if !known.contains(&key.as_str()) || value.is_empty() {
        return None;
    }
    // The key is matched without case, so it's returned as its lower-case form.
    Some((known.iter().find(|k| **k == key)?, value))
}

fn apply(out: &mut Parsed, key: &str, value: &str) {
    let unquoted = value.replace('"', "");
    match key {
        "from" => push_words(&mut out.query.from, &unquoted),
        "subject" => push_words(&mut out.query.subject, &unquoted),
        "label" => {
            let name = unquoted.trim();
            if name.is_empty() {
                out.warnings.push("label: needs a name".into());
            } else {
                out.query.labels.push(name.to_string());
            }
        }
        "is" => match unquoted.to_ascii_lowercase().as_str() {
            "unread" => out.query.unread = Some(true),
            "read" => out.query.unread = Some(false),
            _ => out.warnings.push(format!(
                "is:{unquoted} is not known; use is:read or is:unread"
            )),
        },
        "has" => match unquoted.to_ascii_lowercase().as_str() {
            "attachment" => out.query.attachment = Some(true),
            _ => out
                .warnings
                .push(format!("has:{unquoted} is not known; use has:attachment")),
        },
        "after" => match parse_day(&unquoted) {
            Some(t) => out.query.after = Some(t),
            None => out.warnings.push(format!(
                "after:{unquoted} is not a date; use after:2026-01-31"
            )),
        },
        "before" => match parse_day(&unquoted) {
            Some(t) => out.query.before = Some(t),
            None => out.warnings.push(format!(
                "before:{unquoted} is not a date; use before:2026-01-31"
            )),
        },
        _ => {}
    }
}

/// Adds each word of `text`, without quote characters, to `into`.
fn push_words(into: &mut Vec<String>, text: &str) {
    for word in text.replace('"', "").split_whitespace() {
        into.push(word.to_string());
    }
}

/// The start of a local day, as Unix time, for `YYYY-MM-DD` or `YYYY/MM/DD`.
fn parse_day(text: &str) -> Option<i64> {
    let day = NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(text, "%Y/%m/%d"))
        .ok()?;
    let midnight = day.and_hms_opt(0, 0, 0)?;
    Some(midnight.and_local_timezone(Local).earliest()?.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> i64 {
        let d = NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap();
        d.and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .earliest()
            .unwrap()
            .timestamp()
    }

    #[test]
    fn plain_words_are_words() {
        let p = parse("invoice  march");
        assert_eq!(p.query.words, vec!["invoice", "march"]);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn operators_fill_their_fields() {
        let p = parse(
            r#"from:alice subject:"tax return" label:"Work stuff" is:unread has:attachment after:2026-01-01 before:2026/02/01"#,
        );
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert_eq!(p.query.from, vec!["alice"]);
        assert_eq!(p.query.subject, vec!["tax", "return"]);
        assert_eq!(p.query.labels, vec!["Work stuff"]);
        assert_eq!(p.query.unread, Some(true));
        assert_eq!(p.query.attachment, Some(true));
        assert_eq!(p.query.after, Some(day("2026-01-01")));
        assert_eq!(p.query.before, Some(day("2026-02-01")));
        assert!(p.query.words.is_empty());
    }

    #[test]
    fn is_read_means_not_unread() {
        assert_eq!(parse("is:read").query.unread, Some(false));
    }

    #[test]
    fn unknown_names_are_plain_words() {
        let p = parse("foo:bar from:");
        assert_eq!(p.query.words, vec!["foo:bar", "from:"]);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn bad_values_are_ignored_with_a_warning() {
        let p = parse("is:flagged has:pdf before:notadate after:31-12-2026");
        assert_eq!(p.warnings.len(), 4, "{:?}", p.warnings);
        assert_eq!(p.query, Query::default());
    }

    #[test]
    fn quoted_plain_text_is_read_as_words_for_now() {
        assert_eq!(parse("\"big deal\"").query.words, vec!["big", "deal"]);
    }

    #[test]
    fn an_empty_box_asks_for_nothing() {
        assert!(parse("   ").query.is_empty());
    }
}
