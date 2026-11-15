//! Modified UTF-7, the encoding IMAP uses for non-ASCII mailbox and label names (RFC 3501
//! section 5.1.3). Inside `&...-` the bytes are UTF-16BE in base64 with `,` in place of `/`,
//! and a literal `&` is written as `&-`.
//!
//! The app keeps label names decoded, as UTF-8. The server's form is produced only when a
//! name goes back to the server.

/// Decodes a modified UTF-7 name. Malformed runs are kept as they were written.
pub fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos + 1..];
        if let Some(after) = rest.strip_prefix('-') {
            out.push('&');
            rest = after;
            continue;
        }
        match rest.find('-') {
            Some(end) => {
                match decode_run(&rest[..end]) {
                    Some(text) => out.push_str(&text),
                    None => {
                        out.push('&');
                        out.push_str(&rest[..=end]);
                    }
                }
                rest = &rest[end + 1..];
            }
            None => {
                // An unterminated run is not valid modified UTF-7, so it stays as written.
                out.push('&');
                out.push_str(rest);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Encodes a name for the server. Printable ASCII stays as it is; other characters go into
/// `&...-` runs.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run: Vec<u16> = Vec::new();
    for c in s.chars() {
        if (' '..='~').contains(&c) {
            flush_run(&mut out, &mut run);
            if c == '&' {
                out.push_str("&-");
            } else {
                out.push(c);
            }
        } else {
            let mut buf = [0u16; 2];
            run.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush_run(&mut out, &mut run);
    out
}

fn decode_run(b64: &str) -> Option<String> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in b64.bytes() {
        acc = (acc << 6) | base64_value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // Leftover bits of six or more mean the run was cut short.
    if bits >= 6 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|p| u16::from_be_bytes([p[0], p[1]]))
        .collect();
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .ok()
}

fn flush_run(out: &mut String, run: &mut Vec<u16>) {
    if run.is_empty() {
        return;
    }
    out.push('&');
    let bytes: Vec<u8> = run.iter().flat_map(|u| u.to_be_bytes()).collect();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(base64_char((acc >> bits) & 0x3f));
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(base64_char((acc << (6 - bits)) & 0x3f));
    }
    out.push('-');
    run.clear();
}

fn base64_value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some(u32::from(c - b'A')),
        b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
        b'+' => Some(62),
        b',' => Some(63),
        _ => None,
    }
}

fn base64_char(v: u32) -> char {
    match v {
        0..=25 => char::from(b'A' + v as u8),
        26..=51 => char::from(b'a' + (v - 26) as u8),
        52..=61 => char::from(b'0' + (v - 52) as u8),
        62 => '+',
        _ => ',',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_rfc_example() {
        assert_eq!(
            decode("~peter/mail/&ZeVnLIqe-/&U,BTFw-"),
            "~peter/mail/日本語/台北"
        );
    }

    #[test]
    fn decodes_accented_letters_and_ampersands() {
        assert_eq!(decode("Caf&AOk-"), "Café");
        assert_eq!(decode("R&-D"), "R&D");
        assert_eq!(decode("plain"), "plain");
    }

    #[test]
    fn keeps_malformed_runs_as_written() {
        assert_eq!(decode("a&b"), "a&b");
        assert_eq!(decode("a&!!-b"), "a&!!-b");
    }

    #[test]
    fn encodes_what_decode_reads_back() {
        for name in ["Café", "日本語/台北", "R&D", "plain/Work", "emoji 🎉"] {
            assert_eq!(decode(&encode(name)), name, "{name}");
        }
        assert_eq!(encode("Café"), "Caf&AOk-");
        assert_eq!(encode("R&D"), "R&-D");
        assert_eq!(
            encode("~peter/mail/日本語/台北"),
            "~peter/mail/&ZeVnLIqe-/&U,BTFw-"
        );
    }
}
