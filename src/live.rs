//! Gmail session: implements `MailSource` over IMAP.

use std::io::{Read, Write};
use std::time::Duration;

use gmail_imap_proto::imap::{ImapConn, ImapError, SetReadTimeout, connect_tls};
use gmail_imap_proto::labels::LabelAction;
use gmail_imap_proto::parse_gmail_fetch;

use crate::backend::{
    IdleNotice, MailSource, MailboxInfo, Message, MessageUpdate, SourceError, label_refs,
};

/// Gmail's All Mail folder holds every message, whatever its labels.
pub const ALL_MAIL: &str = "[Gmail]/All Mail";
/// How many UIDs go into one FETCH.
const FETCH_CHUNK: usize = 200;

pub struct LiveSession<S: Read + Write> {
    conn: ImapConn<S>,
    selected: bool,
    /// The server advertised CONDSTORE, so changes can be fetched by modseq.
    condstore: bool,
    /// The server advertised IDLE, so changes can be pushed.
    idle: bool,
    /// Notices from IDLE that arrived during a command, not yet handed out.
    notices: Vec<IdleNotice>,
}

impl<S: Read + Write> LiveSession<S> {
    /// Logs in and checks that the server is Gmail.
    pub fn open(mut conn: ImapConn<S>, user: &str, password: &str) -> Result<Self, String> {
        let err = |e: gmail_imap_proto::imap::ImapError| e.to_string();
        conn.login(user, password).map_err(err)?;
        let caps = conn.capabilities().map_err(err)?;
        if !caps.iter().any(|c| c.eq_ignore_ascii_case("X-GM-EXT-1")) {
            return Err("server does not advertise X-GM-EXT-1; not a Gmail account?".into());
        }
        let condstore = caps.iter().any(|c| c.eq_ignore_ascii_case("CONDSTORE"));
        let idle = caps.iter().any(|c| c.eq_ignore_ascii_case("IDLE"));
        Ok(Self {
            conn,
            selected: false,
            condstore,
            idle,
            notices: Vec::new(),
        })
    }

    fn ensure_selected(&mut self) -> Result<(), SourceError> {
        if !self.selected {
            self.conn.select(ALL_MAIL).map_err(to_source_error)?;
            self.selected = true;
        }
        Ok(())
    }
}

/// A dropped or timed-out connection can be retried. Everything else can't.
fn to_source_error(e: ImapError) -> SourceError {
    match e {
        ImapError::Io(_) | ImapError::Closed => SourceError::Disconnected(e.to_string()),
        other => SourceError::Failed(other.to_string()),
    }
}

/// Reads an IDLE line from the server. Returns `None` for anything that isn't a change.
fn idle_notice(line: &str) -> Option<IdleNotice> {
    let mut parts = line.split_whitespace();
    if parts.next() != Some("*") {
        return None;
    }
    parts.next()?.parse::<u32>().ok()?;
    match parts.next()? {
        "EXISTS" => Some(IdleNotice::NewMail),
        "EXPUNGE" => Some(IdleNotice::Expunged),
        "FETCH" => Some(IdleNotice::Changed),
        _ => None,
    }
}

impl<S: Read + Write + Send + SetReadTimeout> MailSource for LiveSession<S> {
    fn select(&mut self) -> Result<MailboxInfo, SourceError> {
        let sel = self.conn.select(ALL_MAIL).map_err(to_source_error)?;
        self.selected = true;
        Ok(MailboxInfo {
            uidvalidity: sel.uidvalidity,
            exists: sel.exists,
            highest_modseq: sel.highestmodseq,
        })
    }

    fn noop(&mut self) -> Result<(), SourceError> {
        self.conn.noop().map_err(to_source_error)
    }

    fn search_from(&mut self, start: u32) -> Result<Vec<u32>, SourceError> {
        self.ensure_selected()?;
        self.conn.uid_search_from(start).map_err(to_source_error)
    }

    fn fetch_headers(&mut self, uids: &[u32]) -> Result<Vec<Message>, SourceError> {
        self.ensure_selected()?;
        let mut out = Vec::with_capacity(uids.len());
        for chunk in uids.chunks(FETCH_CHUNK) {
            let items =
                "UID X-GM-MSGID X-GM-THRID X-GM-LABELS FLAGS BODY.PEEK[HEADER] BODYSTRUCTURE";
            for raw in self.conn.uid_fetch(chunk, items).map_err(to_source_error)? {
                let f = parse_gmail_fetch(&raw).map_err(|e| SourceError::Failed(e.to_string()))?;
                let (Some(uid), Some(msgid)) = (f.uid, f.msgid) else {
                    continue;
                };
                let headers = f.header.unwrap_or(&[]).to_vec();
                let mut m = Message::from_headers(
                    msgid,
                    f.thrid.unwrap_or(0),
                    uid,
                    f.labels.clone(),
                    f.seen().unwrap_or(false),
                    headers,
                );
                m.attachment = f.has_attachment();
                out.push(m);
            }
        }
        Ok(out)
    }

    fn fetch_body(&mut self, uid: u32) -> Result<Vec<u8>, SourceError> {
        self.ensure_selected()?;
        for raw in self
            .conn
            .uid_fetch(&[uid], "BODY.PEEK[]")
            .map_err(to_source_error)?
        {
            let f = parse_gmail_fetch(&raw).map_err(|e| SourceError::Failed(e.to_string()))?;
            if let Some(body) = f.body {
                return Ok(body.to_vec());
            }
        }
        Err(SourceError::Failed(format!(
            "server returned no body for UID {uid}"
        )))
    }

    fn fetch_bodies(&mut self, uids: &[u32]) -> Result<Vec<(u32, Vec<u8>)>, SourceError> {
        self.ensure_selected()?;
        let mut out = Vec::with_capacity(uids.len());
        for raw in self
            .conn
            .uid_fetch(uids, "UID BODY.PEEK[]")
            .map_err(to_source_error)?
        {
            let f = parse_gmail_fetch(&raw).map_err(|e| SourceError::Failed(e.to_string()))?;
            if let (Some(uid), Some(body)) = (f.uid, f.body) {
                out.push((uid, body.to_vec()));
            }
        }
        Ok(out)
    }

    fn modify_labels(
        &mut self,
        uids: &[u32],
        action: LabelAction,
        labels: &[String],
    ) -> Result<(String, Vec<MessageUpdate>), SourceError> {
        self.ensure_selected()?;
        let command = self
            .conn
            .uid_store(uids, action, &label_refs(labels))
            .map_err(to_source_error)?;

        // Re-read labels and read state so the view matches the server, not our guess.
        let updates = self.fetch_states(uids)?;
        Ok((command, updates))
    }

    fn fetch_structures(&mut self, uids: &[u32]) -> Result<Vec<(u32, bool)>, SourceError> {
        self.ensure_selected()?;
        let mut out = Vec::with_capacity(uids.len());
        for raw in self
            .conn
            .uid_fetch(uids, "UID BODYSTRUCTURE")
            .map_err(to_source_error)?
        {
            let f = parse_gmail_fetch(&raw).map_err(|e| SourceError::Failed(e.to_string()))?;
            if let (Some(uid), Some(has)) = (f.uid, f.has_attachment()) {
                out.push((uid, has));
            }
        }
        Ok(out)
    }

    fn set_seen(
        &mut self,
        uids: &[u32],
        seen: bool,
    ) -> Result<(String, Vec<MessageUpdate>), SourceError> {
        self.ensure_selected()?;
        let command = self
            .conn
            .uid_store_seen(uids, seen)
            .map_err(to_source_error)?;
        // Re-read the state, so the view matches the server rather than our guess.
        let updates = self.fetch_states(uids)?;
        Ok((command, updates))
    }

    fn list_labels(&mut self) -> Result<Vec<String>, SourceError> {
        self.conn.list_labels().map_err(to_source_error)
    }

    fn supports_condstore(&self) -> bool {
        self.condstore
    }

    fn changed_since(&mut self, modseq: u64) -> Result<Vec<MessageUpdate>, SourceError> {
        self.ensure_selected()?;
        let mut out = Vec::new();
        for raw in self
            .conn
            .uid_fetch_changed_since("UID X-GM-LABELS FLAGS", modseq)
            .map_err(to_source_error)?
        {
            let f = parse_gmail_fetch(&raw).map_err(|e| SourceError::Failed(e.to_string()))?;
            if let Some(uid) = f.uid {
                out.push(MessageUpdate {
                    uid,
                    labels: f.labels.iter().cloned().collect(),
                    seen: f.seen().unwrap_or(false),
                });
            }
        }
        Ok(out)
    }

    fn supports_idle(&self) -> bool {
        self.idle
    }

    fn idle_poll(&mut self, wait: Duration) -> Result<Vec<IdleNotice>, SourceError> {
        self.ensure_selected()?;
        let lines = self.conn.idle_poll(wait).map_err(to_source_error)?;
        Ok(lines.iter().filter_map(|l| idle_notice(l)).collect())
    }

    fn take_notices(&mut self) -> Vec<IdleNotice> {
        let stashed = self.conn.take_stashed();
        self.notices
            .extend(stashed.iter().filter_map(|l| idle_notice(l)));
        std::mem::take(&mut self.notices)
    }
}

impl<S: Read + Write> LiveSession<S> {
    /// Labels and read state for the given UIDs, as the server reports them now.
    fn fetch_states(&mut self, uids: &[u32]) -> Result<Vec<MessageUpdate>, SourceError> {
        let mut out = Vec::new();
        for raw in self
            .conn
            .uid_fetch(uids, "UID X-GM-LABELS FLAGS")
            .map_err(to_source_error)?
        {
            let f = parse_gmail_fetch(&raw).map_err(|e| SourceError::Failed(e.to_string()))?;
            if let Some(uid) = f.uid {
                out.push(MessageUpdate {
                    uid,
                    labels: f.labels.iter().cloned().collect(),
                    seen: f.seen().unwrap_or(false),
                });
            }
        }
        Ok(out)
    }
}

impl<S: Read + Write> Drop for LiveSession<S> {
    fn drop(&mut self) {
        let _ = self.conn.logout();
    }
}

/// Connects to Gmail over TLS and signs in.
pub fn connect_gmail(user: &str, password: &str) -> Result<Box<dyn MailSource>, String> {
    let conn = connect_tls("imap.gmail.com", 993)
        .map_err(|e| format!("Could not connect to imap.gmail.com: {e}"))?;
    let session = LiveSession::open(conn, user, password)
        .map_err(|e| format!("Gmail sign-in failed: {e}"))?;
    Ok(Box::new(session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// Uids named by an IMAP sequence set such as `1:3,7`.
    fn uids_in_set(set: &str) -> Vec<u32> {
        let mut out = Vec::new();
        for part in set.split(',') {
            match part.split_once(':') {
                Some((a, b)) => out.extend(a.parse::<u32>().unwrap()..=b.parse::<u32>().unwrap()),
                None => out.push(part.parse().unwrap()),
            }
        }
        out
    }

    /// FETCH responses for the uids in `set`. Supports header-only and full-body items.
    fn fetch_response(set: &str, items: &str) -> String {
        let mut out = String::new();
        for uid in uids_in_set(set) {
            let msgid = 1000 + uid;
            let thrid = 900 + uid;
            let labels = match uid {
                1 => "(\"\\\\Inbox\" \"Work\")",
                2 => "(\"\\\\Inbox\")",
                _ => "(\"Personal\")",
            };
            let raw = format!(
                "From: sender{uid}@example.com\r\nSubject: Hello {uid}\r\n\r\nBody of message {uid}.\r\n"
            );
            let header_len = raw.find("\r\n\r\n").unwrap() + 4;
            if items.contains("BODY.PEEK[HEADER]") {
                let header = &raw[..header_len];
                out.push_str(&format!(
                    "* {uid} FETCH (UID {uid} X-GM-MSGID {msgid} X-GM-THRID {thrid} X-GM-LABELS {labels} BODY[HEADER] {{{}}}\r\n{header})\r\n",
                    header.len()
                ));
            } else if items.contains("BODY") {
                out.push_str(&format!(
                    "* {uid} FETCH (UID {uid} X-GM-MSGID {msgid} X-GM-THRID {thrid} X-GM-LABELS {labels} BODY[] {{{}}}\r\n{raw})\r\n",
                    raw.len()
                ));
            } else {
                out.push_str(&format!(
                    "* {uid} FETCH (UID {uid} X-GM-MSGID {msgid} X-GM-LABELS {labels})\r\n"
                ));
            }
        }
        out
    }

    fn respond(tag: &str, cmd: &str) -> String {
        if let Some(rest) = cmd.strip_prefix("UID FETCH ") {
            let (set, items) = rest.split_once(" (").unwrap();
            let items = items.trim_end_matches(')');
            return format!("{}{tag} OK FETCH completed\r\n", fetch_response(set, items));
        }
        if let Some(rest) = cmd.strip_prefix("UID SEARCH UID ") {
            let start: u32 = rest.trim_end_matches(":*").parse().unwrap();
            let uids: Vec<String> = (1..=3)
                .filter(|u| *u >= start)
                .map(|u| u.to_string())
                .collect();
            return format!(
                "* SEARCH {}\r\n{tag} OK SEARCH completed\r\n",
                uids.join(" ")
            );
        }
        match cmd {
            "LOGIN \"me@gmail.com\" \"app-pass\"" => format!("{tag} OK LOGIN completed\r\n"),
            c if c.starts_with("LOGIN") => {
                format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n")
            }
            "CAPABILITY" => {
                format!("* CAPABILITY IMAP4rev1 X-GM-EXT-1 UIDPLUS\r\n{tag} OK done\r\n")
            }
            c if c.starts_with("SELECT") => format!(
                "* 3 EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY 42] UIDs valid\r\n{tag} OK [READ-WRITE] SELECT completed\r\n"
            ),
            c if c.starts_with("UID STORE") => format!("{tag} OK STORE completed\r\n"),
            "NOOP" => format!("{tag} OK NOOP completed\r\n"),
            "LOGOUT" => format!("* BYE bye\r\n{tag} OK LOGOUT completed\r\n"),
            _ => format!("{tag} BAD unknown command\r\n"),
        }
    }

    /// Plain-TCP fake IMAP server. It records every command it receives.
    fn spawn_fake(seen: Arc<Mutex<Vec<String>>>) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            writer.write_all(b"* OK fake gimap ready\r\n").unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let line = line.trim_end();
                let (tag, cmd) = line.split_once(' ').unwrap();
                seen.lock().unwrap().push(cmd.to_string());
                writer.write_all(respond(tag, cmd).as_bytes()).unwrap();
                if cmd == "LOGOUT" {
                    break;
                }
            }
        });
        addr
    }

    fn session(
        addr: std::net::SocketAddr,
        password: &str,
    ) -> Result<LiveSession<TcpStream>, String> {
        let conn = ImapConn::new(TcpStream::connect(addr).unwrap()).unwrap();
        LiveSession::open(conn, "me@gmail.com", password)
    }

    #[test]
    fn lists_headers_newest_first_and_reports_uidvalidity() {
        let addr = spawn_fake(Arc::new(Mutex::new(Vec::new())));
        let mut s = session(addr, "app-pass").unwrap();
        let info = s.select().unwrap();
        assert_eq!(info.uidvalidity, 42);
        assert_eq!(info.exists, 3);
        let mut uids = s.search_from(1).unwrap();
        uids.reverse();
        assert_eq!(uids, vec![3, 2, 1]);

        let msgs = s.fetch_headers(&uids).unwrap();
        let m1 = msgs.iter().find(|m| m.uid == 1).unwrap();
        assert_eq!(m1.msgid, 1001);
        assert_eq!(m1.thrid, 901);
        assert_eq!(m1.subject, "Hello 1");
        assert_eq!(m1.from, "sender1@example.com");
        assert!(m1.labels.contains("\\Inbox") && m1.labels.contains("Work"));
        assert!(m1.body.is_none(), "headers only until a body is requested");
    }

    #[test]
    fn fetches_body_on_demand() {
        let addr = spawn_fake(Arc::new(Mutex::new(Vec::new())));
        let mut s = session(addr, "app-pass").unwrap();
        s.select().unwrap();
        let body = s.fetch_body(2).unwrap();
        assert!(String::from_utf8_lossy(&body).contains("Body of message 2."));
    }

    #[test]
    fn label_change_sends_uid_store_and_returns_labels() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let addr = spawn_fake(seen.clone());
        let mut s = session(addr, "app-pass").unwrap();
        let (cmd, updates) = s
            .modify_labels(&[1], LabelAction::Remove, &["\\Inbox".to_string()])
            .unwrap();
        assert_eq!(cmd, r#"UID STORE 1 -X-GM-LABELS ("\\Inbox")"#);
        assert!(seen.lock().unwrap().iter().any(|c| c == &cmd));
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].uid, 1);
    }

    #[test]
    fn wrong_password_is_refused() {
        let addr = spawn_fake(Arc::new(Mutex::new(Vec::new())));
        let res = session(addr, "wrong");
        assert!(res.is_err());
    }
}
