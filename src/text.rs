//! Cleaning text that comes from email before it's shown or used as a file name.
//!
//! Some characters change how text is displayed without being visible: bidi overrides can make
//! "report.doc" look like "report" followed by a different extension, and zero-width characters
//! can hide a name. Both are common in phishing, so they're removed from names and subjects.

/// Whether a character changes text direction or is invisible. Zero-width joiners and
/// non-joiners are kept, since some scripts and emoji need them.
pub fn is_hidden_or_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}'   // LRE, RLE, PDF, LRO, RLO
            | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
            | '\u{200B}' | '\u{2060}' | '\u{FEFF}' // zero-width space, word joiner, BOM
            | '\u{200E}' | '\u{200F}' | '\u{061C}' // LRM, RLM, ALM
    ) || (c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
}

/// `text` without the characters `is_hidden_or_bidi` matches.
pub fn strip_hidden(text: &str) -> String {
    text.chars().filter(|&c| !is_hidden_or_bidi(c)).collect()
}

/// A file name that Windows accepts. A device name such as `CON`, `NUL` or `COM1` (with or without
/// an extension) gets a `_` in front, since writing to it reaches a device. Trailing dots and
/// spaces are removed, and the name is cut to `max_bytes`, on a character boundary. May return an
/// empty string, which the caller replaces.
pub fn portable_name(name: &str, max_bytes: usize) -> String {
    let trimmed = name.trim_start().trim_end_matches(['.', ' ']);
    let stem = trimmed
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    let numbered = stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && matches!(stem.as_bytes()[3], b'1'..=b'9');
    let reserved = numbered || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL");
    let mut out = if reserved {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    };
    if out.len() > max_bytes {
        let mut end = max_bytes;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        // Cutting can leave a dot or space at the end, which Windows drops.
        out = out.trim_end_matches(['.', ' ']).to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_bidi_overrides_and_zero_width_characters() {
        // "report" + RLO + "cod.exe" would display as "reportexe.doc".
        assert_eq!(strip_hidden("report\u{202E}cod.exe"), "reportcod.exe");
        assert_eq!(strip_hidden("a\u{200B}b\u{FEFF}c\u{2066}d\u{2069}"), "abcd");
    }

    #[test]
    fn keeps_joiners_and_ordinary_text() {
        assert_eq!(strip_hidden("café"), "café");
        assert_eq!(strip_hidden("👨\u{200D}👩"), "👨\u{200D}👩");
        assert_eq!(strip_hidden("line\tone"), "line\tone");
    }
}
