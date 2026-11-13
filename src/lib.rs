//! Prototype Gmail IMAP client pieces: a small IMAP client, `X-GM-*` response
//! parsing, label operations, and plaintext message rendering.

pub mod imap;
pub mod labels;
pub mod linkcheck;
pub mod links;
pub mod render;
pub mod response;
pub mod store;

pub use labels::{Label, LabelAction, LabelRef, SystemLabel, uid_store_labels};
pub use render::render_plaintext;
pub use response::{GmailFetch, ParseError, parse_gmail_fetch};
