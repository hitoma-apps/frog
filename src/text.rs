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
