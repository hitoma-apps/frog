//! Message model and the `MailSource` trait that the worker thread drives.
//!
//! A `MailSource` owns one connection. The worker calls it from a background
//! thread, so the GUI never blocks on the network.

use std::collections::BTreeSet;
use std::time::Duration;

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

/// What SELECT reports: the mailbox size, its UIDVALIDITY, and its modseq if the server has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxInfo {
    pub uidvalidity: u32,
    pub exists: u32,
    /// `HIGHESTMODSEQ`, if the server supports CONDSTORE.
    pub highest_modseq: Option<u64>,
}

/// The current labels and read state of one message, as the server reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageUpdate {
    pub uid: u32,
    pub labels: BTreeSet<String>,
    pub seen: bool,
}

/// Something the server told us while idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleNotice {
    /// New messages arrived.
    NewMail,
    /// Messages were removed.
    Expunged,
    /// A message's flags or labels changed.
    Changed,
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
    /// Whether the message has been read.
    pub seen: bool,
    pub from: String,
    pub subject: String,
    pub date: String,
    /// When the message was sent, as a Unix timestamp. 0 if the Date header is missing.
    pub sent: i64,
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
        seen: bool,
        headers: Vec<u8>,
    ) -> Self {
        let parsed = MessageParser::default().parse(&headers);
        let sent = parsed
            .as_ref()
            .and_then(|m| m.date())
            .map_or(0, |d| d.to_timestamp());
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
            seen,
            from,
            subject,
            date,
            sent,
            headers,
            body: None,
        }
    }

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
            seen: self.seen,
        }
    }

    /// A listing entry from a cached header. Returns `None` if the UID was
    /// invalidated, since such a message can't be acted on.
    pub fn from_cached(c: CachedMessage) -> Option<Self> {
        let uid = c.uid?;
        let mut m = Self::from_headers(
            c.msgid,
            c.thrid,
            uid,
            c.labels.into_iter().collect(),
            c.seen,
            c.headers,
        );
        // The cached subject, sender and date are what the header parse gave us.
        m.from = c.from;
        m.subject = c.subject;
        m.date = c.date;
        Some(m)
    }
}

/// Everything the worker needs from a mailbox.
pub trait MailSource: Send {
    /// Selects All Mail and reports its size, UIDVALIDITY and modseq.
    fn select(&mut self) -> Result<MailboxInfo, SourceError>;
    /// Keeps the connection alive. Does nothing else.
    fn noop(&mut self) -> Result<(), SourceError>;
    /// UIDs at least `start`, ascending.
    fn search_from(&mut self, start: u32) -> Result<Vec<u32>, SourceError>;
    /// Header-only listing entries, in the order the server returns them.
    fn fetch_headers(&mut self, uids: &[u32]) -> Result<Vec<Message>, SourceError>;
    /// The full RFC 5322 message.
    fn fetch_body(&mut self, uid: u32) -> Result<Vec<u8>, SourceError>;
    /// Applies a label change. Returns the command for the log and the new
    /// state of each affected message.
    fn modify_labels(
        &mut self,
        uids: &[u32],
        action: LabelAction,
        labels: &[String],
    ) -> Result<(String, Vec<MessageUpdate>), SourceError>;

    /// Every label on the account, including ones with no messages yet.
    fn list_labels(&mut self) -> Result<Vec<String>, SourceError> {
        Ok(Vec::new())
    }

    /// Whether the server reports changes since a modseq (CONDSTORE).
    fn supports_condstore(&self) -> bool {
        false
    }
    /// The state of every message changed after `modseq`. Changes made
    /// elsewhere, such as in the web UI, appear here.
    fn changed_since(&mut self, modseq: u64) -> Result<Vec<MessageUpdate>, SourceError> {
        let _ = modseq;
        Err("CONDSTORE is not supported by this server".into())
    }

    /// Whether the server supports IDLE (push notifications).
    fn supports_idle(&self) -> bool {
        false
    }
    /// Waits up to `wait` for the server to report a change while idle.
    fn idle_poll(&mut self, wait: Duration) -> Result<Vec<IdleNotice>, SourceError> {
        let _ = wait;
        Err("IDLE is not supported by this server".into())
    }
    /// Notices that arrived while the connection was busy with a command.
    fn take_notices(&mut self) -> Vec<IdleNotice> {
        Vec::new()
    }
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

/// In-memory mailbox for tests. Clones share the same state, so a test can
/// change messages through a [`MockHandle`] while the worker is running.
#[cfg(test)]
#[derive(Clone)]
pub struct MockSource {
    state: std::sync::Arc<std::sync::Mutex<MockState>>,
}

#[cfg(test)]
#[derive(Default)]
struct MockState {
    entries: Vec<MockEntry>,
    highest_modseq: u64,
}

#[cfg(test)]
struct MockEntry {
    msgid: u64,
    thrid: u64,
    uid: u32,
    labels: BTreeSet<String>,
    seen: bool,
    raw: Vec<u8>,
    modseq: u64,
}

/// Changes the mock mailbox the way another client would.
#[cfg(test)]
#[derive(Clone)]
pub struct MockHandle {
    state: std::sync::Arc<std::sync::Mutex<MockState>>,
}

#[cfg(test)]
impl MockHandle {
    /// Replaces a message's labels, as another client would.
    pub fn set_labels(&self, uid: u32, labels: &[&str]) {
        let mut st = self.state.lock().unwrap();
        st.highest_modseq += 1;
        let modseq = st.highest_modseq;
        if let Some(e) = st.entries.iter_mut().find(|e| e.uid == uid) {
            e.labels = labels.iter().map(|s| s.to_string()).collect();
            e.modseq = modseq;
        }
    }

    /// Delivers a new message to the mailbox, as an incoming mail would. Returns its UID.
    pub fn deliver(&self, from: &str, subject: &str, labels: &[&str]) -> u32 {
        let mut st = self.state.lock().unwrap();
        let uid = st.entries.iter().map(|e| e.uid).max().unwrap_or(0) + 1;
        st.highest_modseq += 1;
        let modseq = st.highest_modseq;
        let raw = format!("From: {from}\r\nSubject: {subject}\r\n\r\nbody\r\n").into_bytes();
        st.entries.push(MockEntry {
            msgid: 2000 + uid as u64,
            thrid: 2000 + uid as u64,
            uid,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            seen: false,
            raw,
            modseq,
        });
        uid
    }

    /// Removes a message, as an expunge from another client would.
    pub fn expunge(&self, uid: u32) {
        let mut st = self.state.lock().unwrap();
        st.entries.retain(|e| e.uid != uid);
    }

    /// Marks a message read or unread, as another client would.
    pub fn set_seen(&self, uid: u32, seen: bool) {
        let mut st = self.state.lock().unwrap();
        st.highest_modseq += 1;
        let modseq = st.highest_modseq;
        if let Some(e) = st.entries.iter_mut().find(|e| e.uid == uid) {
            e.seen = seen;
            e.modseq = modseq;
        }
    }
}

#[cfg(test)]
impl MockSource {
    /// Six messages, UIDs 1 to 6.
    pub fn demo() -> Self {
        let entry = |uid: u32,
                     msgid: u64,
                     thrid: u64,
                     labels: &[&str],
                     seen: bool,
                     from: &str,
                     subject: &str,
                     body: &str| {
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
                seen,
                raw,
                modseq: uid as u64,
            }
        };
        let entries = vec![
            entry(
                1,
                1001,
                9001,
                &["\\Inbox", "\\Important", "Work/Projects"],
                false,
                "Ada <ada@example.com>",
                "Q3 plan",
                "Here is the draft of the Q3 plan.\nPlease review by Friday.",
            ),
            entry(
                2,
                1002,
                9001,
                &["\\Inbox", "Work/Projects"],
                true,
                "Charles <charles@example.com>",
                "Re: Q3 plan",
                "Looks good. Two comments on the budget section.",
            ),
            entry(
                3,
                1003,
                9002,
                &["\\Inbox", "\\Starred"],
                false,
                "Shop <orders@example.com>",
                "Your receipt",
                "Thanks for your order #4411.",
            ),
            entry(
                4,
                1004,
                9003,
                &["Personal"],
                true,
                "Mary <mary@example.com>",
                "Holiday photos",
                "Uploaded the photos from the lake trip.",
            ),
            entry(
                5,
                1005,
                9004,
                &["\\Trash"],
                true,
                "News <news@example.com>",
                "Old newsletter",
                "This one is in the trash.",
            ),
            entry(
                6,
                1006,
                9005,
                &["\\Sent"],
                true,
                "me@example.com",
                "Re: lunch",
                "Thursday works for me.",
            ),
        ];
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(MockState {
                entries,
                highest_modseq: 6,
            })),
        }
    }

    /// A handle for changing this mailbox from a test.
    pub fn handle(&self) -> MockHandle {
        MockHandle {
            state: self.state.clone(),
        }
    }

    /// Labels in the mailbox, including one that has no messages.
    fn all_labels(&self) -> Vec<String> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<String> = st
            .entries
            .iter()
            .flat_map(|e| e.labels.iter().cloned())
            .collect();
        out.push("Unused".into());
        out.retain(|l| !l.starts_with('\\'));
        out.sort();
        out.dedup();
        out
    }

    fn update_of(e: &MockEntry) -> MessageUpdate {
        MessageUpdate {
            uid: e.uid,
            labels: e.labels.clone(),
            seen: e.seen,
        }
    }
}

#[cfg(test)]
impl MailSource for MockSource {
    fn select(&mut self) -> Result<MailboxInfo, SourceError> {
        let st = self.state.lock().unwrap();
        Ok(MailboxInfo {
            uidvalidity: 1,
            exists: st.entries.len() as u32,
            highest_modseq: Some(st.highest_modseq),
        })
    }

    fn noop(&mut self) -> Result<(), SourceError> {
        Ok(())
    }

    fn search_from(&mut self, start: u32) -> Result<Vec<u32>, SourceError> {
        let st = self.state.lock().unwrap();
        let mut uids: Vec<u32> = st
            .entries
            .iter()
            .map(|e| e.uid)
            .filter(|&u| u >= start)
            .collect();
        uids.sort_unstable();
        Ok(uids)
    }

    fn fetch_headers(&mut self, uids: &[u32]) -> Result<Vec<Message>, SourceError> {
        let st = self.state.lock().unwrap();
        Ok(uids
            .iter()
            .filter_map(|&uid| st.entries.iter().find(|e| e.uid == uid))
            .map(|e| {
                Message::from_headers(
                    e.msgid,
                    e.thrid,
                    e.uid,
                    e.labels.iter().cloned().collect(),
                    e.seen,
                    header_block(&e.raw),
                )
            })
            .collect())
    }

    fn fetch_body(&mut self, uid: u32) -> Result<Vec<u8>, SourceError> {
        let st = self.state.lock().unwrap();
        st.entries
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
    ) -> Result<(String, Vec<MessageUpdate>), SourceError> {
        let mut st = self.state.lock().unwrap();
        let found: Vec<u32> = uids
            .iter()
            .copied()
            .filter(|u| st.entries.iter().any(|e| e.uid == *u))
            .collect();
        if found.is_empty() {
            return Err("no messages selected".into());
        }
        let command =
            gmail_imap_proto::labels::uid_store_labels(&found, action, &label_refs(labels))
                .ok_or_else(|| "nothing to change".to_string())?;

        st.highest_modseq += 1;
        let modseq = st.highest_modseq;
        let mut updates = Vec::new();
        for &uid in &found {
            if let Some(e) = st.entries.iter_mut().find(|e| e.uid == uid) {
                match action {
                    LabelAction::Add => e.labels.extend(labels.iter().cloned()),
                    LabelAction::Remove => {
                        for l in labels {
                            e.labels.remove(l);
                        }
                    }
                    LabelAction::Replace => e.labels = labels.iter().cloned().collect(),
                }
                e.modseq = modseq;
                updates.push(Self::update_of(e));
            }
        }
        Ok((command, updates))
    }

    fn list_labels(&mut self) -> Result<Vec<String>, SourceError> {
        Ok(self.all_labels())
    }

    fn supports_condstore(&self) -> bool {
        true
    }

    fn changed_since(&mut self, modseq: u64) -> Result<Vec<MessageUpdate>, SourceError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .entries
            .iter()
            .filter(|e| e.modseq > modseq)
            .map(Self::update_of)
            .collect())
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
    fn sent_time_comes_from_the_date_header() {
        let headers =
            b"From: a@example.com\r\nDate: Tue, 10 Nov 2026 09:30:00 +0000\r\n\r\n".to_vec();
        let m = Message::from_headers(1, 2, 3, vec![], false, headers);
        assert_eq!(m.sent, 1_794_303_000);
        let no_date = Message::from_headers(1, 2, 3, vec![], false, b"From: a\r\n\r\n".to_vec());
        assert_eq!(no_date.sent, 0);
    }

    #[test]
    fn header_block_stops_at_blank_line() {
        let raw = b"From: a\r\nSubject: b\r\n\r\nbody\r\n";
        assert_eq!(header_block(raw), b"From: a\r\nSubject: b\r\n\r\n".to_vec());
    }

    #[test]
    fn changes_made_elsewhere_are_reported_since_a_modseq() {
        let mut src = MockSource::demo();
        let handle = src.handle();
        let before = src.select().unwrap().highest_modseq.unwrap();

        handle.set_labels(4, &["Archive"]);
        handle.set_seen(1, true);

        let changed = src.changed_since(before).unwrap();
        let uids: Vec<u32> = changed.iter().map(|c| c.uid).collect();
        assert_eq!(uids, vec![1, 4], "only the two changed messages");
        let four = changed.iter().find(|c| c.uid == 4).unwrap();
        assert_eq!(four.labels, BTreeSet::from(["Archive".to_string()]));
        assert!(changed.iter().find(|c| c.uid == 1).unwrap().seen);
    }

    #[test]
    fn nothing_changed_since_now() {
        let mut src = MockSource::demo();
        let now = src.select().unwrap().highest_modseq.unwrap();
        assert!(src.changed_since(now).unwrap().is_empty());
    }
}
