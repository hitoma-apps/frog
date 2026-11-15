//! Importance the sender set in a message's headers: `Importance:`, `X-Priority:` or
//! `X-MSMail-Priority:`. Gmail's own importance marker is the `\Important` label, which is not
//! read here.

/// Whether the sender asked for high or low importance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    High,
    Low,
}

/// The importance a sender set: the level, and the header that said so, as sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderImportance {
    pub level: Level,
    /// The header and its value, such as `X-Priority: 1 (Highest)`.
    pub header: String,
}

/// The importance a message's headers set, if it set one. `Importance` is read first, then
/// `X-Priority`, then `X-MSMail-Priority`. Normal importance counts as not set.
pub fn sender_importance(headers: &[u8]) -> Option<SenderImportance> {
    let text = String::from_utf8_lossy(headers);
    let mut found: [Option<String>; 3] = [None, None, None];
    for line in text.lines() {
        // Folded continuation lines belong to the header above, and these headers are short.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let slot = match name.trim().to_ascii_lowercase().as_str() {
            "importance" => 0,
            "x-priority" => 1,
            "x-msmail-priority" => 2,
            _ => continue,
        };
        if found[slot].is_none() {
            found[slot] = Some(value.trim().to_string());
        }
    }
    let names = ["Importance", "X-Priority", "X-MSMail-Priority"];
    for (slot, value) in found.iter().enumerate() {
        let Some(value) = value else { continue };
        if let Some(level) = level_of(slot, value) {
            return Some(SenderImportance {
                level,
                header: format!("{}: {value}", names[slot]),
            });
        }
    }
    None
}

/// The level a header value means. `Importance` and `X-MSMail-Priority` use words; `X-Priority`
/// uses a number from 1 (highest) to 5 (lowest).
fn level_of(slot: usize, value: &str) -> Option<Level> {
    let value = value.to_ascii_lowercase();
    match slot {
        0 | 2 => match value.split_whitespace().next()? {
            "high" => Some(Level::High),
            "low" => Some(Level::Low),
            _ => None,
        },
        _ => match value.chars().next()? {
            '1' | '2' => Some(Level::High),
            '4' | '5' => Some(Level::Low),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(headers: &str) -> Option<SenderImportance> {
        sender_importance(headers.as_bytes())
    }

    #[test]
    fn importance_high_and_low_are_read() {
        assert_eq!(
            find("Importance: High\r\nSubject: x\r\n").unwrap().level,
            Level::High
        );
        assert_eq!(find("Importance: low\r\n").unwrap().level, Level::Low);
    }

    #[test]
    fn x_priority_numbers_are_read() {
        let high = find("X-Priority: 1 (Highest)\r\n").unwrap();
        assert_eq!(high.level, Level::High);
        assert_eq!(high.header, "X-Priority: 1 (Highest)");
        assert_eq!(find("X-Priority: 2\r\n").unwrap().level, Level::High);
        assert_eq!(find("X-Priority: 5\r\n").unwrap().level, Level::Low);
    }

    #[test]
    fn msmail_priority_is_read() {
        assert_eq!(
            find("X-MSMail-Priority: High\r\n").unwrap().level,
            Level::High
        );
    }

    #[test]
    fn normal_or_missing_is_not_set() {
        assert!(find("X-Priority: 3\r\nImportance: normal\r\n").is_none());
        assert!(find("Subject: nothing special\r\n").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn importance_is_read_before_x_priority() {
        let both = find("X-Priority: 5\r\nImportance: high\r\n").unwrap();
        assert_eq!(both.header, "Importance: high");
    }
}
