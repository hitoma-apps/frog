//! Mail backend abstraction. The GUI talks only to `MailBackend`.
//!
//! `MockBackend` keeps messages in memory and builds the same `UID STORE`
//! commands a live IMAP backend would send. A live backend will implement
//! this trait over a real connection.

use std::collections::BTreeSet;

use gmail_imap_proto::labels::{LabelAction, LabelRef, SystemLabel, uid_store_labels};

/// One message as the GUI sees it.
#[derive(Clone, Debug)]
pub struct Message {
    /// `X-GM-MSGID`.
    pub msgid: u64,
    /// `X-GM-THRID`.
    pub thrid: u64,
    /// IMAP UID in the All Mail folder.
    pub uid: u32,
    /// Raw `X-GM-LABELS` names, e.g. `\Inbox` or `Work/Projects`.
    pub labels: BTreeSet<String>,
    pub from: String,
    pub subject: String,
    /// `Message-ID` header without angle brackets, if the message has one.
    pub message_id: Option<String>,
    /// Raw RFC 5322 bytes, rendered by `render_plaintext`.
    pub raw: Vec<u8>,
}

pub trait MailBackend {
    fn messages(&self) -> Vec<Message>;

    /// Applies a label change to the given messages and returns the IMAP
    /// command that performed it, for display.
    fn modify_labels(
        &mut self,
        msgids: &[u64],
        action: LabelAction,
        labels: &[String],
    ) -> Result<String, String>;
}

pub struct MockBackend {
    messages: Vec<Message>,
}

impl MockBackend {
    pub fn demo() -> Self {
        let msg = |msgid, thrid, uid, labels: &[&str], from: &str, subject: &str, body: &str| {
            let raw = format!(
                "From: {from}\r\nTo: me@example.com\r\nSubject: {subject}\r\n\
                 Date: Tue, 10 Nov 2026 09:30:00 +0000\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
            )
            .into_bytes();
            Message {
                msgid,
                thrid,
                uid,
                labels: labels.iter().map(|s| s.to_string()).collect(),
                from: from.to_string(),
                subject: subject.to_string(),
                message_id: Some(format!("{msgid}@mock.example")),
                raw,
            }
        };
        Self {
            messages: vec![
                msg(
                    1001,
                    9001,
                    1,
                    &["\\Inbox", "\\Important", "Work/Projects"],
                    "Ada <ada@example.com>",
                    "Q3 plan",
                    "Here is the draft of the Q3 plan.\nPlease review by Friday.",
                ),
                msg(
                    1002,
                    9001,
                    2,
                    &["\\Inbox", "Work/Projects"],
                    "Charles <charles@example.com>",
                    "Re: Q3 plan",
                    "Looks good. Two comments on the budget section.",
                ),
                msg(
                    1003,
                    9002,
                    3,
                    &["\\Inbox", "\\Starred"],
                    "Shop <orders@example.com>",
                    "Your receipt",
                    "Thanks for your order #4411.",
                ),
                msg(
                    1004,
                    9003,
                    4,
                    &["Personal"],
                    "Mary <mary@example.com>",
                    "Holiday photos",
                    "Uploaded the photos from the lake trip.",
                ),
                msg(
                    1005,
                    9004,
                    5,
                    &["\\Trash"],
                    "News <news@example.com>",
                    "Old newsletter",
                    "This one is in the trash.",
                ),
                msg(
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
}

fn to_label_ref(name: &str) -> LabelRef<'_> {
    match SystemLabel::from_wire(name) {
        Some(sys) => LabelRef::System(sys),
        None => LabelRef::Name(name),
    }
}

impl MailBackend for MockBackend {
    fn messages(&self) -> Vec<Message> {
        self.messages.clone()
    }

    fn modify_labels(
        &mut self,
        msgids: &[u64],
        action: LabelAction,
        labels: &[String],
    ) -> Result<String, String> {
        let targets: Vec<&mut Message> = self
            .messages
            .iter_mut()
            .filter(|m| msgids.contains(&m.msgid))
            .collect();
        if targets.is_empty() {
            return Err("no messages selected".into());
        }

        let uids: Vec<u32> = targets.iter().map(|m| m.uid).collect();
        let refs: Vec<LabelRef<'_>> = labels.iter().map(|l| to_label_ref(l)).collect();
        let command = uid_store_labels(&uids, action, &refs)
            .ok_or_else(|| "nothing to change".to_string())?;

        for m in targets {
            match action {
                LabelAction::Add => m.labels.extend(labels.iter().cloned()),
                LabelAction::Remove => {
                    for l in labels {
                        m.labels.remove(l);
                    }
                }
                LabelAction::Replace => {
                    m.labels = labels.iter().cloned().collect();
                }
            }
        }
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_of(b: &MockBackend, msgid: u64) -> BTreeSet<String> {
        b.messages()
            .into_iter()
            .find(|m| m.msgid == msgid)
            .unwrap()
            .labels
    }

    #[test]
    fn archive_removes_inbox_and_returns_store_command() {
        let mut b = MockBackend::demo();
        let cmd = b
            .modify_labels(&[1003], LabelAction::Remove, &["\\Inbox".to_string()])
            .unwrap();
        assert_eq!(cmd, r#"UID STORE 3 -X-GM-LABELS ("\\Inbox")"#);
        assert!(!labels_of(&b, 1003).contains("\\Inbox"));
        assert!(labels_of(&b, 1003).contains("\\Starred"));
    }

    #[test]
    fn add_user_label_to_multiple_messages() {
        let mut b = MockBackend::demo();
        let cmd = b
            .modify_labels(&[1001, 1002], LabelAction::Add, &["Later".to_string()])
            .unwrap();
        assert_eq!(cmd, r#"UID STORE 1:2 +X-GM-LABELS ("Later")"#);
        assert!(labels_of(&b, 1001).contains("Later"));
        assert!(labels_of(&b, 1002).contains("Later"));
    }

    #[test]
    fn unknown_message_is_an_error() {
        let mut b = MockBackend::demo();
        assert!(
            b.modify_labels(&[9999], LabelAction::Add, &["x".to_string()])
                .is_err()
        );
    }
}
