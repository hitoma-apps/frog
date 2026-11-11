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
            Some(found) => Err(ParseError::Unexpected { pos: self.pos, found }),
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
        let s = std::str::from_utf8(&buf[start..self.pos]).map_err(|_| ParseError::BadAtom(start))?;
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
        Value::Number(n) => u32::try_from(n).map_err(|_| ParseError::BadFetch("seq out of range"))?,
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
            "X-GM-LABELS" => out.labels = labels_from(&value)?,
            "BODY[]" => {
                if let Value::Bytes(b) = value {
                    out.body = Some(b);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn number_as(v: &Value<'_>) -> Option<u64> {
    match v {
        Value::Number(n) => Some(*n),
        _ => None,
    }
}

fn labels_from(v: &Value<'_>) -> Result<Vec<String>, ParseError> {
    match v {
        Value::Nil => Ok(Vec::new()),
        Value::List(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or(ParseError::BadFetch("label is not a string"))
            })
            .collect(),
        _ => Err(ParseError::BadFetch("X-GM-LABELS is not a list")),
    }
}
