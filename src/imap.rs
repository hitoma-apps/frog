//! A small synchronous IMAP client: just enough for Gmail login, SELECT,
//! UID SEARCH, UID FETCH and UID STORE.
//!
//! The client is generic over any `Read + Write` stream. `connect_tls` builds
//! a rustls stream for real servers. Tests use a plain TCP stream.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use crate::labels::{LabelAction, LabelRef, uid_store_labels};

#[derive(Debug)]
pub enum ImapError {
    Io(io::Error),
    Tls(String),
    /// Tagged `NO` response.
    No(String),
    /// Tagged `BAD` response.
    Bad(String),
    /// Anything unexpected in the byte stream.
    Protocol(String),
}

impl fmt::Display for ImapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImapError::Io(e) => write!(f, "I/O error: {e}"),
            ImapError::Tls(e) => write!(f, "TLS error: {e}"),
            ImapError::No(e) => write!(f, "server refused: {e}"),
            ImapError::Bad(e) => write!(f, "server rejected command: {e}"),
            ImapError::Protocol(e) => write!(f, "protocol error: {e}"),
        }
    }
}

impl std::error::Error for ImapError {}

impl From<io::Error> for ImapError {
    fn from(e: io::Error) -> Self {
        ImapError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, ImapError>;

/// Opens a TLS connection to `host:port` and reads the server greeting.
pub fn connect_tls(
    host: &str,
    port: u16,
) -> Result<ImapConn<StreamOwned<ClientConnection, TcpStream>>> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_string()).map_err(|e| ImapError::Tls(e.to_string()))?;
    let conn =
        ClientConnection::new(Arc::new(config), name).map_err(|e| ImapError::Tls(e.to_string()))?;
    let tcp = TcpStream::connect((host, port))?;
    ImapConn::new(StreamOwned::new(conn, tcp))
}

/// The result of one tagged command: its untagged responses (each one
/// complete, including any literal payloads) and the tagged OK text.
#[derive(Debug, Default)]
pub struct Response {
    pub untagged: Vec<Vec<u8>>,
    pub text: String,
}

pub struct ImapConn<S: Read + Write> {
    reader: BufReader<S>,
    next_tag: u32,
}

impl<S: Read + Write> ImapConn<S> {
    /// Wraps a stream and reads the greeting.
    pub fn new(stream: S) -> Result<Self> {
        let mut conn = ImapConn {
            reader: BufReader::new(stream),
            next_tag: 1,
        };
        let greeting = conn.read_line()?;
        if !(greeting.starts_with(b"* OK") || greeting.starts_with(b"* PREAUTH")) {
            return Err(ImapError::Protocol(format!(
                "unexpected greeting: {}",
                String::from_utf8_lossy(&greeting).trim_end()
            )));
        }
        Ok(conn)
    }

    fn read_line(&mut self) -> Result<Vec<u8>> {
        let mut line = Vec::new();
        if self.reader.read_until(b'\n', &mut line)? == 0 {
            return Err(ImapError::Protocol("connection closed by server".into()));
        }
        Ok(line)
    }

    /// Sends `cmd` with a fresh tag and collects responses up to the tagged status.
    pub fn command(&mut self, cmd: &str) -> Result<Response> {
        let tag = format!("a{:04}", self.next_tag);
        self.next_tag += 1;
        {
            let stream = self.reader.get_mut();
            stream.write_all(format!("{tag} {cmd}\r\n").as_bytes())?;
            stream.flush()?;
        }

        let mut untagged = Vec::new();
        loop {
            let mut item = self.read_line()?;
            // A line ending in `{n}` announces a literal of n bytes. The rest of
            // the logical response follows the literal on the next line.
            while let Some(n) = literal_len(&item) {
                let start = item.len();
                item.resize(start + n, 0);
                self.reader.read_exact(&mut item[start..])?;
                let rest = self.read_line()?;
                item.extend_from_slice(&rest);
            }

            if let Some(status) = item
                .strip_prefix(tag.as_bytes())
                .and_then(|r| r.strip_prefix(b" "))
            {
                let text = String::from_utf8_lossy(status).trim_end().to_string();
                let word = text.split_whitespace().next().unwrap_or("");
                return match word {
                    "OK" => Ok(Response { untagged, text }),
                    "NO" => Err(ImapError::No(text)),
                    "BAD" => Err(ImapError::Bad(text)),
                    _ => Err(ImapError::Protocol(format!("bad status line: {text}"))),
                };
            }
            if item.starts_with(b"* ") {
                untagged.push(item);
            }
            // Continuation requests ("+ ...") and anything else are ignored.
        }
    }

    pub fn login(&mut self, user: &str, password: &str) -> Result<()> {
        self.command(&format!("LOGIN {} {}", quote(user), quote(password)))?;
        Ok(())
    }

    pub fn capabilities(&mut self) -> Result<Vec<String>> {
        let resp = self.command("CAPABILITY")?;
        let mut caps = Vec::new();
        for line in &resp.untagged {
            let text = String::from_utf8_lossy(line);
            if let Some(rest) = text.strip_prefix("* CAPABILITY ") {
                caps.extend(rest.split_whitespace().map(str::to_owned));
            }
        }
        Ok(caps)
    }

    /// Selects a mailbox read-write and returns the number of messages in it.
    pub fn select(&mut self, mailbox: &str) -> Result<u32> {
        let resp = self.command(&format!("SELECT {}", quote(mailbox)))?;
        for line in &resp.untagged {
            let text = String::from_utf8_lossy(line);
            let mut parts = text.split_whitespace();
            if let (Some("*"), Some(n), Some("EXISTS")) = (parts.next(), parts.next(), parts.next())
            {
                return n
                    .parse()
                    .map_err(|_| ImapError::Protocol(format!("bad EXISTS count: {n}")));
            }
        }
        Err(ImapError::Protocol(
            "SELECT returned no EXISTS count".into(),
        ))
    }

    /// Every UID in the selected mailbox.
    pub fn uid_search_all(&mut self) -> Result<Vec<u32>> {
        let resp = self.command("UID SEARCH ALL")?;
        let mut uids = Vec::new();
        for line in &resp.untagged {
            let text = String::from_utf8_lossy(line);
            if let Some(rest) = text.strip_prefix("* SEARCH") {
                for tok in rest.split_whitespace() {
                    uids.push(
                        tok.parse()
                            .map_err(|_| ImapError::Protocol(format!("bad UID: {tok}")))?,
                    );
                }
            }
        }
        Ok(uids)
    }

    /// `UID FETCH <uids> (<items>)`. Returns one raw response per message.
    pub fn uid_fetch(&mut self, uids: &[u32], items: &str) -> Result<Vec<Vec<u8>>> {
        let mut set = String::new();
        crate::labels::push_uid_set(&mut set, uids);
        let resp = self.command(&format!("UID FETCH {set} ({items})"))?;
        Ok(resp
            .untagged
            .into_iter()
            .filter(|line| line.windows(7).any(|w| w == b" FETCH "))
            .collect())
    }

    /// Applies a label change with `UID STORE`. Returns the command sent.
    pub fn uid_store(
        &mut self,
        uids: &[u32],
        action: LabelAction,
        labels: &[LabelRef<'_>],
    ) -> Result<String> {
        let cmd = uid_store_labels(uids, action, labels)
            .ok_or_else(|| ImapError::Protocol("nothing to store".into()))?;
        self.command(&cmd)?;
        Ok(cmd)
    }

    pub fn logout(&mut self) -> Result<()> {
        self.command("LOGOUT")?;
        Ok(())
    }
}

/// If `line` ends with a literal marker `{n}\r\n`, returns `n`.
fn literal_len(line: &[u8]) -> Option<usize> {
    let body = line.strip_suffix(b"\r\n")?;
    let body = body.strip_suffix(b"}")?;
    let open = body.iter().rposition(|&b| b == b'{')?;
    let digits = &body[open + 1..];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Quotes a string for IMAP, escaping `\` and `"`.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_marker_detection() {
        assert_eq!(literal_len(b"* 1 FETCH (BODY[] {42}\r\n"), Some(42));
        assert_eq!(literal_len(b"* 1 FETCH (UID 1)\r\n"), None);
        assert_eq!(literal_len(b"* 1 FETCH (X {}\r\n"), None);
        assert_eq!(literal_len(b"{7}"), None); // no CRLF, not a complete line
    }

    #[test]
    fn quoting_escapes_specials() {
        assert_eq!(quote(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
