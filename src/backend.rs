//! Message model and the `MailSource` trait that the worker thread drives.
//!
//! A `MailSource` owns one connection. The worker calls it from a background
//! thread, so the GUI never blocks on the network.

use std::collections::BTreeSet;

#[cfg(test)]
use gmail_imap_proto::labels::uid_store_labels;
use gmail_imap_proto::labels::{LabelAction, LabelRef, SystemLabel};
use gmail_imap_proto::store::CachedMessage;
use mail_parser::MessageParser;

/// Why a mail operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// The connection is gone. The worker can reconnect and retry.
    Disconnected(String),
    /// Anything else. Retrying the same request won't help.
    Failed(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Disconnected(m) | SourceError::Failed(m) => f.write_str(m),
        }
    }
}

impl From<String> for SourceError {
    fn from(s: String) -> Self {
        SourceError::Failed(s)
    }
}

impl From<&str> for SourceError {
    fn from(s: &str) -> Self {
        SourceError::Failed(s.to_string())
    }
}

/// What SELECT reports: how many messages the mailbox holds, and its UIDVALIDITY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxInfo {
    pub uidvalidity: u32,
    pub exists: u32,
}

/// One message as the GUI sees it. Headers are loaded with the list; the full
/// body is loaded on demand.
#[derive(Clone, Debug)]
pub struct Message {
    /// `X-GM-MSGID`.
    pub msgid: u64,
    /// `X-GM-THRID`.
    pub thrid: u64,
    /// IMAP UID in All Mail. Label changes and body fetches use this.
    pub uid: u32,
    /// Raw `X-GM-LABELS` names, e.g. `\Inbox` or `Work/Projects`.
    pub labels: BTreeSet<String>,
    pub from: String,
    pub subject: String,
    pub date: String,
    /// The header block, as sent by the server.
    pub headers: Vec<u8>,
    /// The full RFC 5322 message, once fetched.
    pub body: Option<Vec<u8>>,
}

impl Message {
    /// Builds a listing entry from a header block. Parsing is best effort.
    pub fn from_headers(
        msgid: u64,
        thrid: u64,
        uid: u32,
        labels: Vec<String>,
        headers: Vec<u8>,
    ) -> Self {
        let parsed = MessageParser::default().parse(&headers);
        let (from, subject, date) = match &parsed {
            Some(m) => (
                m.from()
                    .and_then(|a| a.first())
                    .and_then(|a| a.name().or(a.address()))
                    .unwrap_or("(unknown)")
                    .to_string(),
                m.subject().unwrap_or("(no subject)").to_string(),
                m.date().map(|d| d.to_rfc822()).unwrap_or_default(),
            ),
            None => (
                "(unparseable)".into(),
                "(unparseable)".into(),
                String::new(),
            ),
        };
        Self {
            msgid,
            thrid,
            uid,
            labels: labels.into_iter().collect(),
            from,
            subject,
            date,
            headers,
            body: None,
        }
    }
}

/// New label sets for UIDs, as returned after a label change.
pub type LabelUpdates = Vec<(u32, BTreeSet<String>)>;

impl Message {
    /// The cache form of this message. Only messages with a UID can be cached.
    pub fn to_cached(&self) -> CachedMessage {
        CachedMessage {
            msgid: self.msgid,
            thrid: self.thrid,
            uid: Some(self.uid),
            labels: self.labels.clone(),
            from: self.from.clone(),
            subject: self.subject.clone(),
            date: self.date.clone(),
            headers: self.headers.clone(),
        }
    }

    /// A listing entry from a cached header. Returns `None` if the UID was
    /// invalidated, since such a message can't be acted on.
    pub fn from_cached(c: CachedMessage) -> Option<Self> {
        let uid = c.uid?;
        Some(Self::from_headers(
            c.msgid,
            c.thrid,
            uid,
            c.labels.into_iter().collect(),
            c.headers,
        ))
    }
}

/// Everything the worker needs from a mailbox.
pub trait MailSource: Send {
    /// Selects All Mail and reports its size and UIDVALIDITY.
    fn select(&mut self) -> Result<MailboxInfo, SourceError>;
    /// Keeps the connection alive. Does nothing else.
    fn noop(&mut self) -> Result<(), SourceError>;
    /// UIDs at least `start`, ascending.
    fn search_from(&mut self, start: u32) -> Result<Vec<u32>, SourceError>;
    /// Header-only listing entries, in the order the server returns them.
    fn fetch_headers(&mut self, uids: &[u32]) -> Result<Vec<Message>, SourceError>;
    /// The full RFC 5322 message.
    fn fetch_body(&mut self, uid: u32) -> Result<Vec<u8>, SourceError>;
    /// Applies a label change. Returns the command for the log and the
    /// labels of each affected UID afterwards.
    fn modify_labels(
        &mut self,
        uids: &[u32],
        action: LabelAction,
        labels: &[String],
    ) -> Result<(String, LabelUpdates), SourceError>;
}

/// Maps label names to IMAP label references, recognizing system labels.
pub fn label_refs(labels: &[String]) -> Vec<LabelRef<'_>> {
    labels
        .iter()
        .map(|l| match SystemLabel::from_wire(l.as_str()) {
            Some(sys) => LabelRef::System(sys),
            None => LabelRef::Name(l.as_str()),
        })
        .collect()
}

/// The header block of a raw message: everything up to and including the first blank line.
#[cfg(test)]
pub fn header_block(raw: &[u8]) -> Vec<u8> {
    match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => raw[..i + 4].to_vec(),
        None => raw.to_vec(),
    }
}

#[cfg(test)]
struct MockEntry {
    msgid: u64,
    thrid: u64,
    uid: u32,
    labels: BTreeSet<String>,
    raw: Vec<u8>,
}

/// In-memory mailbox for the demo and for tests.
#[cfg(test)]
pub struct MockSource {
    entries: Vec<MockEntry>,
}

#[cfg(test)]
impl MockSource {
    pub fn demo() -> Self {
        let entry = |msgid, thrid, uid, labels: &[&str], from: &str, subject: &str, body: &str| {
            let raw = format!(
                "From: {from}\r\nTo: me@example.com\r\nSubject: {subject}\r\n\
                 Date: Tue, 10 Nov 2026 09:30:00 +0000\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
            )
            .into_bytes();
            MockEntry {
                msgid,
                thrid,
                uid,
                labels: labels.iter().map(|s| s.to_string()).collect(),
                raw,
            }
        };
        Self {
            entries: vec![
                entry(
                    1001,
                    9001,
                    1,
                    &["\\Inbox", "\\Important", "Work/Projects"],
                    "Ada <ada@example.com>",
                    "Q3 plan",
                    "Here is the draft of the Q3 plan.\nPlease review by Friday.",
                ),
                entry(
                    1002,
                    9001,
                    2,
                    &["\\Inbox", "Work/Projects"],
                    "Charles <charles@example.com>",
                    "Re: Q3 plan",
                    "Looks good. Two comments on the budget section.",
                ),
                entry(
                    1003,
                    9002,
                    3,
                    &["\\Inbox", "\\Starred"],
                    "Shop <orders@example.com>",
                    "Your receipt",
                    "Thanks for your order #4411.",
                ),
                entry(
                    1004,
                    9003,
                    4,
                    &["Personal"],
                    "Mary <mary@example.com>",
                    "Holiday photos",
                    "Uploaded the photos from the lake trip.",
                ),
                entry(
                    1005,
                    9004,
                    5,
                    &["\\Trash"],
                    "News <news@example.com>",
                    "Old newsletter",
                    "This one is in the trash.",
                ),
                entry(
                    1006,
                    9005,
                    6,
                    &["\\Sent"],
                    "me@example.com",
                    "Re: lunch",
                    "Thursday works for me.",
                ),
            ],
        }
    }

    fn entry_mut(&mut self, uid: u32) -> Option<&mut MockEntry> {
        self.entries.iter_mut().find(|e| e.uid == uid)
    }
}

#[cfg(test)]
impl MailSource for MockSource {
    fn select(&mut self) -> Result<MailboxInfo, SourceError> {
        Ok(MailboxInfo {
            uidvalidity: 1,
            exists: self.entries.len() as u32,
        })
    }

    fn noop(&mut self) -> Result<(), SourceError> {
        Ok(())
    }

    fn search_from(&mut self, start: u32) -> Result<Vec<u32>, SourceError> {
        let mut uids: Vec<u32> = self
            .entries
            .iter()
            .map(|e| e.uid)
            .filter(|&u| u >= start)
            .collect();
        uids.sort_unstable();
        Ok(uids)
    }

    fn fetch_headers(&mut self, uids: &[u32]) -> Result<Vec<Message>, SourceError> {
        Ok(uids
            .iter()
            .filter_map(|&uid| self.entries.iter().find(|e| e.uid == uid))
            .map(|e| {
                Message::from_headers(
                    e.msgid,
                    e.thrid,
                    e.uid,
                    e.labels.iter().cloned().collect(),
                    header_block(&e.raw),
                )
            })
            .collect())
    }

    fn fetch_body(&mut self, uid: u32) -> Result<Vec<u8>, SourceError> {
        self.entries
            .iter()
            .find(|e| e.uid == uid)
            .map(|e| e.raw.clone())
            .ok_or_else(|| format!("no message with UID {uid}").into())
    }

    fn modify_labels(
        &mut self,
        uids: &[u32],
        action: LabelAction,
        labels: &[String],
    ) -> Result<(String, LabelUpdates), SourceError> {
        let found: Vec<u32> = uids
            .iter()
            .copied()
            .filter(|u| self.entries.iter().any(|e| e.uid == *u))
            .collect();
        if found.is_empty() {
            return Err("no messages selected".into());
        }
        let command = uid_store_labels(&found, action, &label_refs(labels))
            .ok_or_else(|| "nothing to change".to_string())?;

        let mut updates = Vec::new();
        for &uid in &found {
            if let Some(e) = self.entry_mut(uid) {
                match action {
                    LabelAction::Add => e.labels.extend(labels.iter().cloned()),
                    LabelAction::Remove => {
                        for l in labels {
                            e.labels.remove(l);
                        }
                    }
                    LabelAction::Replace => e.labels = labels.iter().cloned().collect(),
                }
                updates.push((uid, e.labels.clone()));
            }
        }
        Ok((command, updates))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_of(src: &mut MockSource, uid: u32) -> BTreeSet<String> {
        src.fetch_headers(&[uid]).unwrap().pop().unwrap().labels
    }

    #[test]
    fn archive_removes_inbox_and_returns_store_command() {
        let mut src = MockSource::demo();
        let (cmd, _) = src
            .modify_labels(&[3], LabelAction::Remove, &["\\Inbox".to_string()])
            .unwrap();
        assert_eq!(cmd, r#"UID STORE 3 -X-GM-LABELS ("\\Inbox")"#);
        assert!(!labels_of(&mut src, 3).contains("\\Inbox"));
        assert!(labels_of(&mut src, 3).contains("\\Starred"));
    }

    #[test]
    fn add_user_label_to_multiple_messages() {
        let mut src = MockSource::demo();
        let (cmd, updates) = src
            .modify_labels(&[1, 2], LabelAction::Add, &["Later".to_string()])
            .unwrap();
        assert_eq!(cmd, r#"UID STORE 1:2 +X-GM-LABELS ("Later")"#);
        assert_eq!(updates.len(), 2);
        assert!(labels_of(&mut src, 1).contains("Later"));
        assert!(labels_of(&mut src, 2).contains("Later"));
    }

    #[test]
    fn unknown_uid_is_an_error() {
        let mut src = MockSource::demo();
        assert!(
            src.modify_labels(&[9999], LabelAction::Add, &["x".to_string()])
                .is_err()
        );
    }

    #[test]
    fn header_block_stops_at_blank_line() {
        let raw = b"From: a\r\nSubject: b\r\n\r\nbody\r\n";
        assert_eq!(header_block(raw), b"From: a\r\nSubject: b\r\n\r\n".to_vec());
    }
}
