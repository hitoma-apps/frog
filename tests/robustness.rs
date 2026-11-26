//! Feeds the parsers hostile and malformed input. The server and every message are untrusted, so
//! none of these may panic, whatever they're given, and file names made from mail must always be
//! safe to write.

use frog::{
    attachments, export, impersonation, importance, linkcheck, original, otp, render_plaintext,
    response, search, text, utf7,
};

/// A small deterministic generator, so a failure can be reproduced from the seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Pieces that mean something to the parsers: encodings, links, markup, device names, path
/// separators, bidi overrides, IMAP literals, and header lines.
const PIECES: &[&str] = &[
    "&",
    "&-",
    "&AOk-",
    "+",
    "+AOk",
    "=?utf-8?q?",
    "=?",
    "?=",
    "\u{202e}",
    "\u{200b}",
    "\u{feff}",
    "https://",
    "http://",
    "https://a@b",
    "<a href=\"",
    "<",
    ">",
    "CON",
    "NUL",
    "COM1",
    "LPT9",
    "..",
    "/",
    "\\",
    ":",
    "\r\n",
    "\r\n\r\n",
    "\n",
    "\0",
    "{99999999999}",
    "{0}",
    "(",
    ")",
    "\"",
    "'",
    "@",
    "?",
    "#",
    "[",
    "]",
    "Content-Type: text/html\r\n",
    "Subject: CODE 123456 is your code\r\n",
    "your code is 12 34 56",
    "123456",
    "9999999999999999999999",
    "\u{1F600}",
    "é",
    "中",
    " ",
    "*",
    "%",
    "\x1b[31m",
    "mailto:x@y.z?subject=a",
    "Importance: high\r\n",
    "X-Priority: 1\r\n",
    "Content-Disposition: attachment; filename=\"CON.exe\"\r\n",
];

/// A random string built from the pieces above and random characters.
fn sample(rng: &mut Rng) -> String {
    let mut s = String::new();
    for _ in 0..rng.below(40) {
        if rng.below(3) == 0 {
            let c = char::from_u32((rng.next() % 0x10000) as u32).unwrap_or('?');
            s.push(c);
        } else {
            s.push_str(PIECES[rng.below(PIECES.len())]);
        }
    }
    s
}

/// Random bytes, some of them not UTF-8.
fn random_bytes(rng: &mut Rng) -> Vec<u8> {
    (0..rng.below(300)).map(|_| rng.next() as u8).collect()
}

const ROUNDS: usize = 3000;

#[test]
fn text_parsers_survive_hostile_input() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..ROUNDS {
        let s = sample(&mut rng);
        let _ = utf7::decode(&s);
        let _ = utf7::encode(&s);
        let _ = linkcheck::analyze(&s);
        let _ = linkcheck::is_suspicious(&s);
        let _ = otp::find(&s);
        let _ = otp::is_code_subject(&s);
        let _ = text::strip_hidden(&s);
        let _ = attachments::is_program(&s);
        let _ = search::parse(&s);
        let _ = impersonation::sender_warnings(&s, &s);
    }
}

#[test]
fn byte_parsers_survive_hostile_input() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for i in 0..ROUNDS {
        let raw = if i % 2 == 0 {
            sample(&mut rng).into_bytes()
        } else {
            random_bytes(&mut rng)
        };
        let _ = render_plaintext(&raw);
        let _ = attachments::list(&raw);
        for index in 0..3 {
            let _ = attachments::contents(&raw, index);
        }
        let _ = response::parse_gmail_fetch(&raw);
        let _ = response::parse_fetch(&raw);
        let _ = original::summary(&raw);
        let _ = importance::sender_importance(&raw);
    }
}

/// Windows refuses these names, with or without an extension, and writing to them reaches a
/// device rather than a file.
fn is_device_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

fn assert_safe_name(name: &str, source: &str) {
    assert!(!name.is_empty(), "empty name from {source:?}");
    assert!(
        !name.contains(['/', '\\']),
        "separator in {name:?} from {source:?}"
    );
    assert!(
        !is_device_name(name),
        "device name {name:?} from {source:?}"
    );
    assert!(
        !name.ends_with(['.', ' ']),
        "trailing dot or space in {name:?}"
    );
    assert!(name.len() <= 200, "name too long: {} bytes", name.len());
}

#[test]
fn file_names_from_mail_are_safe_to_write() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for _ in 0..ROUNDS {
        let s = sample(&mut rng);
        assert_safe_name(&attachments::safe_name(&s), &s);
        assert_safe_name(&export::safe_file_name(&s), &s);
    }
    for fixed in ["CON", "nul.exe", "COM1.pdf", "lpt9", "..", "a/b\\c.txt"] {
        assert_safe_name(&attachments::safe_name(fixed), fixed);
        assert_safe_name(&export::safe_file_name(fixed), fixed);
    }
}

#[test]
fn only_web_addresses_are_opened() {
    assert!(linkcheck::is_web_address("https://example.com/a?b=c"));
    assert!(linkcheck::is_web_address("http://example.com"));
    for target in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "smb://server/share",
        "https://",
        "https://example.com/\nx",
        "https://example.com/ x",
        "https://example.com/\u{1b}[2J",
        "",
    ] {
        assert!(
            !linkcheck::is_web_address(target),
            "{target:?} must not open"
        );
    }
}
