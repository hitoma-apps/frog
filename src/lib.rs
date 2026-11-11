//! Prototype Gmail IMAP client pieces: `X-GM-*` response parsing, label
//! operations, and plaintext message rendering. No network I/O yet.

pub mod labels;
pub mod render;
pub mod response;

pub use labels::{Label, LabelAction, LabelRef, SystemLabel, uid_store_labels};
pub use render::render_plaintext;
pub use response::{GmailFetch, ParseError, parse_gmail_fetch};
