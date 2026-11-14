//! A small, zero-copy parser for the IMAP FETCH responses this prototype needs.
//!
//! It handles atoms, quoted strings (with `\` escapes), `NIL`, numbers,
//! parenthesized lists and `{n}` literals. That is enough for the `X-GM-*`
//! attributes and `BODY[]` payloads. It is not a general IMAP parser.

use std::borrow::Cow;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value<'a> {
    Nil,
    Number(u64),
    Atom(&'a str),
    /// Quoted string with escapes resolved.
    Text(Cow<'a, str>),
    /// Literal payload, borrowed as raw bytes (message bodies may be binary).
    Bytes(&'a [u8]),
    List(Vec<Value<'a>>),
}

impl<'a> Value<'a> {
    /// Returns the string content of an atom or quoted string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Atom(s) => Some(s),
            Value::Text(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    UnexpectedEof,
    Unexpected { pos: usize, found: u8 },
    BadLiteral(usize),
    BadAtom(usize),
    BadFetch(&'static str),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnexpectedEof => write!(f, "unexpected end of response"),
            ParseError::Unexpected { pos, found } => {
                write!(f, "unexpected byte 0x{found:02x} at offset {pos}")
            }
            ParseError::BadLiteral(pos) => write!(f, "invalid literal at offset {pos}"),
            ParseError::BadAtom(pos) => write!(f, "invalid atom at offset {pos}"),
            ParseError::BadFetch(why) => write!(f, "malformed FETCH response: {why}"),
        }
    }
}

impl std::error::Error for ParseError {}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    fn skip_spaces(&mut self) {
        while self.peek() == Some(b' ') {
            self.pos += 1;
        }
    }

    fn expect(&mut self, want: u8) -> Result<(), ParseError> {
        match self.peek() {
            Some(c) if c == want => {
                self.pos += 1;
                Ok(())
            }
            Some(found) => Err(ParseError::Unexpected {
                pos: self.pos,
                found,
            }),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    fn value(&mut self) -> Result<Value<'a>, ParseError> {
        self.skip_spaces();
        match self.peek() {
            None => Err(ParseError::UnexpectedEof),
            Some(b'(') => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_spaces();
                    match self.peek() {
                        None => return Err(ParseError::UnexpectedEof),
                        Some(b')') => {
                            self.pos += 1;
                            break;
                        }
                        Some(_) => items.push(self.value()?),
                    }
                }
                Ok(Value::List(items))
            }
            Some(b'"') => self.quoted(),
            Some(b'{') => self.literal(),
            Some(_) => self.atom(),
        }
    }

    fn quoted(&mut self) -> Result<Value<'a>, ParseError> {
        self.expect(b'"')?;
        let buf = self.buf;
        let start = self.pos;
        let mut escaped = false;
        loop {
            match self.peek() {
                None => return Err(ParseError::UnexpectedEof),
                Some(b'\\') => {
                    escaped = true;
                    self.pos += 2;
                }
                Some(b'"') => break,
                Some(_) => self.pos += 1,
            }
        }
        let raw = &buf[start..self.pos];
        self.pos += 1; // closing quote

        let text = if escaped {
            let mut bytes = Vec::with_capacity(raw.len());
            let mut it = raw.iter().copied();
            while let Some(b) = it.next() {
                if b == b'\\' {
                    if let Some(next) = it.next() {
                        bytes.push(next);
                    }
                } else {
                    bytes.push(b);
                }
            }
            Cow::Owned(String::from_utf8_lossy(&bytes).into_owned())
        } else {
            String::from_utf8_lossy(raw)
        };
        Ok(Value::Text(text))
    }

    fn literal(&mut self) -> Result<Value<'a>, ParseError> {
        let at = self.pos;
        self.expect(b'{')?;
        let mut n: usize = 0;
        let mut digits = 0;
        while let Some(c @ b'0'..=b'9') = self.peek() {
            n = n
                .checked_mul(10)
                .and_then(|n| n.checked_add((c - b'0') as usize))
                .ok_or(ParseError::BadLiteral(at))?;
            digits += 1;
            self.pos += 1;
        }
        if digits == 0 {
            return Err(ParseError::BadLiteral(at));
        }
        self.expect(b'}')?;
        self.expect(b'\r')?;
        self.expect(b'\n')?;
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or(ParseError::BadLiteral(at))?;
        let buf = self.buf;
        let data = &buf[self.pos..end];
        self.pos = end;
        Ok(Value::Bytes(data))
    }

    fn atom(&mut self) -> Result<Value<'a>, ParseError> {
        let buf = self.buf;
        let start = self.pos;
        while let Some(c) = self.peek() {
            if matches!(c, b' ' | b'(' | b')' | b'"' | b'{' | b'\r' | b'\n') {
                break;
            }
            self.pos += 1;
        }
        if start == self.pos {
            return Err(ParseError::Unexpected {
                pos: start,
                found: self.peek().unwrap_or(0),
            });
        }
        let s =
            std::str::from_utf8(&buf[start..self.pos]).map_err(|_| ParseError::BadAtom(start))?;
        if s.eq_ignore_ascii_case("NIL") {
            Ok(Value::Nil)
        } else if let Ok(n) = s.parse::<u64>() {
            Ok(Value::Number(n))
        } else {
            Ok(Value::Atom(s))
        }
    }
}

/// One untagged `* <seq> FETCH (...)` response, as name/value attribute pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResponse<'a> {
    pub seq: u32,
    pub attrs: Vec<(&'a str, Value<'a>)>,
}

/// Parses a single untagged FETCH response line (including any literals).
pub fn parse_fetch(buf: &[u8]) -> Result<FetchResponse<'_>, ParseError> {
    let mut c = Cursor { buf, pos: 0 };
    c.expect(b'*')?;
    c.skip_spaces();
    let seq = match c.value()? {
        Value::Number(n) => {
            u32::try_from(n).map_err(|_| ParseError::BadFetch("seq out of range"))?
        }
        _ => return Err(ParseError::BadFetch("missing sequence number")),
    };
    match c.value()? {
        Value::Atom(s) if s.eq_ignore_ascii_case("FETCH") => {}
        _ => return Err(ParseError::BadFetch("not a FETCH response")),
    }
    c.skip_spaces();
    c.expect(b'(')?;

    let mut attrs = Vec::new();
    loop {
        c.skip_spaces();
        match c.peek() {
            None => return Err(ParseError::UnexpectedEof),
            Some(b')') => break,
            Some(_) => {}
        }
        let key = match c.value()? {
            Value::Atom(k) => k,
            _ => return Err(ParseError::BadFetch("attribute name is not an atom")),
        };
        let val = c.value()?;
        attrs.push((key, val));
    }
    Ok(FetchResponse { seq, attrs })
}

/// The Gmail-specific view of a FETCH response.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GmailFetch<'a> {
    pub seq: u32,
    pub uid: Option<u32>,
    /// `X-GM-MSGID`: stable per-message id across all labels/folders.
    pub msgid: Option<u64>,
    /// `X-GM-THRID`: Gmail conversation/thread id.
    pub thrid: Option<u64>,
    /// `X-GM-LABELS`: raw label names as sent, including system labels such as `\Inbox`.
    pub labels: Vec<String>,
    /// `BODY[]` / `BODY.PEEK[]` payload, if requested.
    pub body: Option<&'a [u8]>,
    /// `BODY[HEADER]` payload (message headers only), if requested.
    pub header: Option<&'a [u8]>,
    /// `FLAGS`, if requested. `\Seen` means read.
    pub flags: Option<Vec<String>>,
    /// `MODSEQ` (CONDSTORE change counter), if the server sent one.
    pub modseq: Option<u64>,
    /// `BODYSTRUCTURE`: the message's part tree, if requested.
    pub structure: Option<Value<'a>>,
    /// `INTERNALDATE`: when the server received the message, as a Unix time, if requested.
    pub internal_date: Option<i64>,
    /// `RFC822.SIZE`: the message's size in bytes, if requested.
    pub size: Option<u64>,
}

impl GmailFetch<'_> {
    /// Whether the message has an attachment, from its structure, if that was requested.
    pub fn has_attachment(&self) -> Option<bool> {
        self.structure.as_ref().map(structure_has_attachment)
    }

    /// Whether the message has been read (`\Seen`), if the flags were requested.
    pub fn seen(&self) -> Option<bool> {
        self.flags
            .as_ref()
            .map(|f| f.iter().any(|flag| flag.eq_ignore_ascii_case("\\Seen")))
    }
}

/// Parses a FETCH response and extracts the Gmail attributes.
pub fn parse_gmail_fetch(buf: &[u8]) -> Result<GmailFetch<'_>, ParseError> {
    let resp = parse_fetch(buf)?;
    let mut out = GmailFetch {
        seq: resp.seq,
        ..Default::default()
    };
    for (name, value) in resp.attrs {
        match name.to_ascii_uppercase().as_str() {
            "UID" => out.uid = number_as(&value).and_then(|n| u32::try_from(n).ok()),
            "X-GM-MSGID" => out.msgid = number_as(&value),
            "X-GM-THRID" => out.thrid = number_as(&value),
            "X-GM-LABELS" => out.labels = labels_from(&value),
            "FLAGS" => out.flags = Some(labels_from(&value)),
            "MODSEQ" => out.modseq = modseq_from(&value),
            "BODYSTRUCTURE" => out.structure = Some(value),
            "INTERNALDATE" => out.internal_date = internal_date_from(&value),
            "RFC822.SIZE" => out.size = number_as(&value),
            "BODY[]" => {
                if let Value::Bytes(b) = value {
                    out.body = Some(b);
                }
            }
            "BODY[HEADER]" => {
                if let Value::Bytes(b) = value {
                    out.header = Some(b);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// `INTERNALDATE`, such as `10-Nov-2026 09:30:00 +0000`, as a Unix time. The day may be
/// padded with a space. Anything else gives `None`.
fn internal_date_from(v: &Value<'_>) -> Option<i64> {
    let text = v.as_str()?.trim();
    chrono::DateTime::parse_from_str(text, "%d-%b-%Y %H:%M:%S %z")
        .ok()
        .map(|d| d.timestamp())
}

/// Whether a `BODYSTRUCTURE` tree has an attachment: a single part (a leaf) whose
/// disposition is `ATTACHMENT`. Multipart containers are searched through.
pub fn structure_has_attachment(v: &Value<'_>) -> bool {
    let Value::List(items) = v else {
        return false;
    };
    match items.first() {
        // A multipart container: its first item is the first child, a list.
        Some(Value::List(_)) => items.iter().any(structure_has_attachment),
        // A single part. The disposition is after the type, subtype, parameters, id,
        // description, encoding, size, and (for text) line count, md5 included.
        _ => items.iter().skip(7).any(|item| match item {
            Value::List(fields) => fields
                .first()
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("ATTACHMENT")),
            _ => false,
        }),
    }
}

/// `MODSEQ (n)`: the number is inside a one-item list.
fn modseq_from(v: &Value<'_>) -> Option<u64> {
    match v {
        Value::List(items) => items.first().and_then(number_as),
        other => number_as(other),
    }
}

fn number_as(v: &Value<'_>) -> Option<u64> {
    match v {
        Value::Number(n) => Some(*n),
        _ => None,
    }
}

/// Label names from an `X-GM-LABELS` list. A name can arrive as a quoted
/// string, a bare atom, a literal, or, if it's all digits, a number. Anything
/// else is skipped, so one odd label doesn't fail the whole message.
fn labels_from(v: &Value<'_>) -> Vec<String> {
    let Value::List(items) = v else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            Value::Number(n) => Some(n.to_string()),
            Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            other => other.as_str().map(str::to_owned),
        })
        .collect()
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn numeric_and_literal_label_names_are_kept() {
        let raw = b"* 1 FETCH (UID 5 X-GM-LABELS (\"\\\\Inbox\" 2024 Work) X-GM-MSGID 9)\r\n";
        let f = parse_gmail_fetch(raw).unwrap();
        assert_eq!(f.labels, vec!["\\Inbox", "2024", "Work"]);
    }

    #[test]
    fn literal_label_name_is_kept() {
        let raw = b"* 1 FETCH (UID 5 X-GM-LABELS ({4}\r\nWork) X-GM-MSGID 9)\r\n";
        let f = parse_gmail_fetch(raw).unwrap();
        assert_eq!(f.labels, vec!["Work"]);
    }
}

#[cfg(test)]
mod flags_tests {
    use super::*;

    fn structure_of(line: &str) -> bool {
        let g = parse_gmail_fetch(line.as_bytes()).unwrap();
        g.has_attachment().unwrap()
    }

    #[test]
    fn a_mixed_message_with_a_pdf_has_an_attachment() {
        assert!(structure_of(
            "* 1 FETCH (UID 7 BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 20 1 NIL NIL NIL NIL)(\"APPLICATION\" \"PDF\" (\"NAME\" \"a.pdf\") NIL NIL \"BASE64\" 100 NIL (\"ATTACHMENT\" (\"FILENAME\" \"a.pdf\")) NIL NIL) \"MIXED\" (\"BOUNDARY\" \"b\") NIL NIL NIL))\r\n"
        ));
    }

    #[test]
    fn inline_images_and_plain_text_have_no_attachment() {
        assert!(!structure_of(
            "* 1 FETCH (UID 8 BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 20 1 NIL NIL NIL NIL)(\"IMAGE\" \"PNG\" (\"NAME\" \"x.png\") \"<id>\" NIL \"BASE64\" 100 NIL (\"INLINE\" (\"FILENAME\" \"x.png\")) NIL NIL) \"RELATED\" (\"BOUNDARY\" \"b\") NIL NIL NIL))\r\n"
        ));
        assert!(!structure_of(
            "* 1 FETCH (UID 9 BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 20 1 NIL NIL NIL NIL))\r\n"
        ));
    }

    #[test]
    fn internal_date_is_read_as_a_unix_time() {
        let g =
            parse_gmail_fetch(b"* 1 FETCH (UID 3 INTERNALDATE \"10-Nov-2026 09:30:00 +0000\")\r\n")
                .unwrap();
        assert_eq!(g.internal_date, Some(1_794_303_000));
    }

    #[test]
    fn a_single_digit_day_and_other_zones_are_read() {
        let g =
            parse_gmail_fetch(b"* 1 FETCH (UID 3 INTERNALDATE \"1-Nov-2026 10:30:00 +0100\")\r\n")
                .unwrap();
        // 09:30 UTC on 1 November 2026.
        assert_eq!(g.internal_date, Some(1_793_525_400));
    }

    #[test]
    fn a_malformed_internal_date_is_left_out() {
        let g = parse_gmail_fetch(b"* 1 FETCH (UID 3 INTERNALDATE \"yesterday\")\r\n").unwrap();
        assert_eq!(g.internal_date, None);
    }

    #[test]
    fn flags_and_modseq_are_read() {
        let raw = b"* 1 FETCH (UID 5 FLAGS (\\Seen \\Flagged) MODSEQ (12345) X-GM-MSGID 9)\r\n";
        let f = parse_gmail_fetch(raw).unwrap();
        assert_eq!(f.seen(), Some(true));
        assert_eq!(f.modseq, Some(12345));
    }

    #[test]
    fn unread_and_absent_flags() {
        let unread = b"* 1 FETCH (UID 5 FLAGS ())\r\n";
        assert_eq!(parse_gmail_fetch(unread).unwrap().seen(), Some(false));
        let absent = b"* 1 FETCH (UID 5)\r\n";
        let f = parse_gmail_fetch(absent).unwrap();
        assert_eq!(f.seen(), None);
        assert_eq!(f.modseq, None);
    }
}
