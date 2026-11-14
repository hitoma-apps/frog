//! Local cache for one Gmail account, stored in SQLite.
//!
//! What's cached:
//! - Message headers, keyed by `X-GM-MSGID`, with their UIDs and labels.
//!   A UID is only valid within one UIDVALIDITY, so `invalidate_uids` clears
//!   them when that changes. The messages themselves stay.
//! - Full message bodies, zstd-compressed, in an LRU cache capped by
//!   compressed size (1 GiB by default).
//!
//! There's one file per account, and each file holds one mailbox (All Mail).
//! Plaintext for now. Encryption at rest is planned.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

use crate::uidset::UidSet;

/// Default body cache size, in compressed bytes.
pub const DEFAULT_BODY_CAP: u64 = 1 << 30;

/// zstd level. Mail is text, so a low level compresses well and stays fast.
const ZSTD_LEVEL: i32 = 3;

const SCHEMA_VERSION: i64 = 11;

#[derive(Debug)]
pub enum StoreError {
    Sql(rusqlite::Error),
    Io(std::io::Error),
    /// A value doesn't fit the column type. Gmail IDs are 64-bit, so this
    /// only happens with corrupt input.
    OutOfRange(u64),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Sql(e) => write!(f, "cache database error: {e}"),
            StoreError::Io(e) => write!(f, "cache file error: {e}"),
            StoreError::OutOfRange(v) => write!(f, "value {v} does not fit the cache"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A thread's list row, as the query returns it: thread, count, unread, attachment, head.
type ThreadListRow = (u64, usize, usize, bool, CachedMessage);
/// A thread's counts and attachment flag, without its head message.
type ThreadMeta = (u64, usize, usize, bool);

/// Which threads a list shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadFilter {
    /// Every thread.
    All,
    /// Threads with at least one message carrying this raw label. Whole threads are shown.
    Label(String),
    /// Threads with a message whose sender or subject matches the text.
    Search(String),
}

/// One row of the thread list: a conversation and its newest message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub thrid: u64,
    /// Cached messages in the thread, with a UID.
    pub count: usize,
    /// How many of them are unread.
    pub unread: usize,
    /// Whether any of them has an attachment.
    pub attachments: bool,
    /// The newest message, with its labels.
    pub head: CachedMessage,
}

/// A cached message header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedMessage {
    /// `X-GM-MSGID`.
    pub msgid: u64,
    /// `X-GM-THRID`.
    pub thrid: u64,
    /// UID in the current UIDVALIDITY, or `None` if that has been invalidated.
    pub uid: Option<u32>,
    pub labels: BTreeSet<String>,
    pub from: String,
    /// The sender's address, as the message gives it. Empty if there isn't one.
    pub from_email: String,
    /// Whether the message has an attachment, from its structure. `None` if not known yet.
    /// Listings leave it `None`, and a write with `None` keeps what the cache already has.
    pub attachment: Option<bool>,
    pub subject: String,
    pub date: String,
    /// The sent time (Unix seconds) from the Date header, or 0 if there isn't one.
    pub sent: i64,
    /// The header block as the server sent it. Empty when read from a listing,
    /// which doesn't need it.
    pub headers: Vec<u8>,
    /// Whether the message has been read (`\Seen`).
    pub seen: bool,
}

/// What the cache knows about the mailbox's sync position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxState {
    pub uidvalidity: u32,
    pub highest_uid: Option<u32>,
    /// HIGHESTMODSEQ at the last sync. `None` if the server doesn't do CONDSTORE.
    pub highest_modseq: Option<u64>,
}

pub struct Store {
    conn: Connection,
    body_cap: u64,
    /// Whether the last session closed the cache cleanly. `false` after a crash or kill.
    opened_cleanly: bool,
    /// A second connection that only reads, for the GUI's queries. It never writes, and it
    /// doesn't mark the cache as closed cleanly when it closes.
    read_only: bool,
}

impl Drop for Store {
    fn drop(&mut self) {
        // Marks the cache as closed cleanly, so the next start can skip the checks. Only the
        // writing connection does: a reader closing says nothing about the writer.
        // A thread that's panicking didn't close the cache cleanly, so it isn't marked clean:
        // the next start then runs the checks.
        if !self.read_only && !std::thread::panicking() {
            let _ = self
                .conn
                .execute("UPDATE cache_state SET clean = 1 WHERE id = 1", []);
        }
    }
}

/// Where an account's cache file lives: `<data dir>/accounts/<sanitized email>.sqlite`.
/// Returns `None` if the platform has no data directory.
pub fn account_cache_path(email: &str) -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "gmail-labels-proto")?;
    let safe: String = email
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '@' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(
        dirs.data_local_dir()
            .join("accounts")
            .join(format!("{safe}.sqlite")),
    )
}

/// The sent time (Unix seconds) from a header block's Date line, or 0 if there isn't one.
pub fn sent_timestamp(headers: &[u8]) -> i64 {
    mail_parser::MessageParser::default()
        .parse(headers)
        .and_then(|m| m.date().map(|d| d.to_timestamp()))
        .unwrap_or(0)
}

/// What `Store::check` found. Physical problems mean the file can't be trusted. Logical
/// problems mean the summaries disagree with the messages, and can be rebuilt.
#[derive(Debug, Default)]
pub struct CheckReport {
    pub physical: Vec<String>,
    pub logical: Vec<String>,
}

/// Adds the attachment columns (schema v11) if they're missing, so a rebuild can use them on
/// every path. Does nothing if they're already there.
fn upgrade_attachments(tx: &Transaction) -> Result<()> {
    let has: i64 = tx.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('threads') WHERE name = 'attachments'",
        [],
        |r| r.get(0),
    )?;
    if has > 0 {
        return Ok(());
    }
    tx.execute_batch(
        "ALTER TABLE messages ADD COLUMN attachment INTEGER;
         ALTER TABLE threads ADD COLUMN attachments INTEGER NOT NULL DEFAULT 0;",
    )?;
    Ok(())
}

/// Gives the thread summaries a head message and a date to sort by (schema v10), in place of
/// the UID they had. Does nothing if that's already done, so it's safe on every path.
fn upgrade_threads(tx: &Transaction) -> Result<()> {
    let has_head: i64 = tx.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('threads') WHERE name = 'head_msgid'",
        [],
        |r| r.get(0),
    )?;
    if has_head > 0 {
        return Ok(());
    }
    tx.execute_batch(
        "DROP INDEX IF EXISTS threads_newest;
         ALTER TABLE threads DROP COLUMN newest_uid;
         ALTER TABLE threads ADD COLUMN head_msgid INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE threads ADD COLUMN newest_sent INTEGER NOT NULL DEFAULT 0;
         CREATE INDEX threads_newest ON threads(newest_sent, thrid);",
    )?;
    Ok(())
}

/// Recomputes every summary table from the messages. The search index is not included.
fn rebuild_derived(tx: &Transaction) -> Result<()> {
    tx.execute_batch(
        "DELETE FROM threads;
         DELETE FROM thread_labels;
         DELETE FROM label_threads;
         INSERT INTO threads (thrid, count, unread, head_msgid, newest_sent, attachments)
             SELECT m.thrid, COUNT(*), SUM(m.seen = 0),
                 (SELECT m2.msgid FROM messages m2 WHERE m2.thrid = m.thrid AND m2.uid IS NOT NULL
                  ORDER BY m2.sent DESC, m2.uid DESC LIMIT 1),
                 MAX(m.sent),
                 COUNT(CASE WHEN m.attachment = 1 THEN 1 END)
             FROM messages m WHERE m.uid IS NOT NULL GROUP BY m.thrid;
         INSERT INTO thread_labels (label, thrid)
             SELECT DISTINCT l.label, m.thrid FROM messages m
             JOIN message_labels l ON l.msgid = m.msgid WHERE m.uid IS NOT NULL;
         INSERT INTO label_threads (label, threads, unread)
             SELECT tl.label, COUNT(*), SUM(t.unread > 0) FROM thread_labels tl
             JOIN threads t ON t.thrid = tl.thrid GROUP BY tl.label;
         UPDATE cache_totals SET
             messages = (SELECT COUNT(*) FROM messages WHERE uid IS NOT NULL),
             threads = (SELECT COUNT(*) FROM threads),
             unread_threads = (SELECT COUNT(*) FROM threads WHERE unread > 0);",
    )?;
    Ok(())
}

/// Copies the cache file aside, after folding the write-ahead log into it, so the copy
/// is complete. Done before a migration rewrites the file.
fn back_up(conn: &Connection, path: &Path) -> Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    std::fs::copy(
        path,
        path.with_file_name(format!("{name}.before-migration.bak")),
    )?;
    Ok(())
}

/// Renames a cache that can't be used, with its write-ahead log, so it's kept for
/// inspection and a new cache can take its place.
fn set_aside(path: &Path) -> Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target = path.with_file_name(format!("{name}.damaged-{stamp}"));
    std::fs::rename(path, &target)?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        if side.exists() {
            let _ = std::fs::rename(&side, format!("{}{suffix}", target.display()));
        }
    }
    Ok(())
}

/// The thread of a stored message, by its `X-GM-MSGID`.
fn thread_of(conn: &Connection, msgid: i64) -> Result<Option<u64>> {
    let thrid: Option<i64> = conn
        .query_row(
            "SELECT thrid FROM messages WHERE msgid = ?1",
            params![msgid],
            |r| r.get(0),
        )
        .optional()?;
    Ok(thrid.map(|t| t as u64))
}

/// Recomputes the summaries of the given threads from their cached messages.
/// A thread with no cached UIDs left drops out of the summaries.
/// A thread's share of the label and total counts: its labels, and whether it's unread.
#[derive(Default)]
struct Contribution {
    exists: bool,
    unread: bool,
    labels: Vec<String>,
}

fn thread_contribution(tx: &Transaction, thrid: i64) -> Result<Contribution> {
    let unread: Option<i64> = tx
        .query_row(
            "SELECT unread FROM threads WHERE thrid = ?1",
            params![thrid],
            |r| r.get(0),
        )
        .optional()?;
    let mut stmt = tx.prepare("SELECT label FROM thread_labels WHERE thrid = ?1")?;
    let labels = stmt
        .query_map(params![thrid], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(Contribution {
        exists: unread.is_some(),
        unread: unread.is_some_and(|u| u > 0),
        labels,
    })
}

/// Moves the label and total counts from a thread's old contribution to its new one.
fn apply_contribution_delta(
    tx: &Transaction,
    before: &Contribution,
    after: &Contribution,
) -> Result<()> {
    for label in &before.labels {
        adjust_label_threads(tx, label, -1, -i64::from(before.unread))?;
    }
    for label in &after.labels {
        adjust_label_threads(tx, label, 1, i64::from(after.unread))?;
    }
    let threads = i64::from(after.exists) - i64::from(before.exists);
    let unread = i64::from(after.unread) - i64::from(before.unread);
    if threads != 0 || unread != 0 {
        tx.execute(
            "UPDATE cache_totals SET threads = threads + ?1, unread_threads = unread_threads + ?2
             WHERE id = 1",
            params![threads, unread],
        )?;
    }
    Ok(())
}

fn adjust_label_threads(tx: &Transaction, label: &str, threads: i64, unread: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO label_threads (label, threads, unread) VALUES (?1, ?2, ?3)
         ON CONFLICT(label) DO UPDATE SET
             threads = threads + excluded.threads, unread = unread + excluded.unread",
        params![label, threads, unread],
    )?;
    tx.execute("DELETE FROM label_threads WHERE threads <= 0", [])?;
    Ok(())
}

fn refresh_threads(tx: &Transaction, thrids: &BTreeSet<u64>) -> Result<()> {
    for &thrid in thrids {
        let thrid = to_i64(thrid)?;
        let before = thread_contribution(tx, thrid)?;
        // The head is the newest message by date, as the thread list shows it.
        let (count, unread, head, newest_sent, attachments): (
            i64,
            i64,
            Option<i64>,
            Option<i64>,
            i64,
        ) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(seen = 0), 0),
                 (SELECT msgid FROM messages WHERE thrid = ?1 AND uid IS NOT NULL
                  ORDER BY sent DESC, uid DESC LIMIT 1),
                 MAX(sent),
                 COUNT(CASE WHEN attachment = 1 THEN 1 END)
             FROM messages WHERE thrid = ?1 AND uid IS NOT NULL",
            params![thrid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        tx.execute("DELETE FROM thread_labels WHERE thrid = ?1", params![thrid])?;
        if count == 0 {
            tx.execute("DELETE FROM threads WHERE thrid = ?1", params![thrid])?;
            let after = thread_contribution(tx, thrid)?;
            apply_contribution_delta(tx, &before, &after)?;
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO threads
                 (thrid, count, unread, head_msgid, newest_sent, attachments)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![thrid, count, unread, head, newest_sent, attachments],
        )?;
        tx.execute(
            "INSERT INTO thread_labels (label, thrid)
             SELECT DISTINCT l.label, m.thrid FROM messages m
             JOIN message_labels l ON l.msgid = m.msgid
             WHERE m.thrid = ?1 AND m.uid IS NOT NULL",
            params![thrid],
        )?;
        let after = thread_contribution(tx, thrid)?;
        apply_contribution_delta(tx, &before, &after)?;
    }
    Ok(())
}

/// Turns search text into an FTS5 query. Each word must match the start of a word in
/// the sender or subject. Returns `None` if the text has no words.
pub fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split_whitespace()
        .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// Reads a message row with the columns: msgid, thrid, uid, from, subject, date, seen, sent.
/// Labels are left empty, and the header block is left out.
fn cached_from_row(r: &rusqlite::Row) -> rusqlite::Result<CachedMessage> {
    Ok(CachedMessage {
        msgid: r.get::<_, i64>(0)? as u64,
        thrid: r.get::<_, i64>(1)? as u64,
        uid: r.get(2)?,
        labels: BTreeSet::new(),
        from: r.get(3)?,
        from_email: r.get(8)?,
        attachment: None,
        subject: r.get(4)?,
        date: r.get(5)?,
        headers: Vec::new(),
        seen: r.get(6)?,
        sent: r.get(7)?,
    })
}

/// The sender's address from a header block's From line, lowercased. Empty if there
/// isn't one.
pub fn sender_address(headers: &[u8]) -> String {
    mail_parser::MessageParser::default()
        .parse(headers)
        .and_then(|m| {
            m.from()
                .and_then(|a| a.first())
                .and_then(|a| a.address())
                .map(str::to_lowercase)
        })
        .unwrap_or_default()
}

fn to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| StoreError::OutOfRange(v))
}

impl Store {
    /// Opens or creates the cache file at `path` with the default body cap.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // A cache from an older version is copied before any migration rewrites it.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 0 && version < SCHEMA_VERSION {
            back_up(&conn, path)?;
        }
        Self::init(conn, DEFAULT_BODY_CAP)
    }

    /// Opens a second, read-only connection to a cache the writer has already opened. In WAL
    /// mode it reads alongside the writer, so queries don't wait for writes. It runs no
    /// migrations and writes nothing.
    pub fn open_reader(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self {
            conn,
            body_cap: DEFAULT_BODY_CAP,
            opened_cleanly: true,
            read_only: true,
        })
    }

    /// Opens the cache, checking it first if the last session didn't close it cleanly.
    /// Damage is repaired where it can be, and a cache that can't be read is set aside
    /// (renamed, not deleted) and a new one started. Returns what was done, for the user.
    pub fn open_checked(path: &Path) -> Result<(Self, Vec<String>)> {
        let mut notes = Vec::new();
        let store = match Self::open(path) {
            Ok(store) => store,
            Err(e) => {
                set_aside(path)?;
                notes.push(format!(
                    "The cache could not be opened ({e}). It was set aside, and a new one started."
                ));
                return Ok((Self::open(path)?, notes));
            }
        };
        if store.opened_cleanly {
            return Ok((store, notes));
        }
        let report = match store.check() {
            Ok(report) => report,
            Err(e) => CheckReport {
                physical: vec![e.to_string()],
                logical: Vec::new(),
            },
        };
        if !report.physical.is_empty() {
            drop(store);
            set_aside(path)?;
            notes.push(format!(
                "The cache failed its checks ({}). It was set aside, and a new one started.",
                report.physical.join("; ")
            ));
            return Ok((Self::open(path)?, notes));
        }
        let mut store = store;
        if !report.logical.is_empty() {
            store.repair()?;
            notes.push(format!(
                "The cache's summaries were out of step ({}). They were rebuilt from the messages.",
                report.logical.join("; ")
            ));
        }
        Ok((store, notes))
    }

    /// Whether the last session closed the cache cleanly.
    pub fn opened_cleanly(&self) -> bool {
        self.opened_cleanly
    }

    /// Checks the file and the data. Physical problems mean the file can't be trusted.
    /// Logical problems mean the summaries disagree with the messages, and can be rebuilt.
    pub fn check(&self) -> Result<CheckReport> {
        let mut physical = Vec::new();
        let mut stmt = self.conn.prepare("PRAGMA quick_check")?;
        for line in stmt.query_map([], |r| r.get::<_, String>(0))? {
            let line = line?;
            if line != "ok" {
                physical.push(line);
            }
        }
        let orphans: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
        if orphans > 0 {
            physical.push(format!("{orphans} label rows point at missing messages"));
        }

        let mut logical = self.derived_mismatches()?;
        if let Err(e) = self.conn.execute(
            "INSERT INTO messages_fts (messages_fts) VALUES ('integrity-check')",
            [],
        ) {
            logical.push(format!("search index: {e}"));
        }
        Ok(CheckReport { physical, logical })
    }

    /// Describes each summary that disagrees with the messages it's derived from.
    fn derived_mismatches(&self) -> Result<Vec<String>> {
        // Each pair: what the summary should hold, and what it holds. Rows in one but not
        // the other, in either direction, are mismatches.
        const CHECKS: [(&str, &str, &str); 3] = [
            (
                "thread summaries",
                "SELECT m.thrid, COUNT(*), SUM(m.seen = 0),
                     (SELECT m2.msgid FROM messages m2 WHERE m2.thrid = m.thrid AND m2.uid IS NOT NULL
                      ORDER BY m2.sent DESC, m2.uid DESC LIMIT 1),
                     MAX(m.sent),
                     COUNT(CASE WHEN m.attachment = 1 THEN 1 END)
                 FROM messages m WHERE m.uid IS NOT NULL GROUP BY m.thrid",
                "SELECT thrid, count, unread, head_msgid, newest_sent, attachments FROM threads",
            ),
            (
                "thread labels",
                "SELECT DISTINCT l.label, m.thrid FROM messages m
                 JOIN message_labels l ON l.msgid = m.msgid WHERE m.uid IS NOT NULL",
                "SELECT label, thrid FROM thread_labels",
            ),
            (
                "label counts",
                "SELECT tl.label, COUNT(*), SUM(t.unread > 0) FROM thread_labels tl
                 JOIN threads t ON t.thrid = tl.thrid GROUP BY tl.label",
                "SELECT label, threads, unread FROM label_threads",
            ),
        ];
        let mut problems = Vec::new();
        for (name, expected, actual) in CHECKS {
            let differing: i64 = self.conn.query_row(
                &format!(
                    "SELECT (SELECT COUNT(*) FROM ({expected} EXCEPT {actual}))
                          + (SELECT COUNT(*) FROM ({actual} EXCEPT {expected}))"
                ),
                [],
                |r| r.get(0),
            )?;
            if differing > 0 {
                problems.push(format!("{name}: {differing} rows differ"));
            }
        }
        let totals: i64 = self.conn.query_row(
            "SELECT (SELECT messages FROM cache_totals WHERE id = 1)
                  != (SELECT COUNT(*) FROM messages WHERE uid IS NOT NULL)
              OR (SELECT threads FROM cache_totals WHERE id = 1)
                  != (SELECT COUNT(*) FROM threads)
              OR (SELECT unread_threads FROM cache_totals WHERE id = 1)
                  != (SELECT COUNT(*) FROM threads WHERE unread > 0)",
            [],
            |r| r.get(0),
        )?;
        if totals != 0 {
            problems.push("message or thread totals are wrong".into());
        }
        Ok(problems)
    }

    /// Rebuilds the summaries and the search index from the messages.
    pub fn repair(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        rebuild_derived(&tx)?;
        tx.commit()?;
        self.conn.execute(
            "INSERT INTO messages_fts (messages_fts) VALUES ('rebuild')",
            [],
        )?;
        Ok(())
    }

    /// An in-memory cache, for tests.
    pub fn open_in_memory(body_cap: u64) -> Result<Self> {
        Self::init(Connection::open_in_memory()?, body_cap)
    }

    fn init(conn: Connection, body_cap: u64) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // In WAL mode, NORMAL doesn't sync the disk on every commit. A power cut can lose the
        // last few commits, but never leaves a half-written one, and the next sync re-fetches
        // what was lost. The clean-shutdown checks cover the rest.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // A second writer (an old worker finishing after a restart) makes this one wait,
        // rather than fail at once.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut store = Self {
            conn,
            body_cap,
            opened_cleanly: false,
            read_only: false,
        };
        store.migrate()?;
        store.opened_cleanly = store.mark_open()?;
        Ok(store)
    }

    /// Records that the cache is open. Returns whether it was closed cleanly last time.
    fn mark_open(&self) -> Result<bool> {
        let clean: i64 =
            self.conn
                .query_row("SELECT clean FROM cache_state WHERE id = 1", [], |r| {
                    r.get(0)
                })?;
        self.conn
            .execute("UPDATE cache_state SET clean = 0 WHERE id = 1", [])?;
        Ok(clean == 1)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        if version < 1 {
            tx.execute_batch(
                "CREATE TABLE sync (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 uidvalidity INTEGER NOT NULL,
                 highest_uid INTEGER
             );
             CREATE TABLE messages (
                 msgid INTEGER PRIMARY KEY,
                 thrid INTEGER NOT NULL,
                 uid INTEGER,
                 from_addr TEXT NOT NULL,
                 subject TEXT NOT NULL,
                 date TEXT NOT NULL,
                 headers BLOB NOT NULL
             );
             CREATE INDEX messages_uid ON messages(uid);
             CREATE TABLE message_labels (
                 msgid INTEGER NOT NULL REFERENCES messages(msgid) ON DELETE CASCADE,
                 label TEXT NOT NULL,
                 PRIMARY KEY (msgid, label)
             );
             CREATE INDEX message_labels_by_label ON message_labels(label, msgid);
             CREATE TABLE bodies (
                 msgid INTEGER PRIMARY KEY,
                 data BLOB NOT NULL,
                 raw_size INTEGER NOT NULL,
                 stored_size INTEGER NOT NULL,
                 last_access INTEGER NOT NULL
             );
             CREATE INDEX bodies_lru ON bodies(last_access);",
            )?;
        }
        if version < 2 {
            tx.execute_batch(
                "ALTER TABLE messages ADD COLUMN seen INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE sync ADD COLUMN highest_modseq INTEGER;",
            )?;
        }
        if version < 3 {
            // Per-label totals, kept exact by triggers on message_labels (which
            // also fire for rows removed by the messages cascade).
            tx.execute_batch(
                "CREATE TABLE label_stats (
                 label TEXT PRIMARY KEY,
                 count INTEGER NOT NULL
             );
             INSERT INTO label_stats (label, count)
                 SELECT label, COUNT(*) FROM message_labels GROUP BY label;
             CREATE TRIGGER message_labels_added AFTER INSERT ON message_labels BEGIN
                 INSERT INTO label_stats (label, count) VALUES (NEW.label, 1)
                 ON CONFLICT(label) DO UPDATE SET count = count + 1;
             END;
             CREATE TRIGGER message_labels_removed AFTER DELETE ON message_labels BEGIN
                 UPDATE label_stats SET count = count - 1 WHERE label = OLD.label;
                 DELETE FROM label_stats WHERE label = OLD.label AND count <= 0;
             END;",
            )?;
        }
        if version < 4 {
            // The sent time is stored, so listings don't parse header blocks. Existing
            // rows are filled in once here.
            tx.execute_batch("ALTER TABLE messages ADD COLUMN sent INTEGER NOT NULL DEFAULT 0;")?;
            let rows: Vec<(i64, Vec<u8>)> = {
                let mut stmt = tx.prepare("SELECT msgid, headers FROM messages")?;
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?
            };
            for (msgid, headers) in rows {
                tx.execute(
                    "UPDATE messages SET sent = ?2 WHERE msgid = ?1",
                    params![msgid, sent_timestamp(&headers)],
                )?;
            }
        }
        if version < 5 {
            // Thread summaries, whole-thread label membership, a message total, and
            // full-text search over sender and subject. All are kept in step with the
            // messages by the write methods below and by the triggers here.
            tx.execute_batch(
                "CREATE INDEX messages_thread ON messages(thrid, uid);
                 CREATE TABLE threads (
                     thrid INTEGER PRIMARY KEY,
                     count INTEGER NOT NULL,
                     unread INTEGER NOT NULL,
                     newest_uid INTEGER NOT NULL
                 );
                 CREATE INDEX threads_newest ON threads(newest_uid);
                 CREATE TABLE thread_labels (
                     label TEXT NOT NULL,
                     thrid INTEGER NOT NULL,
                     PRIMARY KEY (label, thrid)
                 );
                 CREATE INDEX thread_labels_by_thread ON thread_labels(thrid);
                 CREATE TABLE cache_totals (
                     id INTEGER PRIMARY KEY CHECK (id = 1),
                     messages INTEGER NOT NULL
                 );
                 INSERT INTO cache_totals (id, messages)
                     SELECT 1, COUNT(*) FROM messages WHERE uid IS NOT NULL;
                 CREATE TRIGGER cached_messages_added AFTER INSERT ON messages
                     WHEN NEW.uid IS NOT NULL BEGIN
                     UPDATE cache_totals SET messages = messages + 1;
                 END;
                 CREATE TRIGGER cached_messages_removed AFTER DELETE ON messages
                     WHEN OLD.uid IS NOT NULL BEGIN
                     UPDATE cache_totals SET messages = messages - 1;
                 END;
                 CREATE TRIGGER cached_messages_uid AFTER UPDATE OF uid ON messages
                     WHEN (OLD.uid IS NULL) != (NEW.uid IS NULL) BEGIN
                     UPDATE cache_totals
                         SET messages = messages + (CASE WHEN NEW.uid IS NULL THEN -1 ELSE 1 END);
                 END;
                 CREATE VIRTUAL TABLE messages_fts USING fts5(
                     from_addr, subject, content='messages', content_rowid='msgid'
                 );
                 CREATE TRIGGER messages_fts_added AFTER INSERT ON messages BEGIN
                     INSERT INTO messages_fts (rowid, from_addr, subject)
                         VALUES (NEW.msgid, NEW.from_addr, NEW.subject);
                 END;
                 CREATE TRIGGER messages_fts_removed AFTER DELETE ON messages BEGIN
                     INSERT INTO messages_fts (messages_fts, rowid, from_addr, subject)
                         VALUES ('delete', OLD.msgid, OLD.from_addr, OLD.subject);
                 END;
                 CREATE TRIGGER messages_fts_changed AFTER UPDATE OF from_addr, subject ON messages
                 BEGIN
                     INSERT INTO messages_fts (messages_fts, rowid, from_addr, subject)
                         VALUES ('delete', OLD.msgid, OLD.from_addr, OLD.subject);
                     INSERT INTO messages_fts (rowid, from_addr, subject)
                         VALUES (NEW.msgid, NEW.from_addr, NEW.subject);
                 END;
                 INSERT INTO messages_fts (messages_fts) VALUES ('rebuild');
                 INSERT INTO threads (thrid, count, unread, newest_uid)
                     SELECT thrid, COUNT(*), SUM(seen = 0), MAX(uid) FROM messages
                     WHERE uid IS NOT NULL GROUP BY thrid;
                 INSERT INTO thread_labels (label, thrid)
                     SELECT DISTINCT l.label, m.thrid FROM messages m
                     JOIN message_labels l ON l.msgid = m.msgid WHERE m.uid IS NOT NULL;",
            )?;
        }
        if version < 6 {
            // The sender's address, filled in from the stored headers once.
            tx.execute_batch(
                "ALTER TABLE messages ADD COLUMN from_email TEXT NOT NULL DEFAULT '';",
            )?;
            let rows: Vec<(i64, Vec<u8>)> = {
                let mut stmt = tx.prepare("SELECT msgid, headers FROM messages")?;
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?
            };
            for (msgid, headers) in rows {
                tx.execute(
                    "UPDATE messages SET from_email = ?2 WHERE msgid = ?1",
                    params![msgid, sender_address(&headers)],
                )?;
            }
        }
        if version < 7 {
            // Duplicate UIDs can't be told apart, so only the oldest keeps its UID. The
            // others are re-fetched by the next sync. Then the UID index rejects duplicates.
            tx.execute_batch(
                "UPDATE messages SET uid = NULL WHERE uid IS NOT NULL
                     AND msgid NOT IN (SELECT MIN(msgid) FROM messages
                                       WHERE uid IS NOT NULL GROUP BY uid);
                 DROP INDEX messages_uid;
                 CREATE UNIQUE INDEX messages_uid_unique ON messages(uid) WHERE uid IS NOT NULL;
                 CREATE TABLE cache_state (
                     id INTEGER PRIMARY KEY CHECK (id = 1),
                     clean INTEGER NOT NULL
                 );
                 INSERT INTO cache_state (id, clean) VALUES (1, 0);",
            )?;
            // Counts per label are threads, and unread threads (see v9).
            tx.execute_batch(
                "CREATE TABLE label_threads (
                     label TEXT PRIMARY KEY,
                     threads INTEGER NOT NULL,
                     unread INTEGER NOT NULL
                 );
                 ALTER TABLE cache_totals ADD COLUMN threads INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE cache_totals ADD COLUMN unread_threads INTEGER NOT NULL DEFAULT 0;",
            )?;
            upgrade_threads(&tx)?;
            upgrade_attachments(&tx)?;
            // The UID fix above can change the summaries, so they're rebuilt.
            rebuild_derived(&tx)?;
        }
        if version < 8 {
            // Read-state changes made here that the server hasn't confirmed yet.
            tx.execute_batch(
                "CREATE TABLE pending_seen (
                     msgid INTEGER PRIMARY KEY REFERENCES messages(msgid) ON DELETE CASCADE,
                     seen INTEGER NOT NULL
                 );",
            )?;
        }
        if version < 9 {
            // Label counts are threads now (label_threads), so the per-message counts go.
            tx.execute_batch(
                "DROP TRIGGER IF EXISTS message_labels_added;
                 DROP TRIGGER IF EXISTS message_labels_removed;
                 DROP TABLE IF EXISTS label_stats;",
            )?;
        }
        if version < 11 {
            upgrade_attachments(&tx)?;
            rebuild_derived(&tx)?;
        }
        if version < 10 {
            // Threads sort by their newest message's date, not by UID (see upgrade_threads).
            upgrade_threads(&tx)?;
            rebuild_derived(&tx)?;
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    }

    /// The sync position, or `None` if nothing has been cached yet.
    pub fn mailbox_state(&self) -> Result<Option<MailboxState>> {
        let state = self
            .conn
            .query_row(
                "SELECT uidvalidity, highest_uid, highest_modseq FROM sync WHERE id = 1",
                [],
                |r| {
                    Ok(MailboxState {
                        uidvalidity: r.get(0)?,
                        highest_uid: r.get(1)?,
                        highest_modseq: r.get::<_, Option<i64>>(2)?.map(|m| m as u64),
                    })
                },
            )
            .optional()?;
        Ok(state)
    }

    pub fn set_mailbox_state(&self, state: MailboxState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync (id, uidvalidity, highest_uid, highest_modseq) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET uidvalidity = excluded.uidvalidity,
                                           highest_uid = excluded.highest_uid,
                                           highest_modseq = excluded.highest_modseq",
            params![
                state.uidvalidity,
                state.highest_uid,
                state.highest_modseq.map(|m| m as i64)
            ],
        )?;
        Ok(())
    }

    /// Inserts or updates headers and labels. Labels are replaced, not merged.
    pub fn upsert_messages(&mut self, msgs: &[CachedMessage]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let mut affected: BTreeSet<u64> = BTreeSet::new();
        for m in msgs {
            let msgid = to_i64(m.msgid)?;
            if let Some(old) = thread_of(&tx, msgid)? {
                affected.insert(old);
            }
            affected.insert(m.thrid);
            tx.execute(
                "INSERT INTO messages (msgid, thrid, uid, from_addr, subject, date, headers, seen, sent, from_email, attachment)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(msgid) DO UPDATE SET
                     thrid = excluded.thrid, uid = excluded.uid,
                     from_addr = excluded.from_addr, subject = excluded.subject,
                     date = excluded.date, headers = excluded.headers,
                     seen = excluded.seen, sent = excluded.sent,
                     from_email = excluded.from_email,
                     attachment = COALESCE(excluded.attachment, messages.attachment)",
                params![
                    msgid,
                    to_i64(m.thrid)?,
                    m.uid,
                    m.from,
                    m.subject,
                    m.date,
                    m.headers,
                    m.seen,
                    m.sent,
                    m.from_email,
                    m.attachment
                ],
            )?;
            tx.execute(
                "DELETE FROM message_labels WHERE msgid = ?1",
                params![msgid],
            )?;
            for label in &m.labels {
                tx.execute(
                    "INSERT INTO message_labels (msgid, label) VALUES (?1, ?2)",
                    params![msgid, label],
                )?;
            }
        }
        refresh_threads(&tx, &affected)?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces one message's labels, for example after a server-confirmed change.
    pub fn set_labels(&mut self, msgid: u64, labels: &BTreeSet<String>) -> Result<()> {
        let msgid = to_i64(msgid)?;
        let tx = self.conn.transaction()?;
        let thread = thread_of(&tx, msgid)?;
        tx.execute(
            "DELETE FROM message_labels WHERE msgid = ?1",
            params![msgid],
        )?;
        for label in labels {
            tx.execute(
                "INSERT INTO message_labels (msgid, label) VALUES (?1, ?2)",
                params![msgid, label],
            )?;
        }
        if let Some(t) = thread {
            refresh_threads(&tx, &BTreeSet::from([t]))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Applies labels and read state reported by the server, by UID, in one transaction, and
    /// refreshes each affected thread once. A read-state change still waiting to reach the
    /// server is kept, as in `set_seen`. UIDs that aren't cached are skipped.
    pub fn apply_updates(&mut self, updates: &[(u32, &BTreeSet<String>, bool)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let mut affected: BTreeSet<u64> = BTreeSet::new();
        {
            let mut find = tx.prepare_cached("SELECT msgid, thrid FROM messages WHERE uid = ?1")?;
            let mut clear = tx.prepare_cached("DELETE FROM message_labels WHERE msgid = ?1")?;
            let mut add =
                tx.prepare_cached("INSERT INTO message_labels (msgid, label) VALUES (?1, ?2)")?;
            let mut seen_stmt = tx.prepare_cached(
                "UPDATE messages SET seen = ?2 WHERE msgid = ?1
                   AND msgid NOT IN (SELECT msgid FROM pending_seen)",
            )?;
            for &(uid, labels, seen) in updates {
                let row: Option<(i64, i64)> = find
                    .query_row(params![uid], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?;
                let Some((msgid, thrid)) = row else {
                    continue;
                };
                affected.insert(thrid as u64);
                clear.execute(params![msgid])?;
                for label in labels {
                    add.execute(params![msgid, label])?;
                }
                seen_stmt.execute(params![msgid, seen])?;
            }
        }
        refresh_threads(&tx, &affected)?;
        tx.commit()?;
        Ok(())
    }

    /// Records whether a message has been read.
    pub fn set_seen(&mut self, msgid: u64, seen: bool) -> Result<()> {
        let msgid = to_i64(msgid)?;
        let tx = self.conn.transaction()?;
        let thread = thread_of(&tx, msgid)?;
        // A change queued for the server stays as the user made it, until it's confirmed.
        tx.execute(
            "UPDATE messages SET seen = ?2 WHERE msgid = ?1
               AND msgid NOT IN (SELECT msgid FROM pending_seen)",
            params![msgid, seen],
        )?;
        if let Some(t) = thread {
            refresh_threads(&tx, &BTreeSet::from([t]))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// For each label: how many threads have it, and how many of those are unread.
    pub fn label_thread_counts(&self) -> Result<Vec<(String, usize, usize)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT label, threads, unread FROM label_threads ORDER BY label")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, i64>(2)? as usize,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Threads in the cache, and how many of them are unread.
    pub fn thread_totals(&self) -> Result<(usize, usize)> {
        let (threads, unread): (i64, i64) = self.conn.query_row(
            "SELECT threads, unread_threads FROM cache_totals WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((threads as usize, unread as usize))
    }

    /// Which threads a list shows.
    pub fn thread_count(&self, filter: &ThreadFilter) -> Result<usize> {
        let n: i64 = match filter {
            ThreadFilter::All => self
                .conn
                .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))?,
            ThreadFilter::Label(label) => self.conn.query_row(
                "SELECT COUNT(*) FROM thread_labels WHERE label = ?1",
                params![label],
                |r| r.get(0),
            )?,
            ThreadFilter::Search(text) => match fts_query(text) {
                None => 0,
                Some(query) => self.conn.query_row(
                    "SELECT COUNT(DISTINCT m.thrid) FROM messages_fts
                     JOIN messages m ON m.msgid = messages_fts.rowid
                     WHERE messages_fts MATCH ?1 AND m.uid IS NOT NULL",
                    params![query],
                    |r| r.get(0),
                )?,
            },
        };
        Ok(n as usize)
    }

    /// The threads a list shows, newest first, from `offset` on. Each comes with its
    /// newest message. A label or search shows whole threads: every message of a
    /// thread counts, whichever labels it carries.
    pub fn thread_page(
        &self,
        filter: &ThreadFilter,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ThreadSummary>> {
        const COLUMNS: &str = "t.thrid, t.count, t.unread, m.msgid, m.thrid, m.uid,
                               m.from_addr, m.subject, m.date, m.seen, m.sent, m.from_email,
                               t.attachments";
        let (limit, offset) = (limit as i64, offset as i64);
        let rows: Vec<ThreadListRow> = match filter {
            ThreadFilter::All => {
                let sql = format!(
                    "SELECT {COLUMNS} FROM threads t
                     JOIN messages m ON m.msgid = t.head_msgid
                     ORDER BY t.newest_sent DESC, t.thrid DESC LIMIT ?1 OFFSET ?2"
                );
                self.thread_rows(&sql, params![limit, offset])?
            }
            ThreadFilter::Label(label) => {
                // Walk the threads newest first, and stop once a window is full. A
                // label that most threads carry fills its window at once.
                let sql = format!(
                    "SELECT {COLUMNS} FROM threads t
                     JOIN messages m ON m.msgid = t.head_msgid
                     WHERE EXISTS (SELECT 1 FROM thread_labels tl
                                   WHERE tl.label = ?1 AND tl.thrid = t.thrid)
                     ORDER BY t.newest_sent DESC, t.thrid DESC LIMIT ?2 OFFSET ?3"
                );
                self.thread_rows(&sql, params![label, limit, offset])?
            }
            ThreadFilter::Search(text) => match fts_query(text) {
                None => Vec::new(),
                Some(query) => {
                    let sql = format!(
                        "WITH hits AS (
                             SELECT DISTINCT m.thrid FROM messages_fts
                             JOIN messages m ON m.msgid = messages_fts.rowid
                             WHERE messages_fts MATCH ?1 AND m.uid IS NOT NULL)
                         SELECT {COLUMNS} FROM hits h
                         JOIN threads t ON t.thrid = h.thrid
                         JOIN messages m ON m.msgid = t.head_msgid
                         ORDER BY t.newest_sent DESC, t.thrid DESC LIMIT ?2 OFFSET ?3"
                    );
                    self.thread_rows(&sql, params![query, limit, offset])?
                }
            },
        };
        let (meta, mut heads): (Vec<ThreadMeta>, Vec<CachedMessage>) = rows
            .into_iter()
            .map(|(t, c, u, a, m)| ((t, c, u, a), m))
            .unzip();
        self.attach_labels(&mut heads)?;
        Ok(meta
            .into_iter()
            .zip(heads)
            .map(
                |((thrid, count, unread, attachments), head)| ThreadSummary {
                    thrid,
                    count,
                    unread,
                    attachments,
                    head,
                },
            )
            .collect())
    }

    /// Runs a thread-list query. Each row is the thread's summary and its newest message.
    fn thread_rows(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<ThreadListRow>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params, |r| {
                let head = CachedMessage {
                    msgid: r.get::<_, i64>(3)? as u64,
                    thrid: r.get::<_, i64>(4)? as u64,
                    uid: r.get(5)?,
                    labels: BTreeSet::new(),
                    from: r.get(6)?,
                    from_email: r.get(11)?,
                    attachment: None,
                    subject: r.get(7)?,
                    date: r.get(8)?,
                    headers: Vec::new(),
                    seen: r.get(9)?,
                    sent: r.get(10)?,
                };
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, i64>(2)? as usize,
                    r.get::<_, i64>(12)? > 0,
                    head,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every cached message of one thread, newest first, with labels.
    pub fn thread_messages(&self, thrid: u64) -> Result<Vec<CachedMessage>> {
        // The header blocks are read too: a conversation is read in full, and they carry
        // the authentication results.
        let mut stmt = self.conn.prepare_cached(
            "SELECT msgid, thrid, uid, from_addr, subject, date, seen, sent, from_email, headers FROM messages
             WHERE thrid = ?1 AND uid IS NOT NULL ORDER BY uid DESC",
        )?;
        let mut out = stmt
            .query_map(params![to_i64(thrid)?], |r| {
                let mut m = cached_from_row(r)?;
                m.headers = r.get(9)?;
                Ok(m)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.attach_labels(&mut out)?;
        Ok(out)
    }

    /// The UIDs of every cached message in the given threads.
    pub fn uids_for_threads(&self, thrids: &[u64]) -> Result<Vec<u32>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT uid FROM messages WHERE thrid = ?1 AND uid IS NOT NULL")?;
        let mut out = Vec::new();
        for &thrid in thrids {
            for uid in stmt.query_map(params![to_i64(thrid)?], |r| r.get::<_, u32>(0))? {
                out.push(uid?);
            }
        }
        Ok(out)
    }

    /// Marks every cached message in the threads read or unread, and queues the change for
    /// the server. The change is kept until the server confirms it.
    pub fn set_seen_for_threads(&mut self, thrids: &[u64], seen: bool) -> Result<()> {
        let tx = self.conn.transaction()?;
        for &thrid in thrids {
            let thrid = to_i64(thrid)?;
            tx.execute(
                "UPDATE messages SET seen = ?2 WHERE thrid = ?1 AND uid IS NOT NULL",
                params![thrid, seen],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO pending_seen (msgid, seen)
                 SELECT msgid, ?2 FROM messages WHERE thrid = ?1 AND uid IS NOT NULL",
                params![thrid, seen],
            )?;
            refresh_threads(&tx, &BTreeSet::from([thrid as u64]))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Queued read-state changes that can be sent now: (msgid, UID, seen). A change for a
    /// message with no UID waits until the next sync gives it one.
    pub fn pending_seen(&self) -> Result<Vec<(u64, u32, bool)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.msgid, m.uid, p.seen FROM pending_seen p
             JOIN messages m ON m.msgid = p.msgid WHERE m.uid IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, u32>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Whether any queued read-state change can be sent now.
    pub fn has_pending_seen(&self) -> Result<bool> {
        let any: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pending_seen p
                            JOIN messages m ON m.msgid = p.msgid WHERE m.uid IS NOT NULL)",
            [],
            |r| r.get(0),
        )?;
        Ok(any)
    }

    /// Removes confirmed (or refused) changes from the queue.
    pub fn clear_pending_seen(&mut self, msgids: &[u64]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for &msgid in msgids {
            tx.execute(
                "DELETE FROM pending_seen WHERE msgid = ?1",
                params![to_i64(msgid)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// How many messages are cached with a UID. Kept as a running total.
    pub fn message_total(&self) -> Result<usize> {
        let n: i64 =
            self.conn
                .query_row("SELECT messages FROM cache_totals WHERE id = 1", [], |r| {
                    r.get(0)
                })?;
        Ok(n as usize)
    }

    /// One page of cached headers with UID below `before` (or the newest, if
    /// `None`), newest first. Walking pages down the cache lists all of it.
    pub fn list_page(&self, before: Option<u32>, limit: usize) -> Result<Vec<CachedMessage>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT msgid, thrid, uid, from_addr, subject, date, seen, sent, from_email FROM messages
             WHERE uid IS NOT NULL AND (?1 IS NULL OR uid < ?1)
             ORDER BY uid DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![before, limit as i64], |r| {
            Ok(CachedMessage {
                msgid: r.get::<_, i64>(0)? as u64,
                thrid: r.get::<_, i64>(1)? as u64,
                uid: r.get(2)?,
                labels: BTreeSet::new(),
                from: r.get(3)?,
                from_email: r.get(8)?,
                attachment: None,
                subject: r.get(4)?,
                date: r.get(5)?,
                headers: Vec::new(),
                seen: r.get(6)?,
                sent: r.get(7)?,
            })
        })?;
        let mut out = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        self.attach_labels(&mut out)?;
        Ok(out)
    }

    /// UIDs that have a cached header, newest first.
    pub fn cached_uids(&self) -> Result<Vec<u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT uid FROM messages WHERE uid IS NOT NULL ORDER BY uid DESC")?;
        let uids = stmt
            .query_map([], |r| r.get::<_, u32>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(uids)
    }

    /// Records whether messages have attachments, by UID, and refreshes their threads.
    pub fn set_attachments(&mut self, flags: &[(u32, bool)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let mut affected: BTreeSet<u64> = BTreeSet::new();
        for &(uid, has) in flags {
            let thread: Option<i64> = tx
                .query_row(
                    "SELECT thrid FROM messages WHERE uid = ?1",
                    params![uid],
                    |r| r.get(0),
                )
                .optional()?;
            affected.extend(thread.map(|t| t as u64));
            tx.execute(
                "UPDATE messages SET attachment = ?2 WHERE uid = ?1",
                params![uid, has],
            )?;
        }
        refresh_threads(&tx, &affected)?;
        tx.commit()?;
        Ok(())
    }

    /// UIDs of cached messages whose attachments haven't been looked at yet.
    pub fn uids_missing_attachment_info(&self) -> Result<Vec<u32>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT uid FROM messages WHERE uid IS NOT NULL AND attachment IS NULL",
        )?;
        let uids = stmt
            .query_map([], |r| r.get::<_, u32>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(uids)
    }

    /// The UIDs that have a cached header, as a set.
    pub fn cached_uid_set(&self) -> Result<UidSet> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT uid FROM messages WHERE uid IS NOT NULL")?;
        let mut set = UidSet::new();
        for uid in stmt.query_map([], |r| r.get::<_, u32>(0))? {
            set.insert(uid?);
        }
        Ok(set)
    }

    /// The message ID for a UID in the current UIDVALIDITY, if cached.
    pub fn msgid_for_uid(&self, uid: u32) -> Result<Option<u64>> {
        let msgid: Option<i64> = self
            .conn
            .query_row(
                "SELECT msgid FROM messages WHERE uid = ?1",
                params![uid],
                |r| r.get(0),
            )
            .optional()?;
        Ok(msgid.map(|m| m as u64))
    }

    /// Removes cached messages by UID, for messages deleted on the server.
    pub fn remove_uids(&mut self, uids: &[u32]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let mut affected: BTreeSet<u64> = BTreeSet::new();
        for uid in uids {
            let thread: Option<i64> = tx
                .query_row(
                    "SELECT thrid FROM messages WHERE uid = ?1",
                    params![uid],
                    |r| r.get(0),
                )
                .optional()?;
            affected.extend(thread.map(|t| t as u64));
            tx.execute("DELETE FROM messages WHERE uid = ?1", params![uid])?;
        }
        refresh_threads(&tx, &affected)?;
        tx.commit()?;
        Ok(())
    }

    /// Clears every UID. Call this when UIDVALIDITY changes. The messages
    /// themselves are kept, and the next sync assigns new UIDs. Threads have no
    /// cached UIDs until the sync assigns them, so their summaries are cleared too.
    pub fn invalidate_uids(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE messages SET uid = NULL", [])?;
        tx.execute("DELETE FROM sync", [])?;
        tx.execute("DELETE FROM threads", [])?;
        tx.execute("DELETE FROM thread_labels", [])?;
        tx.execute("DELETE FROM label_threads", [])?;
        tx.execute(
            "UPDATE cache_totals SET threads = 0, unread_threads = 0 WHERE id = 1",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Cached headers, newest first, optionally limited to one label.
    /// Messages with no UID (after a UIDVALIDITY change) are left out.
    pub fn list_messages(&self, label: Option<&str>, limit: usize) -> Result<Vec<CachedMessage>> {
        let sql = match label {
            None => {
                "SELECT msgid, thrid, uid, from_addr, subject, date, seen, sent, from_email FROM messages
                 WHERE uid IS NOT NULL ORDER BY uid DESC LIMIT ?2"
            }
            Some(_) => {
                "SELECT msgid, thrid, uid, from_addr, subject, date, seen, sent, from_email FROM messages
                 WHERE uid IS NOT NULL
                   AND EXISTS (SELECT 1 FROM message_labels l
                               WHERE l.msgid = messages.msgid AND l.label = ?1)
                 ORDER BY uid DESC LIMIT ?2"
            }
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params![label, limit as i64], |r| {
            Ok(CachedMessage {
                msgid: r.get::<_, i64>(0)? as u64,
                thrid: r.get::<_, i64>(1)? as u64,
                uid: r.get(2)?,
                labels: BTreeSet::new(),
                from: r.get(3)?,
                from_email: r.get(8)?,
                attachment: None,
                subject: r.get(4)?,
                date: r.get(5)?,
                headers: Vec::new(),
                seen: r.get(6)?,
                sent: r.get(7)?,
            })
        })?;
        let mut out = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        self.attach_labels(&mut out)?;
        Ok(out)
    }

    /// Fills in the labels of a page of messages with one query, rather than one
    /// query per message.
    fn attach_labels(&self, msgs: &mut [CachedMessage]) -> Result<()> {
        if msgs.is_empty() {
            return Ok(());
        }
        // Message IDs are stored as i64 (checked on write), so these are plain integers.
        let ids: Vec<String> = msgs.iter().map(|m| (m.msgid as i64).to_string()).collect();
        let json = format!("[{}]", ids.join(","));
        let mut stmt = self.conn.prepare_cached(
            "SELECT msgid, label FROM message_labels
             WHERE msgid IN (SELECT value FROM json_each(?1))",
        )?;
        let mut by_msg: HashMap<i64, BTreeSet<String>> = HashMap::new();
        let rows = stmt.query_map(params![json], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (msgid, label) = row?;
            by_msg.entry(msgid).or_default().insert(label);
        }
        for m in msgs.iter_mut() {
            m.labels = by_msg.remove(&(m.msgid as i64)).unwrap_or_default();
        }
        Ok(())
    }

    /// Returns the full message if it's cached, and marks it recently used.
    pub fn get_body(&mut self, msgid: u64) -> Result<Option<Vec<u8>>> {
        let msgid = to_i64(msgid)?;
        let data: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT data FROM bodies WHERE msgid = ?1",
                params![msgid],
                |r| r.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            return Ok(None);
        };
        self.conn.execute(
            "UPDATE bodies SET last_access = (SELECT COALESCE(MAX(last_access), 0) + 1 FROM bodies)
             WHERE msgid = ?1",
            params![msgid],
        )?;
        Ok(Some(zstd::decode_all(&data[..])?))
    }

    /// Caches a full message, then evicts least recently used bodies until the
    /// cache is within its cap.
    pub fn put_body(&mut self, msgid: u64, raw: &[u8]) -> Result<()> {
        self.put_bodies(&[(msgid, raw)])
    }

    /// Stores several bodies in one transaction, then evicts the least recently used
    /// bodies if the cache is over its cap.
    pub fn put_bodies(&mut self, bodies: &[(u64, &[u8])]) -> Result<()> {
        // Compress before the transaction opens, so the write lock is held only briefly.
        let mut encoded = Vec::with_capacity(bodies.len());
        for &(msgid, raw) in bodies {
            encoded.push((
                to_i64(msgid)?,
                zstd::encode_all(raw, ZSTD_LEVEL)?,
                raw.len() as i64,
            ));
        }
        let tx = self.conn.transaction()?;
        for (msgid, data, raw_size) in &encoded {
            tx.execute(
                "INSERT INTO bodies (msgid, data, raw_size, stored_size, last_access)
                 VALUES (?1, ?2, ?3, ?4, (SELECT COALESCE(MAX(last_access), 0) + 1 FROM bodies))
                 ON CONFLICT(msgid) DO UPDATE SET
                     data = excluded.data, raw_size = excluded.raw_size,
                     stored_size = excluded.stored_size, last_access = excluded.last_access",
                params![msgid, data, raw_size, data.len() as i64],
            )?;
        }
        // Over the cap, the least recently used bodies go. The total is read once, and each
        // eviction takes its own size off it, rather than summing the table again.
        let mut total: i64 = tx.query_row(
            "SELECT COALESCE(SUM(stored_size), 0) FROM bodies",
            [],
            |r| r.get(0),
        )?;
        while total.max(0) as u64 > self.body_cap {
            let oldest: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT msgid, stored_size FROM bodies ORDER BY last_access ASC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((msgid, size)) = oldest else {
                break;
            };
            tx.execute("DELETE FROM bodies WHERE msgid = ?1", params![msgid])?;
            total -= size;
        }
        tx.commit()?;
        Ok(())
    }

    /// Total compressed bytes of cached bodies.
    pub fn body_bytes(&self) -> Result<u64> {
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(stored_size), 0) FROM bodies",
            [],
            |r| r.get(0),
        )?;
        Ok(total as u64)
    }

    /// Drops all cached bodies. Headers and sync state are kept.
    pub fn clear_bodies(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM bodies", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(msgid: u64, uid: u32, labels: &[&str]) -> CachedMessage {
        CachedMessage {
            msgid,
            thrid: msgid + 100,
            uid: Some(uid),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            from: format!("sender{uid}@example.com"),
            subject: format!("Hello {uid}"),
            date: "Tue, 10 Nov 2026 09:30:00 +0000".into(),
            sent: 1_794_303_000 + i64::from(uid),
            from_email: format!("sender{uid}@example.com"),
            attachment: None,
            headers: format!("Subject: Hello {uid}\r\n\r\n").into_bytes(),
            seen: false,
        }
    }

    /// Pseudo-random bytes. They don't compress, so body size is predictable.
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (0..len)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 33) as u8
            })
            .collect()
    }

    #[test]
    fn headers_round_trip_newest_first_with_labels() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[
            msg(1001, 1, &["\\Inbox", "Work"]),
            msg(1002, 2, &["\\Inbox"]),
        ])
        .unwrap();
        let listed = s.list_messages(None, 10).unwrap();
        assert_eq!(
            listed.iter().map(|m| m.uid).collect::<Vec<_>>(),
            vec![Some(2), Some(1)]
        );
        assert_eq!(listed[1].labels.len(), 2);
        assert!(listed[1].labels.contains("Work"));
        assert!(
            listed[0].headers.is_empty(),
            "listings leave out the header block"
        );
    }

    #[test]
    fn label_filter_and_limit() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[
            msg(1, 1, &["Work"]),
            msg(2, 2, &["Personal"]),
            msg(3, 3, &["Work", "\\Starred"]),
        ])
        .unwrap();
        let work: Vec<u64> = s
            .list_messages(Some("Work"), 10)
            .unwrap()
            .iter()
            .map(|m| m.msgid)
            .collect();
        assert_eq!(work, vec![3, 1]);
        assert_eq!(s.list_messages(None, 2).unwrap().len(), 2);
    }

    #[test]
    fn set_labels_replaces_rather_than_merges() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[msg(7, 7, &["\\Inbox", "Work"])])
            .unwrap();
        s.set_labels(7, &BTreeSet::from(["Archive".to_string()]))
            .unwrap();
        let m = &s.list_messages(None, 1).unwrap()[0];
        assert_eq!(m.labels, BTreeSet::from(["Archive".to_string()]));
    }

    #[test]
    fn upsert_replaces_labels_from_the_server() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[msg(7, 7, &["\\Inbox"])]).unwrap();
        s.upsert_messages(&[msg(7, 7, &["Work"])]).unwrap();
        let m = &s.list_messages(None, 1).unwrap()[0];
        assert_eq!(m.labels, BTreeSet::from(["Work".to_string()]));
    }

    #[test]
    fn invalidating_uids_keeps_messages_but_clears_uids() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.set_mailbox_state(MailboxState {
            uidvalidity: 5,
            highest_uid: Some(9),
            highest_modseq: None,
        })
        .unwrap();
        s.upsert_messages(&[msg(1, 1, &["\\Inbox"])]).unwrap();
        s.invalidate_uids().unwrap();
        assert_eq!(s.mailbox_state().unwrap(), None);
        // Without a UID a message can't be acted on, so it's left out of listings.
        assert!(s.list_messages(None, 10).unwrap().is_empty());
        assert_eq!(s.msgid_for_uid(1).unwrap(), None);
        // The next sync assigns a UID again and the message comes back.
        s.upsert_messages(&[msg(1, 4, &["\\Inbox"])]).unwrap();
        assert_eq!(s.list_messages(None, 10).unwrap().len(), 1);
    }

    #[test]
    fn cached_uids_and_msgid_lookup() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[msg(1001, 1, &[]), msg(1003, 3, &[]), msg(1002, 2, &[])])
            .unwrap();
        assert_eq!(s.cached_uids().unwrap(), vec![3, 2, 1]);
        assert_eq!(s.msgid_for_uid(2).unwrap(), Some(1002));
        assert_eq!(s.msgid_for_uid(99).unwrap(), None);
        s.invalidate_uids().unwrap();
        assert!(s.cached_uids().unwrap().is_empty());
    }

    #[test]
    fn remove_uids_drops_messages_and_their_labels() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[msg(1, 1, &["Work"]), msg(2, 2, &["Work"])])
            .unwrap();
        s.remove_uids(&[1]).unwrap();
        assert_eq!(s.cached_uids().unwrap(), vec![2]);
        let work = s.list_messages(Some("Work"), 10).unwrap();
        assert_eq!(work.iter().map(|m| m.msgid).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn pages_walk_the_whole_cache_newest_first() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let all: Vec<CachedMessage> = (1..=5).map(|uid| msg(uid as u64, uid, &[])).collect();
        s.upsert_messages(&all).unwrap();
        let first = s.list_page(None, 2).unwrap();
        assert_eq!(
            first.iter().map(|m| m.uid).collect::<Vec<_>>(),
            vec![Some(5), Some(4)]
        );
        let second = s.list_page(first.last().unwrap().uid, 2).unwrap();
        assert_eq!(
            second.iter().map(|m| m.uid).collect::<Vec<_>>(),
            vec![Some(3), Some(2)]
        );
        let last = s.list_page(second.last().unwrap().uid, 2).unwrap();
        assert_eq!(
            last.iter().map(|m| m.uid).collect::<Vec<_>>(),
            vec![Some(1)]
        );
        assert!(s.list_page(last.last().unwrap().uid, 2).unwrap().is_empty());
    }

    #[test]
    fn label_counts_are_threads_and_unread_threads() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        // msg(1) is in thread 101, msg(2) in thread 102. Both are unread.
        s.upsert_messages(&[msg(1, 1, &["Work", "\\Inbox"]), msg(2, 2, &["Work"])])
            .unwrap();
        assert_eq!(
            s.label_thread_counts().unwrap(),
            vec![("Work".into(), 2, 2), ("\\Inbox".into(), 1, 1)]
        );
        assert_eq!(s.thread_totals().unwrap(), (2, 2));
        assert!(s.check().unwrap().logical.is_empty());

        // Relabelling message 1 moves thread 101 out of Work, and into Home.
        s.set_labels(1, &BTreeSet::from(["Home".to_string()]))
            .unwrap();
        assert_eq!(
            s.label_thread_counts().unwrap(),
            vec![("Home".into(), 1, 1), ("Work".into(), 1, 1)]
        );
        assert!(s.check().unwrap().logical.is_empty());

        // Reading message 1 makes its thread read. Thread 102 is still unread.
        s.set_seen(1, true).unwrap();
        assert_eq!(
            s.label_thread_counts().unwrap(),
            vec![("Home".into(), 1, 0), ("Work".into(), 1, 1)]
        );
        assert_eq!(s.thread_totals().unwrap(), (2, 1));
        assert!(s.check().unwrap().logical.is_empty());

        // Removing the only message of thread 102 takes its labels out of the counts.
        s.remove_uids(&[2]).unwrap();
        assert_eq!(
            s.label_thread_counts().unwrap(),
            vec![("Home".into(), 1, 0)]
        );
        assert_eq!(s.thread_totals().unwrap(), (1, 0));
        assert!(s.check().unwrap().logical.is_empty());
    }

    #[test]
    fn thread_summaries_follow_every_write() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        // Two messages in thread 100 (msgid + 100), one in thread 101.
        let mut a = msg(1, 1, &["Work"]);
        a.thrid = 100;
        let mut b = msg(2, 2, &["Home"]);
        b.thrid = 100;
        let mut c = msg(3, 3, &["Work"]);
        c.thrid = 101;
        s.upsert_messages(&[a, b, c]).unwrap();

        assert_eq!(s.thread_count(&ThreadFilter::All).unwrap(), 2);
        // Newest first: thread 101 holds uid 3, thread 100's newest is uid 2.
        let all = s.thread_page(&ThreadFilter::All, 0, 10).unwrap();
        assert_eq!(all[0].thrid, 101);
        assert_eq!(all[1].thrid, 100);
        assert_eq!(all[1].count, 2);
        assert_eq!(all[1].unread, 2);
        assert_eq!(all[1].head.uid, Some(2));
        assert_eq!(all[1].head.labels, BTreeSet::from(["Home".to_string()]));

        // A label shows the whole thread, even messages without that label.
        let work = s
            .thread_page(&ThreadFilter::Label("Work".into()), 0, 10)
            .unwrap();
        assert_eq!(work.len(), 2);
        assert_eq!(work[1].thrid, 100, "thread 100 has a Work message (uid 1)");
        assert_eq!(work[1].count, 2, "and it counts all of its messages");
        assert_eq!(
            s.thread_count(&ThreadFilter::Label("Work".into())).unwrap(),
            2
        );

        // Reading a message lowers the unread count.
        s.set_seen(2, true).unwrap();
        assert_eq!(
            s.thread_page(&ThreadFilter::All, 0, 10).unwrap()[1].unread,
            1
        );

        // Relabelling moves the thread in and out of a label's list.
        s.set_labels(3, &BTreeSet::from(["Home".to_string()]))
            .unwrap();
        assert_eq!(
            s.thread_count(&ThreadFilter::Label("Work".into())).unwrap(),
            1
        );
        assert_eq!(
            s.thread_count(&ThreadFilter::Label("Home".into())).unwrap(),
            2
        );

        // Removing a thread's last cached message removes the thread.
        s.remove_uids(&[3]).unwrap();
        assert_eq!(s.thread_count(&ThreadFilter::All).unwrap(), 1);
        assert_eq!(s.message_total().unwrap(), 2);
        assert_eq!(s.uids_for_threads(&[100, 101]).unwrap().len(), 2);

        // Invalidating UIDs empties the lists, and the next sync brings them back.
        s.invalidate_uids().unwrap();
        assert_eq!(s.thread_count(&ThreadFilter::All).unwrap(), 0);
        assert_eq!(s.message_total().unwrap(), 0);
    }

    #[test]
    fn threads_are_listed_by_newest_date_not_arrival() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        // Arrived last (highest UID), but sent long ago.
        let mut old = msg(9, 9, &[]);
        old.thrid = 500;
        old.sent = 1_000;
        // Arrived first, but sent most recently.
        let mut recent = msg(1, 1, &[]);
        recent.thrid = 100;
        recent.sent = 2_000_000_000;
        // One thread whose head is the newest by date, not the highest UID.
        let mut later_arrival = msg(7, 7, &[]);
        later_arrival.thrid = 900;
        later_arrival.sent = 10;
        let mut newer_date = msg(3, 3, &[]);
        newer_date.thrid = 900;
        newer_date.sent = 5_000;
        s.upsert_messages(&[old, recent, later_arrival, newer_date])
            .unwrap();

        let page = s.thread_page(&ThreadFilter::All, 0, 10).unwrap();
        assert_eq!(
            page.iter().map(|t| t.thrid).collect::<Vec<_>>(),
            vec![100, 900, 500]
        );
        assert_eq!(page[1].head.uid, Some(3), "the head is the newest by date");
        assert!(s.check().unwrap().logical.is_empty());
    }

    #[test]
    fn attachment_flags_reach_the_thread_list_and_survive_relistings() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let mut a = msg(1, 1, &[]);
        a.thrid = 100;
        let mut b = msg(2, 2, &[]);
        b.thrid = 100;
        s.upsert_messages(&[a, b]).unwrap();
        let mut missing = s.uids_missing_attachment_info().unwrap();
        missing.sort();
        assert_eq!(missing, vec![1, 2]);

        s.set_attachments(&[(2, true), (1, false)]).unwrap();
        assert!(s.uids_missing_attachment_info().unwrap().is_empty());
        assert!(s.thread_page(&ThreadFilter::All, 0, 5).unwrap()[0].attachments);
        assert!(s.check().unwrap().logical.is_empty());

        // A write that doesn't know about attachments keeps the cached answer.
        let mut again = msg(2, 2, &[]);
        again.thrid = 100;
        assert_eq!(again.attachment, None);
        s.upsert_messages(&[again]).unwrap();
        assert!(s.thread_page(&ThreadFilter::All, 0, 5).unwrap()[0].attachments);
        assert!(s.check().unwrap().logical.is_empty());
    }

    #[test]
    fn search_matches_sender_and_subject_words() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let mut a = msg(1, 1, &[]);
        a.from = "Alice Smith <alice@example.com>".into();
        a.subject = "Quarterly invoice".into();
        let mut b = msg(2, 2, &[]);
        b.from = "bob@example.org".into();
        b.subject = "Lunch on Friday".into();
        s.upsert_messages(&[a, b]).unwrap();

        let find = |text: &str| {
            s.thread_page(&ThreadFilter::Search(text.into()), 0, 10)
                .unwrap()
                .iter()
                .map(|t| t.head.uid.unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(find("invoice"), vec![1]);
        assert_eq!(find("inv"), vec![1], "prefixes of words match");
        assert_eq!(find("alice quarterly"), vec![1], "every word must match");
        assert_eq!(find("friday lunch"), vec![2]);
        assert!(find("nothing here").is_empty());
        assert!(find("   ").is_empty());
        assert_eq!(
            s.thread_count(&ThreadFilter::Search("invoice".into()))
                .unwrap(),
            1
        );
        // Quotes in the text can't break the query.
        assert!(find("\"quoted\" 'text").is_empty());
    }

    #[test]
    fn fts_query_quotes_each_word() {
        assert_eq!(fts_query("  a  b ").as_deref(), Some("\"a\"* \"b\"*"));
        assert_eq!(
            fts_query("say \"hi\"").as_deref(),
            Some("\"say\"* \"\"\"hi\"\"\"*")
        );
        assert_eq!(fts_query(""), None);
    }

    #[test]
    fn migrating_from_v3_builds_every_summary() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let mut m = msg(1, 1, &["Work"]);
        m.headers = b"From: Alice <Alice@Example.ORG>\r\nDate: Tue, 10 Nov 2026 09:30:00 +0000\r\nSubject: Hello 1\r\n\r\n".to_vec();
        m.sent = 0;
        s.upsert_messages(&[m]).unwrap();
        // Put the database back in its v3 shape: none of the v4 or v5 additions.
        s.conn
            .execute_batch(
                "DROP TRIGGER messages_fts_added;
                 DROP TRIGGER messages_fts_removed;
                 DROP TRIGGER messages_fts_changed;
                 DROP TRIGGER cached_messages_added;
                 DROP TRIGGER cached_messages_removed;
                 DROP TRIGGER cached_messages_uid;
                 DROP TABLE messages_fts;
                 DROP TABLE threads;
                 DROP TABLE thread_labels;
                 DROP TABLE cache_totals;
                 DROP INDEX messages_thread;
                 DROP INDEX messages_uid_unique;
                 CREATE INDEX messages_uid ON messages(uid);
                 DROP TABLE cache_state;
                 DROP TABLE pending_seen;
                 DROP TABLE label_threads;
                 ALTER TABLE messages DROP COLUMN attachment;
                 ALTER TABLE messages DROP COLUMN sent;
                 ALTER TABLE messages DROP COLUMN from_email;
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        s.migrate().unwrap();
        assert_eq!(s.list_page(None, 10).unwrap()[0].sent, 1_794_303_000);
        assert_eq!(s.thread_count(&ThreadFilter::All).unwrap(), 1);
        assert_eq!(s.message_total().unwrap(), 1);
        assert_eq!(
            s.list_page(None, 10).unwrap()[0].from_email,
            "alice@example.org"
        );
        assert_eq!(
            s.thread_page(&ThreadFilter::Search("hello".into()), 0, 5)
                .unwrap()
                .len(),
            1,
            "the rebuilt search index finds the subject \"Hello 1\""
        );
    }

    #[test]
    fn a_batch_of_bodies_round_trips() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[msg(1, 1, &[]), msg(2, 2, &[])])
            .unwrap();
        s.put_bodies(&[(1, b"first body"), (2, b"second body")])
            .unwrap();
        assert_eq!(s.get_body(1).unwrap().as_deref(), Some(&b"first body"[..]));
        assert_eq!(s.get_body(2).unwrap().as_deref(), Some(&b"second body"[..]));
    }

    #[test]
    fn a_dirty_cache_with_stale_summaries_is_repaired() {
        let dir = std::env::temp_dir().join(format!("gmail-repair-test-{}", std::process::id()));
        let path = dir.join("cache.sqlite");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut s = Store::open(&path).unwrap();
            s.upsert_messages(&[msg(1, 1, &["Work"]), msg(2, 2, &["Work"])])
                .unwrap();
        }
        // Damage a summary, and leave the cache marked as not closed cleanly.
        {
            let raw = Connection::open(&path).unwrap();
            raw.execute("DELETE FROM threads", []).unwrap();
            raw.execute("UPDATE cache_state SET clean = 0 WHERE id = 1", [])
                .unwrap();
        }
        let (s, notes) = Store::open_checked(&path).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("thread summaries"));
        assert_eq!(s.thread_count(&ThreadFilter::All).unwrap(), 2);
        assert!(s.check().unwrap().logical.is_empty());
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_cache_is_set_aside_and_a_new_one_started() {
        let dir = std::env::temp_dir().join(format!("gmail-damage-test-{}", std::process::id()));
        let path = dir.join("cache.sqlite");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut s = Store::open(&path).unwrap();
            s.upsert_messages(&[msg(1, 1, &["Work"])]).unwrap();
        }
        // Overwrite a page of the file, and mark it as not closed cleanly.
        {
            let raw = Connection::open(&path).unwrap();
            raw.execute("UPDATE cache_state SET clean = 0 WHERE id = 1", [])
                .unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        for b in bytes.iter_mut().skip(4096).take(4096) {
            *b = 0xFF;
        }
        std::fs::write(&path, &bytes).unwrap();

        let (s, notes) = Store::open_checked(&path).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("set aside"), "{}", notes[0]);
        assert_eq!(s.message_total().unwrap(), 0, "the new cache starts empty");
        let kept: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(kept.iter().any(|n| n.contains(".damaged-")), "{kept:?}");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_copies_the_cache_file() {
        let dir = std::env::temp_dir().join(format!("gmail-backup-test-{}", std::process::id()));
        let path = dir.join("cache.sqlite");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut s = Store::open(&path).unwrap();
            s.upsert_messages(&[msg(1, 1, &["Work"])]).unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        back_up(&conn, &path).unwrap();
        let copy = dir.join("cache.sqlite.before-migration.bak");
        assert_eq!(
            std::fs::metadata(&copy).unwrap().len(),
            std::fs::metadata(&path).unwrap().len()
        );
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_uid_index_rejects_a_duplicate_uid() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[msg(1, 1, &[])]).unwrap();
        let err = s.upsert_messages(&[msg(2, 1, &[])]);
        // The upsert resolves a conflict by msgid, so a second msgid with the same UID
        // is the case the unique index must refuse.
        assert!(err.is_err(), "a second message can't take UID 1");
    }

    #[test]
    fn read_state_stays_queued_until_the_server_confirms() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let mut a = msg(1, 1, &[]);
        a.thrid = 100;
        a.seen = false;
        let mut b = msg(2, 2, &[]);
        b.thrid = 100;
        b.seen = false;
        s.upsert_messages(&[a, b]).unwrap();

        s.set_seen_for_threads(&[100], true).unwrap();
        let unread = |s: &Store| s.thread_page(&ThreadFilter::All, 0, 5).unwrap()[0].unread;
        assert_eq!(unread(&s), 0);
        let mut queued = s.pending_seen().unwrap();
        queued.sort();
        assert_eq!(queued, vec![(1, 1, true), (2, 2, true)]);
        assert!(s.has_pending_seen().unwrap());

        // A sync that reports the old state doesn't undo a change that's still queued.
        s.set_seen(1, false).unwrap();
        assert_eq!(unread(&s), 0);

        // Once the server has confirmed, the cache follows the server again.
        s.clear_pending_seen(&[1, 2]).unwrap();
        assert!(!s.has_pending_seen().unwrap());
        s.set_seen(1, false).unwrap();
        assert_eq!(unread(&s), 1);
    }

    #[test]
    #[ignore]
    fn small_commit_timing() {
        let dir = std::env::temp_dir().join(format!("gmail-sync-timing-{}", std::process::id()));
        let path = dir.join("cache.sqlite");
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Store::open(&path).unwrap();
        let msgs: Vec<CachedMessage> = (1..=2000)
            .map(|u| msg(u64::from(u), u, &["Work"]))
            .collect();
        s.upsert_messages(&msgs).unwrap();
        for mode in ["FULL", "NORMAL"] {
            s.conn.pragma_update(None, "synchronous", mode).unwrap();
            let t = std::time::Instant::now();
            for u in 1..=2000u64 {
                s.set_seen(u, u % 2 == 0).unwrap();
            }
            println!(
                "2000 single-message commits, synchronous={mode}: {:?}",
                t.elapsed()
            );
        }
        let work = BTreeSet::from(["Work".to_string()]);
        let updates: Vec<(u32, &BTreeSet<String>, bool)> =
            (1..=2000u32).map(|u| (u, &work, u % 3 == 0)).collect();
        let t = std::time::Instant::now();
        s.apply_updates(&updates).unwrap();
        println!("the same 2000 changes in one batch: {:?}", t.elapsed());
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_batch_of_updates_matches_one_at_a_time() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let mut a = msg(1, 1, &["Work"]);
        a.thrid = 100;
        let mut b = msg(2, 2, &["Work"]);
        b.thrid = 100;
        s.upsert_messages(&[a, b, msg(3, 3, &["Home"])]).unwrap();

        let home = BTreeSet::from(["Home".to_string()]);
        let none = BTreeSet::new();
        // UID 99 isn't cached: it's skipped.
        s.apply_updates(&[(1, &home, true), (2, &none, true), (99, &home, false)])
            .unwrap();
        assert_eq!(
            s.label_thread_counts().unwrap(),
            vec![("Home".into(), 2, 1)],
            "thread 100 is now Home and read; message 3's thread is Home and unread"
        );
        assert!(s.check().unwrap().logical.is_empty());

        // A read-state change still queued for the server isn't overwritten.
        s.set_seen_for_threads(&[103], true).unwrap();
        s.apply_updates(&[(3, &home, false)]).unwrap();
        assert_eq!(s.thread_totals().unwrap(), (2, 0));
    }

    #[test]
    fn a_panic_leaves_the_cache_marked_not_clean() {
        let dir = std::env::temp_dir().join(format!("gmail-panic-test-{}", std::process::id()));
        let path = dir.join("cache.sqlite");
        let _ = std::fs::remove_dir_all(&dir);
        drop(Store::open(&path).unwrap());
        assert!(Store::open(&path).unwrap().opened_cleanly());

        let in_thread = path.clone();
        let outcome = std::panic::catch_unwind(move || {
            let _store = Store::open(&in_thread).unwrap();
            panic!("a bug while the cache is open");
        });
        assert!(outcome.is_err());
        assert!(
            !Store::open(&path).unwrap().opened_cleanly(),
            "the next start runs the checks"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mailbox_state_round_trips() {
        let s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        assert_eq!(s.mailbox_state().unwrap(), None);
        let st = MailboxState {
            uidvalidity: 42,
            highest_uid: Some(7),
            highest_modseq: Some(99),
        };
        s.set_mailbox_state(st).unwrap();
        assert_eq!(s.mailbox_state().unwrap(), Some(st));
    }

    #[test]
    fn body_round_trips_through_compression() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let raw = "Hello, body text. ".repeat(500).into_bytes();
        assert_eq!(s.get_body(1).unwrap(), None);
        s.put_body(1, &raw).unwrap();
        assert_eq!(s.get_body(1).unwrap(), Some(raw));
        assert!(s.body_bytes().unwrap() < 1000, "text should compress well");
    }

    #[test]
    fn evicts_least_recently_used_bodies_over_the_cap() {
        // Each body is about 1 KB of incompressible data, so the cap holds two.
        let mut s = Store::open_in_memory(2_500).unwrap();
        s.put_body(1, &noise(1000, 1)).unwrap();
        s.put_body(2, &noise(1000, 2)).unwrap();
        // Touch message 1 so message 2 is now the least recently used.
        assert!(s.get_body(1).unwrap().is_some());
        s.put_body(3, &noise(1000, 3)).unwrap();

        assert!(s.body_bytes().unwrap() <= 2_500);
        assert!(
            s.get_body(2).unwrap().is_none(),
            "LRU body should be evicted"
        );
        assert!(s.get_body(1).unwrap().is_some());
        assert!(s.get_body(3).unwrap().is_some());
    }

    #[test]
    fn clear_bodies_keeps_headers() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[msg(1, 1, &["\\Inbox"])]).unwrap();
        s.put_body(1, b"body").unwrap();
        s.clear_bodies().unwrap();
        assert_eq!(s.body_bytes().unwrap(), 0);
        assert!(s.get_body(1).unwrap().is_none());
        assert_eq!(s.list_messages(None, 10).unwrap().len(), 1);
    }

    #[test]
    fn cache_survives_reopening_the_file() {
        let dir = std::env::temp_dir().join(format!("gmail-store-test-{}", std::process::id()));
        let path = dir.join("cache.sqlite");
        let _ = std::fs::remove_file(&path);
        {
            let mut s = Store::open(&path).unwrap();
            s.upsert_messages(&[msg(1, 1, &["\\Inbox"])]).unwrap();
            s.put_body(1, b"kept").unwrap();
        }
        let mut s = Store::open(&path).unwrap();
        assert_eq!(s.list_messages(None, 10).unwrap().len(), 1);
        assert_eq!(s.get_body(1).unwrap(), Some(b"kept".to_vec()));
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn account_path_is_sanitized_and_under_the_data_dir() {
        let p = account_cache_path("Me+test/../x@Gmail.com").unwrap();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, "me_test_.._x@gmail.com.sqlite");
        assert!(p.to_string_lossy().contains("accounts"));
    }
}

#[cfg(test)]
mod bulk_timing {
    use super::*;
    use std::time::Instant;

    /// Measures the cache at real-world size. Run with:
    /// `cargo test --release bulk_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn quarter_million_headers() {
        let n: u64 = 250_000;
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let labels: BTreeSet<String> = ["\\Inbox".to_string(), "Work".to_string()].into();
        // A realistic header block (about 1.5 KB), so header parsing costs what it does in practice.
        let headers = [
            "Received: from mail.example.net (mail.example.net [203.0.113.5])\r\n\tby mx.example.com with ESMTPS id abc123\r\n\tfor <me@example.com>; Tue, 10 Nov 2026 09:29:58 +0000 (UTC)\r\n",
            "Received-SPF: pass (google.com: domain of a@example.com designates 203.0.113.5 as permitted sender)\r\n",
            "Authentication-Results: mx.example.com; spf=pass; dkim=pass header.i=@example.com; dmarc=pass\r\n",
            "DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/relaxed; d=example.com; s=sel; h=from:to:subject:date:message-id; bh=0123456789abcdefghijklmnopqrstuvwxyzABCDEF=; b=abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOP\r\n",
            "Date: Tue, 10 Nov 2026 09:30:00 +0000\r\n",
            "From: A Person <a@example.com>\r\n",
            "To: Me <me@example.com>\r\n",
            "Message-ID: <abc123@example.com>\r\n",
            "Subject: Something\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/alternative; boundary=\"b1\"\r\n\r\n",
        ]
        .concat()
        .into_bytes();

        let t = Instant::now();
        for start in (1..=n).step_by(200) {
            let batch: Vec<CachedMessage> = (start..(start + 200).min(n + 1))
                .map(|uid| CachedMessage {
                    msgid: 1_000_000_000_000 + uid,
                    // Threads of three messages, so the thread lists have real grouping.
                    thrid: 9_000_000_000 + uid / 3,
                    uid: Some(uid as u32),
                    labels: labels.clone(),
                    from: "a@example.com".into(),
                    from_email: "a@example.com".into(),
                    attachment: None,
                    subject: "Something".into(),
                    date: "Tue, 10 Nov 2026 09:30:00 +0000".into(),
                    headers: headers.clone(),
                    sent: sent_timestamp(&headers),
                    seen: false,
                })
                .collect();
            s.upsert_messages(&batch).unwrap();
        }
        println!("insert 250k headers: {:?}", t.elapsed());

        let t = Instant::now();
        let uids = s.cached_uids().unwrap();
        println!("cached_uids ({} rows): {:?}", uids.len(), t.elapsed());

        // The list the GUI shows: walk every thread a window at a time, as it scrolls.
        let t = Instant::now();
        let total = s.thread_count(&ThreadFilter::All).unwrap();
        println!("thread_count(All): {:?} ({total} threads)", t.elapsed());
        let t = Instant::now();
        let (mut offset, mut seen) = (0usize, 0usize);
        loop {
            let page = s.thread_page(&ThreadFilter::All, offset, 200).unwrap();
            if page.is_empty() {
                break;
            }
            seen += page.iter().map(|x| x.count).sum::<usize>();
            offset += page.len();
        }
        println!(
            "thread_page walk ({seen} messages, windows of 200): {:?}",
            t.elapsed()
        );

        // A rare label, on ten messages in ten threads.
        for msgid in 1_000_000_000_001..=1_000_000_000_010u64 {
            let labels: BTreeSet<String> = [
                "\\Inbox".to_string(),
                "Work".to_string(),
                "Rare".to_string(),
            ]
            .into();
            s.set_labels(msgid, &labels).unwrap();
        }
        let t = Instant::now();
        let rare_total = s.thread_count(&ThreadFilter::Label("Rare".into())).unwrap();
        let rare_page = s
            .thread_page(&ThreadFilter::Label("Rare".into()), 0, 200)
            .unwrap();
        println!(
            "label Rare: count + first window: {:?} ({rare_total} threads, {} shown)",
            t.elapsed(),
            rare_page.len()
        );

        let t = Instant::now();
        let work_total = s.thread_count(&ThreadFilter::Label("Work".into())).unwrap();
        let work_page = s
            .thread_page(&ThreadFilter::Label("Work".into()), 0, 200)
            .unwrap();
        println!(
            "label Work: count + first window: {:?} ({work_total} threads, {} shown)",
            t.elapsed(),
            work_page.len()
        );

        let t = Instant::now();
        let hits = s
            .thread_count(&ThreadFilter::Search("something".into()))
            .unwrap();
        let hit_page = s
            .thread_page(&ThreadFilter::Search("something".into()), 0, 200)
            .unwrap();
        println!(
            "search \"something\" (every message matches): count + first window: {:?} ({hits} threads, {} shown)",
            t.elapsed(),
            hit_page.len()
        );

        let t = Instant::now();
        let newest = s.list_messages(None, 500).unwrap();
        println!(
            "list_messages(500): {:?} ({} rows)",
            t.elapsed(),
            newest.len()
        );

        let t = Instant::now();
        let work = s.list_messages(Some("Work"), 500).unwrap();
        println!(
            "list_messages(Work, 500): {:?} ({} rows)",
            t.elapsed(),
            work.len()
        );

        let t = Instant::now();
        let stale: Vec<u32> = (1..=1000).collect();
        s.remove_uids(&stale).unwrap();
        println!("remove 1000 stale uids: {:?}", t.elapsed());
    }
}
