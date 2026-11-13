//! A small synchronous IMAP client: just enough for Gmail login, SELECT,
//! UID SEARCH, UID FETCH and UID STORE.
//!
//! The client is generic over any `Read + Write` stream. `connect_tls` builds
//! a rustls stream for real servers. Tests use a plain TCP stream.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use crate::labels::{LabelAction, LabelRef, uid_store_labels};

#[derive(Debug)]
pub enum ImapError {
    Io(io::Error),
    /// The server closed the connection.
    Closed,
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
            ImapError::Closed => write!(f, "connection closed by server"),
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

/// Reads the number after `UIDVALIDITY ` in a response line, if present.
fn parse_uidvalidity(text: &str) -> Option<u32> {
    parse_bracketed_number(text, "UIDVALIDITY ")
}

/// Reads the number right after `key` (for example `HIGHESTMODSEQ 42`).
fn parse_bracketed_number<T: std::str::FromStr>(text: &str, key: &str) -> Option<T> {
    let rest = text.split(key).nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// What SELECT reports about a mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selected {
    pub exists: u32,
    pub uidvalidity: u32,
    /// `HIGHESTMODSEQ`, if the server supports CONDSTORE.
    pub highestmodseq: Option<u64>,
}

/// Opens a TLS connection to `host:port` and reads the server greeting.
/// How long to wait for the TCP connection to open.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long any single read or write may stall before the connection is treated as dead.
pub const IO_TIMEOUT: Duration = Duration::from_secs(60);

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
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| ImapError::Protocol(format!("no address for {host}")))?;
    let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
    tcp.set_write_timeout(Some(IO_TIMEOUT))?;
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
    /// Bytes of a line read so far. Kept across read timeouts, so a line split by one isn't lost.
    partial: Vec<u8>,
    /// The tag of the IDLE command, while idling.
    idling: Option<String>,
    /// When the current IDLE started. The server may drop IDLE after about 30 minutes.
    idle_started: Option<std::time::Instant>,
    /// Untagged lines that arrived while a command ended IDLE. See [`ImapConn::take_stashed`].
    stash: Vec<String>,
}

/// How long one IDLE may run before it's restarted (RFC 2177 suggests under 29 minutes).
pub const IDLE_RESTART: Duration = Duration::from_secs(25 * 60);

/// Sets the read timeout on the underlying socket. IDLE polling needs short
/// timeouts, and it has to restore the normal one afterwards.
pub trait SetReadTimeout {
    fn set_read_timeout_to(&self, t: Option<Duration>) -> io::Result<()>;
}

impl SetReadTimeout for TcpStream {
    fn set_read_timeout_to(&self, t: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(t)
    }
}

impl SetReadTimeout for StreamOwned<ClientConnection, TcpStream> {
    fn set_read_timeout_to(&self, t: Option<Duration>) -> io::Result<()> {
        self.sock.set_read_timeout(t)
    }
}

fn is_timeout(e: &ImapError) -> bool {
    matches!(e, ImapError::Io(io) if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

impl<S: Read + Write> ImapConn<S> {
    /// Wraps a stream and reads the greeting.
    pub fn new(stream: S) -> Result<Self> {
        let mut conn = ImapConn {
            reader: BufReader::new(stream),
            next_tag: 1,
            partial: Vec::new(),
            idling: None,
            idle_started: None,
            stash: Vec::new(),
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

    /// Reads one complete line. If a read times out partway through, the bytes
    /// read so far are kept, and the next call continues the same line.
    fn read_line(&mut self) -> Result<Vec<u8>> {
        loop {
            let n = self.reader.read_until(b'\n', &mut self.partial)?;
            if n == 0 {
                return Err(ImapError::Closed);
            }
            if self.partial.ends_with(b"\n") {
                return Ok(std::mem::take(&mut self.partial));
            }
        }
    }

    /// Sends `cmd` with a fresh tag and collects responses up to the tagged status.
    pub fn command(&mut self, cmd: &str) -> Result<Response> {
        if self.idling.is_some() {
            // Commands can't run inside IDLE. End it, and keep what the server sent.
            let lines = self.idle_done()?;
            self.stash.extend(lines);
        }
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

    /// Selects a mailbox read-write. Returns its message count and UIDVALIDITY.
    pub fn select(&mut self, mailbox: &str) -> Result<Selected> {
        let resp = self.command(&format!("SELECT {}", quote(mailbox)))?;
        let mut exists = None;
        for line in &resp.untagged {
            let text = String::from_utf8_lossy(line);
            let mut parts = text.split_whitespace();
            if let (Some("*"), Some(n), Some("EXISTS")) = (parts.next(), parts.next(), parts.next())
            {
                exists = n.parse().ok();
            }
        }
        // UIDVALIDITY arrives as an untagged `* OK [UIDVALIDITY n]`. Some servers
        // put it in the tagged status instead, so check both.
        let uidvalidity = resp
            .untagged
            .iter()
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .chain(std::iter::once(resp.text.clone()))
            .find_map(|text| parse_uidvalidity(&text));
        let highestmodseq = resp
            .untagged
            .iter()
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .chain(std::iter::once(resp.text.clone()))
            .find_map(|text| parse_bracketed_number(&text, "HIGHESTMODSEQ "));
        match (exists, uidvalidity) {
            (Some(exists), Some(uidvalidity)) => Ok(Selected {
                exists,
                uidvalidity,
                highestmodseq,
            }),
            _ => Err(ImapError::Protocol(
                "SELECT did not report EXISTS and UIDVALIDITY".into(),
            )),
        }
    }

    /// UIDs in the selected mailbox that are at least `start`, ascending.
    /// IMAP always returns the highest UID for `start:*` even when it is below
    /// `start`, so callers should filter.
    pub fn uid_search_from(&mut self, start: u32) -> Result<Vec<u32>> {
        let resp = self.command(&format!("UID SEARCH UID {start}:*"))?;
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
        uids.retain(|&u| u >= start);
        uids.sort_unstable();
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

    /// Messages whose flags, labels or other data changed after `modseq`
    /// (CONDSTORE). Returns one raw FETCH response per message.
    pub fn uid_fetch_changed_since(&mut self, items: &str, modseq: u64) -> Result<Vec<Vec<u8>>> {
        let resp = self.command(&format!("UID FETCH 1:* ({items}) (CHANGEDSINCE {modseq})"))?;
        Ok(resp
            .untagged
            .into_iter()
            .filter(|line| line.windows(7).any(|w| w == b" FETCH "))
            .collect())
    }

    /// Starts IDLE if it isn't running.
    pub fn idle_start(&mut self) -> Result<()> {
        if self.idling.is_some() {
            return Ok(());
        }
        let tag = format!("a{:04}", self.next_tag);
        self.next_tag += 1;
        {
            let stream = self.reader.get_mut();
            stream.write_all(format!("{tag} IDLE\r\n").as_bytes())?;
            stream.flush()?;
        }
        loop {
            let line = self.read_line()?;
            if line.starts_with(b"+") {
                break;
            }
            if let Some(status) = line.strip_prefix(tag.as_bytes()) {
                return Err(ImapError::No(
                    String::from_utf8_lossy(status).trim().to_string(),
                ));
            }
            // Untagged data before the continuation request is kept for the caller.
            if line.starts_with(b"* ") {
                self.stash
                    .push(String::from_utf8_lossy(&line).trim_end().to_string());
            }
        }
        self.idling = Some(tag);
        self.idle_started = Some(std::time::Instant::now());
        Ok(())
    }

    /// Ends IDLE with DONE, and returns the untagged lines the server sent before it finished.
    fn idle_done(&mut self) -> Result<Vec<String>> {
        let Some(tag) = self.idling.take() else {
            return Ok(Vec::new());
        };
        self.idle_started = None;
        self.reader.get_mut().write_all(b"DONE\r\n")?;
        self.reader.get_mut().flush()?;
        let mut lines = Vec::new();
        loop {
            let line = self.read_line()?;
            if let Some(status) = line
                .strip_prefix(tag.as_bytes())
                .and_then(|r| r.strip_prefix(b" "))
            {
                let text = String::from_utf8_lossy(status).trim_end().to_string();
                return if text.starts_with("OK") {
                    Ok(lines)
                } else {
                    Err(ImapError::Bad(text))
                };
            }
            if line.starts_with(b"* ") {
                lines.push(String::from_utf8_lossy(&line).trim_end().to_string());
            }
        }
    }

    /// Waits up to `wait` for untagged updates while idling. Starts IDLE if
    /// needed, and restarts it after [`IDLE_RESTART`]. Returns the untagged
    /// lines received, which include any stashed earlier.
    pub fn idle_poll(&mut self, wait: Duration) -> Result<Vec<String>>
    where
        S: SetReadTimeout,
    {
        if self
            .idle_started
            .is_some_and(|t| t.elapsed() >= IDLE_RESTART)
        {
            let lines = self.idle_done()?;
            self.stash.extend(lines);
        }
        self.idle_start()?;
        let mut lines = std::mem::take(&mut self.stash);
        self.reader.get_mut().set_read_timeout_to(Some(wait))?;
        let result = loop {
            match self.read_line() {
                Ok(line) => {
                    if line.starts_with(b"* ") {
                        lines.push(String::from_utf8_lossy(&line).trim_end().to_string());
                    } else {
                        // A tagged line here means the server ended IDLE on its own.
                        self.idling = None;
                        self.idle_started = None;
                        break Err(ImapError::Protocol("server ended IDLE".into()));
                    }
                }
                Err(e) if is_timeout(&e) => break Ok(lines),
                Err(e) => break Err(e),
            }
        };
        self.reader
            .get_mut()
            .set_read_timeout_to(Some(IO_TIMEOUT))?;
        result
    }

    /// Untagged lines that arrived while a command ended IDLE. Clears them.
    pub fn take_stashed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.stash)
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

    /// Does nothing on the server, but keeps the connection from going idle.
    pub fn noop(&mut self) -> Result<()> {
        self.command("NOOP")?;
        Ok(())
    }

    /// Every label (IMAP mailbox) on the account, including labels with no messages.
    /// System folders and unselectable entries are left out.
    pub fn list_labels(&mut self) -> Result<Vec<String>> {
        let resp = self.command("LIST \"\" \"*\"")?;
        let mut out = Vec::new();
        for line in &resp.untagged {
            let text = String::from_utf8_lossy(line);
            let Some((attrs, name)) = parse_list_line(&text) else {
                continue;
            };
            if attrs.iter().any(|a| a.eq_ignore_ascii_case("\\Noselect")) {
                continue;
            }
            if name.eq_ignore_ascii_case("INBOX") || name.starts_with("[Gmail]") {
                continue;
            }
            out.push(name);
        }
        out.sort();
        out.dedup();
        Ok(out)
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

/// Parses one untagged LIST response: `* LIST (attrs) "delim" name`.
/// Returns the attributes and the mailbox name.
pub fn parse_list_line(line: &str) -> Option<(Vec<&str>, String)> {
    let rest = line.strip_prefix("* LIST (")?;
    let close = rest.find(')')?;
    let attrs: Vec<&str> = rest[..close].split_whitespace().collect();
    let rest = rest[close + 1..].trim_start();
    let rest = match rest.strip_prefix('"') {
        Some(r) => &r[r.find('"')? + 1..],
        None => rest.strip_prefix("NIL")?,
    };
    let name = rest.trim();
    let name = match name.strip_prefix('"') {
        Some(q) => unescape_quoted(q.strip_suffix('"')?),
        None => name.to_string(),
    };
    Some((attrs, name))
}

fn unescape_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
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

#[cfg(test)]
mod select_tests {
    use super::*;

    #[test]
    fn uidvalidity_from_untagged_ok() {
        assert_eq!(
            parse_uidvalidity("* OK [UIDVALIDITY 1768] UIDs valid"),
            Some(1768)
        );
    }

    #[test]
    fn uidvalidity_from_tagged_status() {
        assert_eq!(
            parse_uidvalidity("OK [UIDVALIDITY 7] [READ-WRITE] done"),
            Some(7)
        );
    }

    #[test]
    fn no_uidvalidity_is_none() {
        assert_eq!(parse_uidvalidity("* 3 EXISTS"), None);
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Duration;

    #[test]
    fn silent_server_times_out_instead_of_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept and then say nothing.
        std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(5));
        });
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        match ImapConn::new(stream) {
            Err(ImapError::Io(e)) => assert!(matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            )),
            _ => panic!("expected a timeout error"),
        }
    }

    #[test]
    fn closed_connection_is_reported_as_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"* OK hi\r\n").unwrap();
            // Drop the connection after the greeting.
        });
        let stream = TcpStream::connect(addr).unwrap();
        let mut conn = ImapConn::new(stream).unwrap();
        assert!(matches!(
            conn.noop(),
            Err(ImapError::Closed) | Err(ImapError::Io(_))
        ));
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;

    /// Plays a server that announces a new message during IDLE, ends IDLE on
    /// DONE, and answers one NOOP afterwards.
    fn spawn_idle_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut w = stream;
            w.write_all(b"* OK ready\r\n").unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let line = line.trim_end();
                if line == "a0001 IDLE" {
                    w.write_all(b"+ idling\r\n* 4 EXISTS\r\n").unwrap();
                } else if line == "DONE" {
                    w.write_all(b"a0001 OK IDLE terminated\r\n").unwrap();
                } else if line == "a0002 NOOP" {
                    w.write_all(b"a0002 OK NOOP done\r\n").unwrap();
                    break;
                }
            }
        });
        addr
    }

    #[test]
    fn idle_reports_updates_then_a_command_ends_it() {
        let addr = spawn_idle_server();
        let stream = TcpStream::connect(addr).unwrap();
        let mut conn = ImapConn::new(stream).unwrap();

        // The update arrives while idling. The poll then times out and returns it.
        let lines = conn.idle_poll(Duration::from_millis(300)).unwrap();
        assert_eq!(lines, vec!["* 4 EXISTS".to_string()]);

        // A command ends IDLE with DONE and then runs. The server answers normally.
        conn.command("NOOP").unwrap();
        assert!(conn.take_stashed().is_empty());
    }

    #[test]
    fn server_ending_idle_is_reported() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut w = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            w.write_all(b"* OK ready\r\n").unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            w.write_all(b"+ idling\r\na0001 BAD going away\r\n")
                .unwrap();
        });
        let mut conn = ImapConn::new(TcpStream::connect(addr).unwrap()).unwrap();
        assert!(conn.idle_poll(Duration::from_millis(200)).is_err());
    }
}

#[cfg(test)]
mod list_tests {
    use super::*;

    #[test]
    fn quoted_name_and_attributes() {
        let (attrs, name) =
            parse_list_line("* LIST (\\HasNoChildren) \"/\" \"House/HOA\"").unwrap();
        assert_eq!(attrs, vec!["\\HasNoChildren"]);
        assert_eq!(name, "House/HOA");
    }

    #[test]
    fn unquoted_name_and_nil_delimiter() {
        let (attrs, name) = parse_list_line("* LIST () NIL INBOX").unwrap();
        assert!(attrs.is_empty());
        assert_eq!(name, "INBOX");
    }

    #[test]
    fn escaped_quotes_in_names() {
        let (_, name) = parse_list_line(r#"* LIST () "/" "Say \"hi\"""#).unwrap();
        assert_eq!(name, r#"Say "hi""#);
    }

    #[test]
    fn non_list_lines_are_ignored() {
        assert!(parse_list_line("* 5 EXISTS").is_none());
    }
}
