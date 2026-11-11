//! Gmail label model on top of the `X-GM-LABELS` IMAP extension.
//!
//! Gmail has no folders. Each message carries a set of labels, and IMAP
//! exposes that set through `X-GM-LABELS`. Modifying the set uses
//! `STORE ... +X-GM-LABELS / -X-GM-LABELS / X-GM-LABELS`.
//!
//! Notes on semantics that the rest of the client depends on:
//! - `\All Mail` is not a label in `X-GM-LABELS`. Every message is in All Mail
//!   implicitly, so nothing is added or removed to "archive" except `\Inbox`.
//! - Archiving is removing `\Inbox`. Trashing is adding `\Trash`.
//! - Labels are per message (`X-GM-MSGID`), not per thread. Thread-level
//!   actions must fan out to every message with the same `X-GM-THRID`.

use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SystemLabel {
    Inbox,
    Sent,
    Draft,
    Starred,
    Important,
    Trash,
    Spam,
}

impl SystemLabel {
    pub const ALL: [SystemLabel; 7] = [
        SystemLabel::Inbox,
        SystemLabel::Sent,
        SystemLabel::Draft,
        SystemLabel::Starred,
        SystemLabel::Important,
        SystemLabel::Trash,
        SystemLabel::Spam,
    ];

    /// Parses the unquoted wire form, e.g. `\Inbox`.
    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.as_wire() == s)
    }

    /// The wire form with a single leading backslash, e.g. `\Inbox`.
    pub fn as_wire(self) -> &'static str {
        match self {
            SystemLabel::Inbox => "\\Inbox",
            SystemLabel::Sent => "\\Sent",
            SystemLabel::Draft => "\\Draft",
            SystemLabel::Starred => "\\Starred",
            SystemLabel::Important => "\\Important",
            SystemLabel::Trash => "\\Trash",
            SystemLabel::Spam => "\\Spam",
        }
    }
}

/// A label as it appears in an `X-GM-LABELS` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Label<'a> {
    System(SystemLabel),
    /// Any other backslash-prefixed label this prototype doesn't model yet.
    Other(&'a str),
    /// A user label. `/` denotes nesting, matching the IMAP mailbox name.
    User(&'a str),
}

impl<'a> Label<'a> {
    pub fn classify(raw: &'a str) -> Self {
        if let Some(sys) = SystemLabel::from_wire(raw) {
            Label::System(sys)
        } else if raw.starts_with('\\') {
            Label::Other(raw)
        } else {
            Label::User(raw)
        }
    }
}

/// A label reference used in a STORE command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelRef<'a> {
    System(SystemLabel),
    Name(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelAction {
    Add,
    Remove,
    /// Replace the full label set. An empty list clears all labels.
    Replace,
}

impl LabelAction {
    fn store_item(self) -> &'static str {
        match self {
            LabelAction::Add => "+X-GM-LABELS",
            LabelAction::Remove => "-X-GM-LABELS",
            LabelAction::Replace => "X-GM-LABELS",
        }
    }
}

/// Builds the body of `UID STORE <uids> <action> (<labels>)`.
///
/// The caller adds the tag and CRLF. UIDs are sent as ranges where possible,
/// which keeps long commands short for large selections. Returns `None` for
/// an empty label list, except `Replace`, which clears all labels.
pub fn uid_store_labels(
    uids: &[u32],
    action: LabelAction,
    labels: &[LabelRef<'_>],
) -> Option<String> {
    if uids.is_empty() {
        return None;
    }
    if labels.is_empty() && action != LabelAction::Replace {
        return None;
    }

    let mut out = String::with_capacity(32 + uids.len() * 6);
    out.push_str("UID STORE ");
    push_uid_set(&mut out, uids);
    out.push(' ');
    out.push_str(action.store_item());
    out.push_str(" (");
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match label {
            LabelRef::System(sys) => push_quoted(&mut out, sys.as_wire()),
            LabelRef::Name(name) => push_quoted(&mut out, name),
        }
    }
    out.push(')');
    Some(out)
}

/// Archive = remove `\Inbox`.
pub fn archive(uids: &[u32]) -> Option<String> {
    uid_store_labels(
        uids,
        LabelAction::Remove,
        &[LabelRef::System(SystemLabel::Inbox)],
    )
}

/// Trash = add `\Trash`.
pub fn trash(uids: &[u32]) -> Option<String> {
    uid_store_labels(
        uids,
        LabelAction::Add,
        &[LabelRef::System(SystemLabel::Trash)],
    )
}

/// Sorts, dedupes and emits UIDs as comma-separated ranges, e.g. `1:3,7,9:10`.
pub fn push_uid_set(out: &mut String, uids: &[u32]) {
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut first = true;
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        if !first {
            out.push(',');
        }
        first = false;
        if start == end {
            let _ = write!(out, "{start}");
        } else {
            let _ = write!(out, "{start}:{end}");
        }
        i += 1;
    }
}

/// Quotes a label name for IMAP. Backslashes and double quotes are escaped.
fn push_quoted(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}
