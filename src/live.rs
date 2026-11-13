//! Live IMAP backend. It connects to Gmail, loads recent messages from All
//! Mail, and sends label changes with `UID STORE`.

use std::io::{Read, Write};

use gmail_imap_proto::imap::{ImapConn, ImapError};
use gmail_imap_proto::labels::{LabelAction, LabelRef, SystemLabel};
use gmail_imap_proto::parse_gmail_fetch;
use mail_parser::MessageParser;

use crate::backend::{MailBackend, Message};

/// Gmail's All Mail folder holds every message, whatever its labels.
pub const ALL_MAIL: &str = "[Gmail]/All Mail";
const FETCH_CHUNK: usize = 50;
/// Newest messages to load from All Mail.
pub const LIVE_LIMIT: usize = 200;

/// Connects to Gmail over TLS, logs in, and loads the newest `LIVE_LIMIT` messages.
/// The error is a message for the user, not a typed error.
pub fn connect_gmail(user: &str, password: &str) -> Result<Box<dyn MailBackend + Send>, String> {
    let conn = gmail_imap_proto::imap::connect_tls("imap.gmail.com", 993)
        .map_err(|e| format!("Could not connect to imap.gmail.com: {e}"))?;
    let backend = LiveBackend::open(conn, user, password, LIVE_LIMIT)
        .map_err(|e| format!("Gmail sign-in or load failed: {e}"))?;
    Ok(Box::new(backend))
}

pub struct LiveBackend<S: Read + Write> {
    conn: ImapConn<S>,
    messages: Vec<Message>,
}

impl<S: Read + Write> LiveBackend<S> {
    /// Logs in, checks for the Gmail extension, and loads the newest `limit`
    /// messages, newest first.
    pub fn open(
        mut conn: ImapConn<S>,
        user: &str,
        password: &str,
        limit: usize,
    ) -> Result<Self, ImapError> {
        conn.login(user, password)?;

        let caps = conn.capabilities()?;
        if !caps.iter().any(|c| c.eq_ignore_ascii_case("X-GM-EXT-1")) {
            return Err(ImapError::Protocol(
                "server does not advertise X-GM-EXT-1; not a Gmail account?".into(),
            ));
        }

        conn.select(ALL_MAIL)?;
        let mut uids = conn.uid_search_all()?;
        uids.sort_unstable_by(|a, b| b.cmp(a));
        uids.truncate(limit);

        let mut messages = Vec::with_capacity(uids.len());
        for chunk in uids.chunks(FETCH_CHUNK) {
            let items = "UID X-GM-MSGID X-GM-THRID X-GM-LABELS BODY.PEEK[]";
            for raw in conn.uid_fetch(chunk, items)? {
                let f = parse_gmail_fetch(&raw).map_err(|e| ImapError::Protocol(e.to_string()))?;
                let (Some(uid), Some(msgid)) = (f.uid, f.msgid) else {
                    continue;
                };
                let body = f.body.unwrap_or(&[]);
                messages.push(to_message(msgid, f.thrid.unwrap_or(0), uid, f.labels, body));
            }
        }
        messages.sort_by_key(|m| std::cmp::Reverse(m.uid));
        Ok(Self { conn, messages })
    }
}

impl<S: Read + Write> Drop for LiveBackend<S> {
    fn drop(&mut self) {
        let _ = self.conn.logout();
    }
}

fn to_message(msgid: u64, thrid: u64, uid: u32, labels: Vec<String>, raw: &[u8]) -> Message {
    let parsed = MessageParser::default().parse(raw);
    let message_id = parsed
        .as_ref()
        .and_then(|m| m.message_id())
        .map(str::to_owned);
    let (from, subject) = match parsed {
        Some(m) => (
            m.from()
                .and_then(|a| a.first())
                .and_then(|a| a.name().or(a.address()))
                .unwrap_or("(unknown)")
                .to_string(),
            m.subject().unwrap_or("(no subject)").to_string(),
        ),
        None => ("(unparseable)".to_string(), "(unparseable)".to_string()),
    };
    Message {
        msgid,
        thrid,
        uid,
        labels: labels.into_iter().collect(),
        from,
        subject,
        message_id,
        raw: raw.to_vec(),
    }
}

impl<S: Read + Write> MailBackend for LiveBackend<S> {
    fn messages(&self) -> Vec<Message> {
        self.messages.clone()
    }

    fn modify_labels(
        &mut self,
        msgids: &[u64],
        action: LabelAction,
        labels: &[String],
    ) -> Result<String, String> {
        let uids: Vec<u32> = self
            .messages
            .iter()
            .filter(|m| msgids.contains(&m.msgid))
            .map(|m| m.uid)
            .collect();
        if uids.is_empty() {
            return Err("no messages selected".into());
        }

        let refs: Vec<LabelRef<'_>> = labels
            .iter()
            .map(|l| match SystemLabel::from_wire(l.as_str()) {
                Some(sys) => LabelRef::System(sys),
                None => LabelRef::Name(l.as_str()),
            })
            .collect();
        let cmd = self
            .conn
            .uid_store(&uids, action, &refs)
            .map_err(|e| e.to_string())?;

        // Re-read labels from the server so the view matches Gmail, not our guess.
        let fresh = self
            .conn
            .uid_fetch(&uids, "UID X-GM-MSGID X-GM-LABELS")
            .map_err(|e| e.to_string())?;
        for raw in fresh {
            let f = parse_gmail_fetch(&raw).map_err(|e| e.to_string())?;
            if let Some(uid) = f.uid
                && let Some(m) = self.messages.iter_mut().find(|m| m.uid == uid)
            {
                m.labels = f.labels.into_iter().collect();
            }
        }
        Ok(cmd)
    }
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

    /// Builds the FETCH responses for the uids in `set`.
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
            if items.contains("BODY") {
                let raw = format!(
                    "From: sender{uid}@example.com\r\nSubject: Hello {uid}\r\n\r\nBody of message {uid}.\r\n"
                );
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
        match cmd {
            "LOGIN \"me@gmail.com\" \"app-pass\"" => format!("{tag} OK LOGIN completed\r\n"),
            c if c.starts_with("LOGIN") => {
                format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n")
            }
            "CAPABILITY" => {
                format!("* CAPABILITY IMAP4rev1 X-GM-EXT-1 UIDPLUS\r\n{tag} OK done\r\n")
            }
            c if c.starts_with("SELECT") => {
                format!("* 3 EXISTS\r\n* 0 RECENT\r\n{tag} OK [READ-WRITE] SELECT completed\r\n")
            }
            "UID SEARCH ALL" => format!("* SEARCH 1 2 3\r\n{tag} OK SEARCH completed\r\n"),
            c if c.starts_with("UID STORE") => format!("{tag} OK STORE completed\r\n"),
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

    fn connect(addr: std::net::SocketAddr) -> ImapConn<TcpStream> {
        ImapConn::new(TcpStream::connect(addr).unwrap()).unwrap()
    }

    #[test]
    fn loads_newest_first_with_gmail_fields() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let addr = spawn_fake(seen.clone());
        let b = LiveBackend::open(connect(addr), "me@gmail.com", "app-pass", 10).unwrap();

        let msgs = b.messages();
        assert_eq!(
            msgs.iter().map(|m| m.uid).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        assert_eq!(msgs[0].subject, "Hello 3");
        assert_eq!(msgs[0].from, "sender3@example.com");

        let m1 = msgs.iter().find(|m| m.uid == 1).unwrap();
        assert_eq!(m1.msgid, 1001);
        assert_eq!(m1.thrid, 901);
        assert!(m1.labels.contains("\\Inbox") && m1.labels.contains("Work"));
        assert!(String::from_utf8_lossy(&m1.raw).contains("Body of message 1."));
    }

    #[test]
    fn label_change_sends_uid_store_and_refreshes() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let addr = spawn_fake(seen.clone());
        let mut b = LiveBackend::open(connect(addr), "me@gmail.com", "app-pass", 10).unwrap();

        let cmd = b
            .modify_labels(&[1001], LabelAction::Remove, &["\\Inbox".to_string()])
            .unwrap();
        assert_eq!(cmd, r#"UID STORE 1 -X-GM-LABELS ("\\Inbox")"#);
        assert!(seen.lock().unwrap().iter().any(|c| c == &cmd));
    }

    #[test]
    fn wrong_password_is_refused() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let addr = spawn_fake(seen);
        let res = LiveBackend::open(connect(addr), "me@gmail.com", "wrong", 10);
        assert!(matches!(res.err(), Some(ImapError::No(_))));
    }
}
