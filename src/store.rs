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
use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Default body cache size, in compressed bytes.
pub const DEFAULT_BODY_CAP: u64 = 1 << 30;

/// zstd level. Mail is text, so a low level compresses well and stays fast.
const ZSTD_LEVEL: i32 = 3;

const SCHEMA_VERSION: i64 = 5;

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
fn refresh_threads(tx: &Transaction, thrids: &BTreeSet<u64>) -> Result<()> {
    for &thrid in thrids {
        let thrid = to_i64(thrid)?;
        let (count, unread, newest): (i64, i64, Option<i64>) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(seen = 0), 0), MAX(uid) FROM messages
             WHERE thrid = ?1 AND uid IS NOT NULL",
            params![thrid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        tx.execute("DELETE FROM thread_labels WHERE thrid = ?1", params![thrid])?;
        if count == 0 {
            tx.execute("DELETE FROM threads WHERE thrid = ?1", params![thrid])?;
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO threads (thrid, count, unread, newest_uid)
             VALUES (?1, ?2, ?3, ?4)",
            params![thrid, count, unread, newest],
        )?;
        tx.execute(
            "INSERT INTO thread_labels (label, thrid)
             SELECT DISTINCT l.label, m.thrid FROM messages m
             JOIN message_labels l ON l.msgid = m.msgid
             WHERE m.thrid = ?1 AND m.uid IS NOT NULL",
            params![thrid],
        )?;
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
        subject: r.get(4)?,
        date: r.get(5)?,
        headers: Vec::new(),
        seen: r.get(6)?,
        sent: r.get(7)?,
    })
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
        Self::init(Connection::open(path)?, DEFAULT_BODY_CAP)
    }

    /// An in-memory cache, for tests.
    pub fn open_in_memory(body_cap: u64) -> Result<Self> {
        Self::init(Connection::open_in_memory()?, body_cap)
    }

    fn init(conn: Connection, body_cap: u64) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let mut store = Self { conn, body_cap };
        store.migrate()?;
        Ok(store)
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
                "INSERT INTO messages (msgid, thrid, uid, from_addr, subject, date, headers, seen, sent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(msgid) DO UPDATE SET
                     thrid = excluded.thrid, uid = excluded.uid,
                     from_addr = excluded.from_addr, subject = excluded.subject,
                     date = excluded.date, headers = excluded.headers,
                     seen = excluded.seen, sent = excluded.sent",
                params![
                    msgid,
                    to_i64(m.thrid)?,
                    m.uid,
                    m.from,
                    m.subject,
                    m.date,
                    m.headers,
                    m.seen,
                    m.sent
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

    /// Records whether a message has been read.
    pub fn set_seen(&mut self, msgid: u64, seen: bool) -> Result<()> {
        let msgid = to_i64(msgid)?;
        let tx = self.conn.transaction()?;
        let thread = thread_of(&tx, msgid)?;
        tx.execute(
            "UPDATE messages SET seen = ?2 WHERE msgid = ?1",
            params![msgid, seen],
        )?;
        if let Some(t) = thread {
            refresh_threads(&tx, &BTreeSet::from([t]))?;
        }
        tx.commit()?;
        Ok(())
    }

    /// How many cached messages carry each label, by label name. Read from the
    /// stored totals, so it doesn't scan the messages.
    pub fn label_counts(&self) -> Result<Vec<(String, usize)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT label, count FROM label_stats ORDER BY label")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
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
                               m.from_addr, m.subject, m.date, m.seen, m.sent";
        let (limit, offset) = (limit as i64, offset as i64);
        let rows: Vec<(u64, usize, usize, CachedMessage)> = match filter {
            ThreadFilter::All => {
                let sql = format!(
                    "SELECT {COLUMNS} FROM threads t
                     JOIN messages m ON m.thrid = t.thrid AND m.uid = t.newest_uid
                     ORDER BY t.newest_uid DESC LIMIT ?1 OFFSET ?2"
                );
                self.thread_rows(&sql, params![limit, offset])?
            }
            ThreadFilter::Label(label) => {
                // Walk the threads newest first, and stop once a window is full. A
                // label that most threads carry fills its window at once.
                let sql = format!(
                    "SELECT {COLUMNS} FROM threads t
                     JOIN messages m ON m.thrid = t.thrid AND m.uid = t.newest_uid
                     WHERE EXISTS (SELECT 1 FROM thread_labels tl
                                   WHERE tl.label = ?1 AND tl.thrid = t.thrid)
                     ORDER BY t.newest_uid DESC LIMIT ?2 OFFSET ?3"
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
                         JOIN messages m ON m.thrid = t.thrid AND m.uid = t.newest_uid
                         ORDER BY t.newest_uid DESC LIMIT ?2 OFFSET ?3"
                    );
                    self.thread_rows(&sql, params![query, limit, offset])?
                }
            },
        };
        let (meta, mut heads): (Vec<(u64, usize, usize)>, Vec<CachedMessage>) =
            rows.into_iter().map(|(t, c, u, m)| ((t, c, u), m)).unzip();
        self.attach_labels(&mut heads)?;
        Ok(meta
            .into_iter()
            .zip(heads)
            .map(|((thrid, count, unread), head)| ThreadSummary {
                thrid,
                count,
                unread,
                head,
            })
            .collect())
    }

    /// Runs a thread-list query. Each row is the thread's summary and its newest message.
    fn thread_rows(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<(u64, usize, usize, CachedMessage)>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params, |r| {
                let head = CachedMessage {
                    msgid: r.get::<_, i64>(3)? as u64,
                    thrid: r.get::<_, i64>(4)? as u64,
                    uid: r.get(5)?,
                    labels: BTreeSet::new(),
                    from: r.get(6)?,
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
                    head,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every cached message of one thread, newest first, with labels.
    pub fn thread_messages(&self, thrid: u64) -> Result<Vec<CachedMessage>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT msgid, thrid, uid, from_addr, subject, date, seen, sent FROM messages
             WHERE thrid = ?1 AND uid IS NOT NULL ORDER BY uid DESC",
        )?;
        let mut out = stmt
            .query_map(params![to_i64(thrid)?], cached_from_row)?
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
            "SELECT msgid, thrid, uid, from_addr, subject, date, seen, sent FROM messages
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
        tx.commit()?;
        Ok(())
    }

    /// Cached headers, newest first, optionally limited to one label.
    /// Messages with no UID (after a UIDVALIDITY change) are left out.
    pub fn list_messages(&self, label: Option<&str>, limit: usize) -> Result<Vec<CachedMessage>> {
        let sql = match label {
            None => {
                "SELECT msgid, thrid, uid, from_addr, subject, date, seen, sent FROM messages
                 WHERE uid IS NOT NULL ORDER BY uid DESC LIMIT ?2"
            }
            Some(_) => {
                "SELECT msgid, thrid, uid, from_addr, subject, date, seen, sent FROM messages
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
        let msgid = to_i64(msgid)?;
        let data = zstd::encode_all(raw, ZSTD_LEVEL)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO bodies (msgid, data, raw_size, stored_size, last_access)
             VALUES (?1, ?2, ?3, ?4, (SELECT COALESCE(MAX(last_access), 0) + 1 FROM bodies))
             ON CONFLICT(msgid) DO UPDATE SET
                 data = excluded.data, raw_size = excluded.raw_size,
                 stored_size = excluded.stored_size, last_access = excluded.last_access",
            params![msgid, data, raw.len() as i64, data.len() as i64],
        )?;
        loop {
            let total: i64 = tx.query_row(
                "SELECT COALESCE(SUM(stored_size), 0) FROM bodies",
                [],
                |r| r.get(0),
            )?;
            if total as u64 <= self.body_cap {
                break;
            }
            let evicted = tx.execute(
                "DELETE FROM bodies WHERE msgid = (
                     SELECT msgid FROM bodies ORDER BY last_access ASC LIMIT 1)",
                [],
            )?;
            if evicted == 0 {
                break;
            }
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
    fn label_counts_follow_every_write() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[msg(1, 1, &["Work", "\\Inbox"]), msg(2, 2, &["Work"])])
            .unwrap();
        assert_eq!(
            s.label_counts().unwrap(),
            vec![("Work".to_string(), 2), ("\\Inbox".to_string(), 1)]
        );

        // Replacing labels adjusts both the old and the new label.
        let home: BTreeSet<String> = ["Home".to_string()].into();
        s.set_labels(1, &home).unwrap();
        assert_eq!(
            s.label_counts().unwrap(),
            vec![("Home".to_string(), 1), ("Work".to_string(), 1)]
        );

        // Removing a message (the labels cascade) drops its labels from the totals.
        s.remove_uids(&[2]).unwrap();
        assert_eq!(s.label_counts().unwrap(), vec![("Home".to_string(), 1)]);
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
        m.headers = b"Date: Tue, 10 Nov 2026 09:30:00 +0000\r\nSubject: Hello 1\r\n\r\n".to_vec();
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
                 ALTER TABLE messages DROP COLUMN sent;
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        s.migrate().unwrap();
        assert_eq!(s.list_page(None, 10).unwrap()[0].sent, 1_794_303_000);
        assert_eq!(s.thread_count(&ThreadFilter::All).unwrap(), 1);
        assert_eq!(s.message_total().unwrap(), 1);
        assert_eq!(
            s.thread_page(&ThreadFilter::Search("hello".into()), 0, 5)
                .unwrap()
                .len(),
            1,
            "the rebuilt search index finds the subject \"Hello 1\""
        );
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
