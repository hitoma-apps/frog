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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

use crate::search::Query;
use crate::uidset::UidSet;

/// Default body cache size, in compressed bytes.
pub const DEFAULT_BODY_CAP: u64 = 1 << 30;

/// zstd level. Mail is text, so a low level compresses well and stays fast.
const ZSTD_LEVEL: i32 = 3;

const SCHEMA_VERSION: i64 = 15;

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
/// The FROM, WHERE and arguments of a thread-list query, for one filter.
struct ListSql {
    /// A WITH clause, for filters that need one.
    with: String,
    from: &'static str,
    /// Extra conditions, each starting with ` AND `.
    conditions: String,
    args: Vec<rusqlite::types::Value>,
}

impl ListSql {
    /// The query parts for `filter`, or `None` when it can match nothing (an empty search).
    fn for_filter(filter: &ThreadFilter) -> Option<Self> {
        use rusqlite::types::Value;
        let mut list = ListSql {
            with: String::new(),
            from: "FROM threads t",
            conditions: String::new(),
            args: Vec::new(),
        };
        match filter {
            ThreadFilter::All => {}
            ThreadFilter::Label(label) => {
                list.conditions.push_str(
                    " AND EXISTS (SELECT 1 FROM thread_labels tl
                                  WHERE tl.label = ? AND tl.thrid = t.thrid)",
                );
                list.args.push(Value::Text(label.clone()));
            }
            ThreadFilter::Search(text) => {
                let query = fts_query(text)?;
                list.with = "WITH hits AS (
                                 SELECT DISTINCT m.thrid FROM messages_fts
                                 JOIN messages m ON m.msgid = messages_fts.rowid
                                 WHERE messages_fts MATCH ? AND m.uid IS NOT NULL) "
                    .to_string();
                list.from = "FROM hits h JOIN threads t ON t.thrid = h.thrid";
                list.args.push(Value::Text(query));
            }
        }
        Some(list)
    }
}

/// The columns a thread-list row reads, from `threads t` joined to its head message `m`.
const THREAD_COLUMNS: &str = "t.thrid, t.count, t.unread, m.msgid, m.thrid, m.uid,
                              m.from_addr, m.subject, m.date, m.seen, m.sent, m.from_email,
                              t.attachments, m.received, t.newest_time";

/// The field the thread list is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SortKey {
    /// The newest message's date.
    Date,
    /// The newest message's sender address.
    Sender,
    /// The newest message's subject.
    Subject,
}

/// How the thread list is sorted. Ties are broken by thread id, in the same direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListOrder {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for ListOrder {
    /// Newest first, as the list has always shown it.
    fn default() -> Self {
        Self {
            key: SortKey::Date,
            descending: true,
        }
    }
}

/// A thread's position in the list: its newest time, then its thread id.
pub type ThreadKey = (i64, u64);

type ThreadListRow = (ThreadKey, ThreadMeta, CachedMessage);
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
    /// When the server received the message, as a Unix time (INTERNALDATE). `None` if not known
    /// yet. Listings leave it `None`, and a write with `None` keeps what the cache has.
    pub received: Option<i64>,
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

/// The conditions a search adds to a query on `messages m`, as SQL (each starts with " AND ")
/// and the values they bind, in order.
fn search_filter(query: &Query) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;
    let mut sql = String::new();
    let mut args: Vec<Value> = Vec::new();
    for word in &query.words {
        sql.push_str(" AND m.msgid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)");
        args.push(Value::Text(format!("\"{word}\"*")));
    }
    for word in &query.from {
        sql.push_str(" AND m.msgid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)");
        args.push(Value::Text(format!(
            "{{from_addr from_email}} : \"{word}\"*"
        )));
    }
    for word in &query.subject {
        sql.push_str(" AND m.msgid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)");
        args.push(Value::Text(format!("subject : \"{word}\"*")));
    }
    for label in &query.labels {
        sql.push_str(" AND m.msgid IN (SELECT msgid FROM message_labels WHERE label = ?)");
        args.push(Value::Text(label.clone()));
    }
    match query.unread {
        Some(true) => sql.push_str(" AND m.seen = 0"),
        Some(false) => sql.push_str(" AND m.seen = 1"),
        None => {}
    }
    if query.attachment == Some(true) {
        sql.push_str(" AND m.attachment = 1");
    }
    if let Some(after) = query.after {
        sql.push_str(" AND COALESCE(m.received, m.sent) >= ?");
        args.push(Value::Integer(after));
    }
    if let Some(before) = query.before {
        sql.push_str(" AND COALESCE(m.received, m.sent) < ?");
        args.push(Value::Integer(before));
    }
    (sql, args)
}

/// Where the app's log file lives: `<data dir>/frog.log`. Returns `None` if the platform has no
/// data directory.
pub fn log_file_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "gmail-labels-proto")?;
    Some(dirs.data_dir().join("frog.log"))
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

/// Creates the cache's folder and file so only the owner can read them. The cache holds mail
/// bodies, so other users on the computer must not be able to open it.
fn prepare_private_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path)?;
        }
    }
    Ok(())
}

/// Limits an existing cache file and its journal files to the owner.
fn restrict_cache_files(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            let file = PathBuf::from(name);
            if file.exists() {
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// A message in the storage view: one of the largest cached messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeMessage {
    pub uid: u32,
    pub thrid: u64,
    pub from: String,
    pub subject: String,
    pub received: Option<i64>,
    pub size: u64,
    pub attachment: bool,
}

/// A sender's place in the sender statistics: how many cached messages they sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderStat {
    /// The sender's address, lower-cased.
    pub address: String,
    /// A display form of the sender, as one of their messages shows it.
    pub display: String,
    pub messages: usize,
    pub unread: usize,
    /// The newest message's received time, or sent time where that's unknown.
    pub latest: i64,
}

/// The storage view: the largest cached messages, and how many messages have a known size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageView {
    pub rows: Vec<LargeMessage>,
    /// Messages whose size is known.
    pub sized: usize,
    /// Cached messages with a UID, whether or not their size is known.
    pub total: usize,
}

/// Rows between the sort keys sampled for jumps. A jump counts forward from the nearest sampled
/// row, so it counts at most this many rows.
pub const SAMPLE_STRIDE: usize = 1_000;

/// Remembers sort keys of thread rows, so a page can seek to its start instead of counting rows
/// from the top. Two kinds are kept: rows recently read, which make scrolling in order cheap,
/// and a sample of every `stride`-th row, which makes a jump cheap. Both are dropped when
/// another connection changes the cache (see `Store::data_version`), since row positions may
/// have moved.
pub struct PageCursor {
    stride: usize,
    /// The thread ids in another sort order, for the current filter and cache version.
    sorted: Option<(ThreadFilter, ListOrder, i64, Vec<u64>)>,
    filter: Option<ThreadFilter>,
    version: i64,
    /// Row index → sort key, for rows recently read.
    recent: BTreeMap<usize, ThreadKey>,
    /// Row index → sort key, for every `stride`-th row. Filled on the first jump.
    sampled: BTreeMap<usize, ThreadKey>,
    /// Whether `sampled` has been filled for the current filter and version.
    is_sampled: bool,
}

/// Most recently read rows remembered. Past this, they're dropped and remembered again.
const MAX_RECENT: usize = 4_096;

impl PageCursor {
    /// A cursor that samples every `stride`-th row for jumps.
    pub fn new(stride: usize) -> Self {
        Self {
            stride: stride.max(1),
            sorted: None,
            filter: None,
            version: 0,
            recent: BTreeMap::new(),
            sampled: BTreeMap::new(),
            is_sampled: false,
        }
    }

    /// The page of `count` threads from row `start`, in the order `order` gives. The newest-first
    /// order uses the remembered rows; any other order keeps the list of ids for the filter.
    pub fn page(
        &mut self,
        store: &Store,
        filter: &ThreadFilter,
        order: ListOrder,
        start: usize,
        count: usize,
    ) -> Result<Vec<ThreadSummary>> {
        let version = store.data_version()?;
        if order != ListOrder::default() {
            let fresh = matches!(
                &self.sorted,
                Some((f, o, v, _)) if f == filter && *o == order && *v == version
            );
            if !fresh {
                let ids = store.thread_order(filter, order)?;
                self.sorted = Some((filter.clone(), order, version, ids));
            }
            let ids: Vec<u64> = match &self.sorted {
                Some((_, _, _, ids)) => ids.iter().skip(start).take(count).copied().collect(),
                None => Vec::new(),
            };
            return store.threads_by_id(&ids);
        }
        if self.filter.as_ref() != Some(filter) || self.version != version {
            self.filter = Some(filter.clone());
            self.version = version;
            self.recent.clear();
            self.sampled.clear();
            self.is_sampled = false;
        }
        // A jump with no row close above `start` to count from: sample the whole list once, so
        // the row to count from is never more than `stride` rows away.
        let close = self
            .nearest(start)
            .is_some_and(|(i, _)| start - i <= self.stride);
        if !close && start > 0 && !self.is_sampled {
            for (index, key) in store.thread_keys_every(filter, self.stride)? {
                self.sampled.insert(index, key);
            }
            self.is_sampled = true;
        }
        // Count forward from the nearest remembered row above `start`, if there is one.
        let (after, skip) = match self.nearest(start) {
            Some((index, key)) => (Some(key), start - index - 1),
            None => (None, start),
        };
        let rows = store.thread_page_after(filter, after, skip, count)?;
        if self.recent.len() + 2 > MAX_RECENT {
            self.recent.clear();
        }
        if let (Some((first, _)), Some((last, _))) = (rows.first(), rows.last()) {
            self.recent.insert(start, *first);
            self.recent.insert(start + rows.len() - 1, *last);
        }
        Ok(rows.into_iter().map(|(_, summary)| summary).collect())
    }

    /// The closest known row above `start`: the one with the largest index below it.
    fn nearest(&self, start: usize) -> Option<(usize, ThreadKey)> {
        let recent = self.recent.range(..start).next_back();
        let sampled = self.sampled.range(..start).next_back();
        match (recent, sampled) {
            (Some((&a, &ka)), Some((&b, &kb))) => Some(if a >= b { (a, ka) } else { (b, kb) }),
            (Some((&a, &ka)), None) => Some((a, ka)),
            (None, Some((&b, &kb))) => Some((b, kb)),
            (None, None) => None,
        }
    }
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

/// Adds the received-time column (schema v12), and renames the thread sort key from the sent
/// time to a time that's received when known. Does nothing if that's already done.
/// Label names are kept decoded (see `utf7`). Caches from before that stored them as the server
/// sent them, so the names are decoded in place and the derived tables rebuilt. A name that
/// already exists decoded keeps that row.
fn upgrade_label_names(tx: &Transaction) -> Result<()> {
    let raw_names: Vec<String> = {
        let mut stmt = tx.prepare("SELECT DISTINCT label FROM message_labels")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?
    };
    for raw in raw_names {
        let name = crate::utf7::decode(&raw);
        if name == raw {
            continue;
        }
        tx.execute(
            "UPDATE OR IGNORE message_labels SET label = ?2 WHERE label = ?1",
            params![raw, name],
        )?;
        tx.execute("DELETE FROM message_labels WHERE label = ?1", params![raw])?;
    }
    rebuild_derived(tx)
}

/// The search index covered only `from_addr`, which holds the sender's display name when there is
/// one. It now covers the sender's address too, so `from:` finds mail by either.
fn upgrade_fts_sender_address(tx: &Transaction) -> Result<()> {
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS messages_fts_added;
         DROP TRIGGER IF EXISTS messages_fts_removed;
         DROP TRIGGER IF EXISTS messages_fts_changed;
         DROP TABLE IF EXISTS messages_fts;
         CREATE VIRTUAL TABLE messages_fts USING fts5(
             from_addr, from_email, subject, content='messages', content_rowid='msgid'
         );
         CREATE TRIGGER messages_fts_added AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts (rowid, from_addr, from_email, subject)
                 VALUES (NEW.msgid, NEW.from_addr, NEW.from_email, NEW.subject);
         END;
         CREATE TRIGGER messages_fts_removed AFTER DELETE ON messages BEGIN
             INSERT INTO messages_fts (messages_fts, rowid, from_addr, from_email, subject)
                 VALUES ('delete', OLD.msgid, OLD.from_addr, OLD.from_email, OLD.subject);
         END;
         CREATE TRIGGER messages_fts_changed AFTER UPDATE OF from_addr, from_email, subject ON messages
         BEGIN
             INSERT INTO messages_fts (messages_fts, rowid, from_addr, from_email, subject)
                 VALUES ('delete', OLD.msgid, OLD.from_addr, OLD.from_email, OLD.subject);
             INSERT INTO messages_fts (rowid, from_addr, from_email, subject)
                 VALUES (NEW.msgid, NEW.from_addr, NEW.from_email, NEW.subject);
         END;
         INSERT INTO messages_fts (messages_fts) VALUES ('rebuild');",
    )?;
    Ok(())
}

fn upgrade_size(tx: &Transaction) -> Result<()> {
    let has_size: i64 = tx.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'size'",
        [],
        |r| r.get(0),
    )?;
    if has_size == 0 {
        tx.execute_batch("ALTER TABLE messages ADD COLUMN size INTEGER;")?;
    }
    tx.execute_batch("CREATE INDEX IF NOT EXISTS messages_size ON messages(size);")?;
    Ok(())
}

fn upgrade_received(tx: &Transaction) -> Result<()> {
    let has_received: i64 = tx.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'received'",
        [],
        |r| r.get(0),
    )?;
    if has_received == 0 {
        tx.execute_batch("ALTER TABLE messages ADD COLUMN received INTEGER;")?;
    }
    let has_time: i64 = tx.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('threads') WHERE name = 'newest_time'",
        [],
        |r| r.get(0),
    )?;
    if has_time == 0 {
        // The sort key was called newest_sent before this version. The index follows the
        // rename, so it's recreated only if the column had to be added instead.
        let has_sent: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('threads') WHERE name = 'newest_sent'",
            [],
            |r| r.get(0),
        )?;
        if has_sent > 0 {
            tx.execute_batch("ALTER TABLE threads RENAME COLUMN newest_sent TO newest_time;")?;
        } else {
            tx.execute_batch(
                "ALTER TABLE threads ADD COLUMN newest_time INTEGER NOT NULL DEFAULT 0;
                 DROP INDEX IF EXISTS threads_newest;
                 CREATE INDEX threads_newest ON threads(newest_time, thrid);",
            )?;
        }
    }
    Ok(())
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
         ALTER TABLE threads ADD COLUMN newest_time INTEGER NOT NULL DEFAULT 0;
         CREATE INDEX threads_newest ON threads(newest_time, thrid);",
    )?;
    Ok(())
}

/// Recomputes every summary table from the messages. The search index is not included.
fn rebuild_derived(tx: &Transaction) -> Result<()> {
    // The columns the rebuild uses must exist on every path, including old caches.
    upgrade_threads(tx)?;
    upgrade_attachments(tx)?;
    upgrade_received(tx)?;
    tx.execute_batch(
        "DELETE FROM threads;
         DELETE FROM thread_labels;
         DELETE FROM label_threads;
         INSERT INTO threads (thrid, count, unread, head_msgid, newest_time, attachments)
             SELECT m.thrid, COUNT(*), SUM(m.seen = 0),
                 (SELECT m2.msgid FROM messages m2 WHERE m2.thrid = m.thrid AND m2.uid IS NOT NULL
                  ORDER BY COALESCE(m2.received, m2.sent) DESC, m2.uid DESC LIMIT 1),
                 MAX(COALESCE(m.received, m.sent)),
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
        .prepare_cached("SELECT thrid FROM messages WHERE msgid = ?1")?
        .query_row(params![msgid], |r| r.get(0))
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
    Ok(())
}

fn refresh_threads(tx: &Transaction, thrids: &BTreeSet<u64>) -> Result<()> {
    for &thrid in thrids {
        let thrid = to_i64(thrid)?;
        let before = thread_contribution(tx, thrid)?;
        // The head is the newest message by date, as the thread list shows it.
        let (count, unread, head, newest_time, attachments): (
            i64,
            i64,
            Option<i64>,
            Option<i64>,
            i64,
        ) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(seen = 0), 0),
                 (SELECT msgid FROM messages WHERE thrid = ?1 AND uid IS NOT NULL
                  ORDER BY COALESCE(received, sent) DESC, uid DESC LIMIT 1),
                 MAX(COALESCE(received, sent)),
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
                 (thrid, count, unread, head_msgid, newest_time, attachments)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![thrid, count, unread, head, newest_time, attachments],
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
    // Drops labels whose last thread went away. Done once for the batch, not after each change.
    tx.execute("DELETE FROM label_threads WHERE threads <= 0", [])?;
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
        received: None,
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
        prepare_private_file(path)?;
        let conn = Connection::open(path)?;
        restrict_cache_files(path)?;
        // A cache from an older version is copied before any migration rewrites it.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 0 && version < SCHEMA_VERSION {
            back_up(&conn, path)?;
        }
        Self::init(conn, DEFAULT_BODY_CAP)
    }

    /// How many times other connections have changed the cache. A reader compares it between
    /// reads, to tell when positions it remembered may be stale.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?)
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
        crate::log::log("CACHE", "open: opening the cache file");
        let store = match crate::timed_log("CACHE", "open: opening and upgrading the cache", || {
            Self::open(path)
        }) {
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
        if self.read_only {
            // FTS5's integrity-check is written as an INSERT, which a read-only connection
            // can't run. The reader compares how many documents the index holds with how
            // many messages there are instead.
            let (indexed, messages): (i64, i64) = self.conn.query_row(
                "SELECT (SELECT COUNT(*) FROM messages_fts_docsize),
                        (SELECT COUNT(*) FROM messages)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if indexed != messages {
                logical.push(format!(
                    "search index: {indexed} indexed messages, but {messages} messages"
                ));
            }
        } else if let Err(e) = self.conn.execute(
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
                      ORDER BY COALESCE(m2.received, m2.sent) DESC, m2.uid DESC LIMIT 1),
                     MAX(COALESCE(m.received, m.sent)),
                     COUNT(CASE WHEN m.attachment = 1 THEN 1 END)
                 FROM messages m WHERE m.uid IS NOT NULL GROUP BY m.thrid",
                "SELECT thrid, count, unread, head_msgid, newest_time, attachments FROM threads",
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
    /// Sets how many bytes of message bodies the cache keeps. Takes effect at the next body write,
    /// which then evicts the least recently used bodies down to the cap.
    pub fn set_body_cap(&mut self, cap: u64) {
        self.body_cap = cap;
    }

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
        if version < 12 {
            // The thread sort key changes from the sent time to the received time.
            rebuild_derived(&tx)?;
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
        if version < 13 {
            // Message sizes, for the storage view. They don't affect thread summaries.
            upgrade_size(&tx)?;
        }
        if version < 15 {
            upgrade_fts_sender_address(&tx)?;
        }
        if version < 14 {
            upgrade_label_names(&tx)?;
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
            tx.prepare_cached(
                "INSERT INTO messages (msgid, thrid, uid, from_addr, subject, date, headers, seen, sent, from_email, attachment, received)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(msgid) DO UPDATE SET
                     thrid = excluded.thrid, uid = excluded.uid,
                     from_addr = excluded.from_addr, subject = excluded.subject,
                     date = excluded.date, headers = excluded.headers,
                     seen = excluded.seen, sent = excluded.sent,
                     from_email = excluded.from_email,
                     attachment = COALESCE(excluded.attachment, messages.attachment),
                     received = COALESCE(excluded.received, messages.received)",
            )?
            .execute(params![
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
                m.attachment,
                m.received
            ])?;
            tx.prepare_cached("DELETE FROM message_labels WHERE msgid = ?1")?
                .execute(params![msgid])?;
            let mut insert_label =
                tx.prepare_cached("INSERT INTO message_labels (msgid, label) VALUES (?1, ?2)")?;
            for label in &m.labels {
                insert_label.execute(params![msgid, label])?;
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
    /// The ids of every thread a list shows, in the order `order` gives.
    pub fn thread_order(&self, filter: &ThreadFilter, order: ListOrder) -> Result<Vec<u64>> {
        let Some(list) = ListSql::for_filter(filter) else {
            return Ok(Vec::new());
        };
        let key = match order.key {
            SortKey::Date => "t.newest_time",
            SortKey::Sender => "LOWER(m.from_email)",
            SortKey::Subject => "LOWER(m.subject)",
        };
        let dir = if order.descending { "DESC" } else { "ASC" };
        let sql = format!(
            "{with}SELECT t.thrid {from}
             JOIN messages m ON m.msgid = t.head_msgid
             WHERE 1 = 1{conditions}
             ORDER BY {key} {dir}, t.thrid {dir}",
            with = list.with,
            from = list.from,
            conditions = list.conditions,
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let ids = stmt
            .query_map(rusqlite::params_from_iter(list.args.iter()), |r| {
                Ok(r.get::<_, i64>(0)? as u64)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// The threads with these ids, in the order given. An id with no thread is left out.
    pub fn threads_by_id(&self, ids: &[u64]) -> Result<Vec<ThreadSummary>> {
        use rusqlite::types::Value;
        let mut found: HashMap<u64, (ThreadMeta, CachedMessage)> = HashMap::new();
        for chunk in ids.chunks(400) {
            let marks = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT {THREAD_COLUMNS} FROM threads t
                 JOIN messages m ON m.msgid = t.head_msgid
                 WHERE t.thrid IN ({marks})"
            );
            let args: Vec<Value> = chunk.iter().map(|&t| Value::Integer(t as i64)).collect();
            for (_, meta, head) in
                self.thread_rows(&sql, rusqlite::params_from_iter(args.iter()))?
            {
                found.insert(meta.0, (meta, head));
            }
        }
        let mut metas = Vec::new();
        let mut heads = Vec::new();
        for id in ids {
            if let Some((meta, head)) = found.remove(id) {
                metas.push(meta);
                heads.push(head);
            }
        }
        self.attach_labels(&mut heads)?;
        Ok(metas
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

    /// A page of the thread list in the order `order` gives, from `offset` on.
    pub fn thread_page_sorted(
        &self,
        filter: &ThreadFilter,
        order: ListOrder,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ThreadSummary>> {
        let page: Vec<u64> = self
            .thread_order(filter, order)?
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect();
        self.threads_by_id(&page)
    }

    pub fn thread_page(
        &self,
        filter: &ThreadFilter,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ThreadSummary>> {
        Ok(self
            .thread_page_after(filter, None, offset, limit)?
            .into_iter()
            .map(|(_, summary)| summary)
            .collect())
    }

    /// A page of the thread list, with each thread's sort key. With `after`, the page starts
    /// from the first thread below that key, and `skip` counts rows from there. Without it,
    /// `skip` counts from the top.
    pub fn thread_page_after(
        &self,
        filter: &ThreadFilter,
        after: Option<ThreadKey>,
        skip: usize,
        limit: usize,
    ) -> Result<Vec<(ThreadKey, ThreadSummary)>> {
        use rusqlite::types::Value;
        let Some(mut list) = ListSql::for_filter(filter) else {
            return Ok(Vec::new());
        };
        if let Some((newest, thrid)) = after {
            list.conditions
                .push_str(" AND (t.newest_time, t.thrid) < (?, ?)");
            list.args.push(Value::Integer(newest));
            list.args.push(Value::Integer(thrid as i64));
        }
        let sql = format!(
            "{with}SELECT {THREAD_COLUMNS} {from}
             JOIN messages m ON m.msgid = t.head_msgid
             WHERE 1 = 1{conditions}
             ORDER BY t.newest_time DESC, t.thrid DESC LIMIT ? OFFSET ?",
            with = list.with,
            from = list.from,
            conditions = list.conditions,
        );
        list.args.push(Value::Integer(limit as i64));
        list.args.push(Value::Integer(skip as i64));
        let rows = self.thread_rows(&sql, rusqlite::params_from_iter(list.args.iter()))?;
        let (mut keys, mut meta, mut heads) = (Vec::new(), Vec::new(), Vec::new());
        for (key, m, head) in rows {
            keys.push(key);
            meta.push(m);
            heads.push(head);
        }
        self.attach_labels(&mut heads)?;
        Ok(keys
            .into_iter()
            .zip(meta)
            .zip(heads)
            .map(|((key, (thrid, count, unread, attachments)), head)| {
                (
                    key,
                    ThreadSummary {
                        thrid,
                        count,
                        unread,
                        attachments,
                        head,
                    },
                )
            })
            .collect())
    }

    /// The sort key of every `stride`-th thread in the list, starting with the first, paired
    /// with its position. These are the rows a jump counts from.
    pub fn thread_keys_every(
        &self,
        filter: &ThreadFilter,
        stride: usize,
    ) -> Result<Vec<(usize, ThreadKey)>> {
        let Some(list) = ListSql::for_filter(filter) else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "{with}SELECT t.newest_time, t.thrid {from}
             JOIN messages m ON m.msgid = t.head_msgid
             WHERE 1 = 1{conditions}
             ORDER BY t.newest_time DESC, t.thrid DESC",
            with = list.with,
            from = list.from,
            conditions = list.conditions,
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let keys = stmt.query_map(rusqlite::params_from_iter(list.args.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        let mut out = Vec::new();
        for (index, key) in keys.enumerate() {
            if index % stride == 0 {
                out.push((index, key?));
            }
        }
        Ok(out)
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
                    received: r.get(13)?,
                    subject: r.get(7)?,
                    date: r.get(8)?,
                    headers: Vec::new(),
                    seen: r.get(9)?,
                    sent: r.get(10)?,
                };
                Ok((
                    (r.get::<_, i64>(14)?, r.get::<_, i64>(0)? as u64),
                    (
                        r.get::<_, i64>(0)? as u64,
                        r.get::<_, i64>(1)? as usize,
                        r.get::<_, i64>(2)? as usize,
                        r.get::<_, i64>(12)? > 0,
                    ),
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
            "SELECT msgid, thrid, uid, from_addr, subject, date, seen, sent, from_email, headers, received FROM messages
             WHERE thrid = ?1 AND uid IS NOT NULL ORDER BY uid DESC",
        )?;
        let mut out = stmt
            .query_map(params![to_i64(thrid)?], |r| {
                let mut m = cached_from_row(r)?;
                m.headers = r.get(9)?;
                m.received = r.get(10)?;
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
                received: None,
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

    /// Records each message's attachment flag and received time, by UID, and refreshes their
    /// threads. A `None` time keeps the one the cache has.
    pub fn set_structures(&mut self, rows: &[(u32, bool, Option<i64>, Option<u64>)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let mut affected: BTreeSet<u64> = BTreeSet::new();
        for &(uid, has, received, size) in rows {
            let thread: Option<i64> = tx
                .query_row(
                    "SELECT thrid FROM messages WHERE uid = ?1",
                    params![uid],
                    |r| r.get(0),
                )
                .optional()?;
            affected.extend(thread.map(|t| t as u64));
            tx.execute(
                "UPDATE messages SET attachment = ?2, received = COALESCE(?3, received), size = COALESCE(?4, size) WHERE uid = ?1",
                params![uid, has, received, size.map(|s| s as i64)],
            )?;
        }
        refresh_threads(&tx, &affected)?;
        tx.commit()?;
        Ok(())
    }

    /// UIDs of cached messages whose attachments haven't been looked at yet.
    pub fn uids_missing_attachment_info(&self) -> Result<Vec<u32>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT uid FROM messages WHERE uid IS NOT NULL AND (attachment IS NULL OR received IS NULL OR size IS NULL)",
        )?;
        let uids = stmt
            .query_map([], |r| r.get::<_, u32>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(uids)
    }

    /// The cached messages that match a search, newest first, at most `limit` of them. Every
    /// part of the query must hold for the same message.
    pub fn search_messages(&self, query: &Query, limit: usize) -> Result<Vec<CachedMessage>> {
        use rusqlite::types::Value;
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let (filter, mut args) = search_filter(query);
        let sql = format!(
            "SELECT m.uid, m.msgid, m.thrid, m.from_addr, m.subject, m.date, m.seen,
                    m.sent, m.from_email, m.attachment, m.received
             FROM messages m WHERE m.uid IS NOT NULL{filter}
             ORDER BY COALESCE(m.received, m.sent) DESC, m.uid DESC LIMIT ?"
        );
        args.push(Value::Integer(limit as i64));

        let mut hits = {
            let mut stmt = self.conn.prepare(&sql)?;
            stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok(CachedMessage {
                    uid: r.get(0)?,
                    msgid: r.get::<_, i64>(1)? as u64,
                    thrid: r.get::<_, i64>(2)? as u64,
                    labels: BTreeSet::new(),
                    from: r.get(3)?,
                    from_email: r.get(8)?,
                    attachment: r.get(9)?,
                    received: r.get(10)?,
                    subject: r.get(4)?,
                    date: r.get(5)?,
                    headers: Vec::new(),
                    seen: r.get(6)?,
                    sent: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        self.attach_labels(&mut hits)?;
        Ok(hits)
    }

    /// How many cached messages match a search, however many `search_messages` returns.
    pub fn count_search(&self, query: &Query) -> Result<usize> {
        if query.is_empty() {
            return Ok(0);
        }
        let (filter, args) = search_filter(query);
        let n: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM messages m WHERE m.uid IS NOT NULL{filter}"),
            rusqlite::params_from_iter(args.iter()),
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Threads whose newest cached message is from `account`, so no one has answered it, newest
    /// first, with each thread's message count. Drafts don't count.
    pub fn waiting_for_reply(
        &self,
        account: &str,
        limit: usize,
    ) -> Result<Vec<(CachedMessage, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.uid, m.msgid, m.thrid, m.from_addr, m.subject, m.date, m.seen,
                    m.sent, m.from_email, m.attachment, m.received, t.count
             FROM threads t
             JOIN messages m ON m.msgid = t.head_msgid
             WHERE m.from_email = ?1
               AND NOT EXISTS (SELECT 1 FROM message_labels l
                               WHERE l.msgid = m.msgid AND l.label = '\\Draft')
             ORDER BY COALESCE(m.received, m.sent) DESC, t.thrid DESC
             LIMIT ?2",
        )?;
        let mut rows: Vec<(CachedMessage, usize)> = stmt
            .query_map(params![account.to_ascii_lowercase(), limit as i64], |r| {
                let message = CachedMessage {
                    uid: r.get(0)?,
                    msgid: r.get::<_, i64>(1)? as u64,
                    thrid: r.get::<_, i64>(2)? as u64,
                    labels: BTreeSet::new(),
                    from: r.get(3)?,
                    from_email: r.get(8)?,
                    attachment: r.get(9)?,
                    received: r.get(10)?,
                    subject: r.get(4)?,
                    date: r.get(5)?,
                    headers: Vec::new(),
                    seen: r.get(6)?,
                    sent: r.get(7)?,
                };
                Ok((message, r.get::<_, i64>(11)? as usize))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut heads: Vec<CachedMessage> = rows.iter().map(|(m, _)| m.clone()).collect();
        self.attach_labels(&mut heads)?;
        for ((row, _), head) in rows.iter_mut().zip(heads) {
            row.labels = head.labels;
        }
        Ok(rows)
    }

    /// The senders with the most cached messages, most first, at most `limit` of them.
    pub fn frequent_senders(&self, limit: usize) -> Result<Vec<SenderStat>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_email, COUNT(*), SUM(seen = 0), MAX(COALESCE(received, sent)),
                    MAX(from_addr)
             FROM messages
             WHERE uid IS NOT NULL AND from_email != ''
             GROUP BY from_email
             ORDER BY COUNT(*) DESC, from_email
             LIMIT ?1",
        )?;
        let senders = stmt
            .query_map(params![limit as i64], |r| {
                Ok(SenderStat {
                    address: r.get(0)?,
                    messages: r.get::<_, i64>(1)? as usize,
                    unread: r.get::<_, i64>(2)? as usize,
                    latest: r.get(3)?,
                    display: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(senders)
    }

    /// The largest cached messages, biggest first, at most `limit` of them.
    pub fn storage_view(&self, limit: usize) -> Result<StorageView> {
        let sized: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE size IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE uid IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT uid, thrid, from_addr, subject, received, size, attachment FROM messages
             WHERE uid IS NOT NULL AND size IS NOT NULL
             ORDER BY size DESC, msgid
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(LargeMessage {
                    uid: r.get(0)?,
                    thrid: r.get::<_, i64>(1)? as u64,
                    from: r.get(2)?,
                    subject: r.get(3)?,
                    received: r.get(4)?,
                    size: r.get::<_, i64>(5)? as u64,
                    attachment: r.get::<_, Option<bool>>(6)?.unwrap_or(false),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(StorageView {
            rows,
            sized: sized as usize,
            total: total as usize,
        })
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
                received: None,
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

    /// Drops all cached bodies. Headers and sync state are kept. Returns how many were removed.
    pub fn clear_bodies(&mut self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM bodies", [])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_view_lists_the_largest_sized_messages_first() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[
            msg(1, 1, &["Work"]),
            msg(2, 2, &["Work"]),
            msg(3, 3, &["Work"]),
        ])
        .unwrap();
        s.set_structures(&[
            (1, false, None, Some(100)),
            (2, true, None, Some(9_000)),
            (3, false, None, None),
        ])
        .unwrap();
        let v = s.storage_view(10).unwrap();
        assert_eq!((v.sized, v.total), (2, 3));
        let uids: Vec<u32> = v.rows.iter().map(|r| r.uid).collect();
        assert_eq!(uids, vec![2, 1]);
        assert!(v.rows[0].attachment);
        assert_eq!(s.storage_view(1).unwrap().rows.len(), 1);
    }

    #[test]
    fn old_label_names_are_decoded_by_the_upgrade() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        // The first message has the name as an older cache stored it, in modified UTF-7.
        s.upsert_messages(&[msg(1, 1, &["Caf&AOk-"]), msg(2, 2, &["Café"])])
            .unwrap();
        let tx = s.conn.transaction().unwrap();
        upgrade_label_names(&tx).unwrap();
        tx.commit().unwrap();
        assert_eq!(s.list_messages(Some("Café"), 10).unwrap().len(), 2);
        assert!(s.list_messages(Some("Caf&AOk-"), 10).unwrap().is_empty());
    }

    #[test]
    fn search_is_per_message_and_every_part_must_hold_for_it() {
        use crate::search::parse;
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[
            msg(1, 1, &["Work"]),
            msg(2, 2, &["Work"]),
            msg(3, 3, &["Home"]),
        ])
        .unwrap();
        s.set_structures(&[
            (1, false, Some(2_000), Some(20)),
            (2, true, Some(1_000), Some(10)),
            (3, true, Some(3_000), Some(30)),
        ])
        .unwrap();
        s.set_seen(1, true).unwrap();
        let uids = |s: &Store, text: &str| -> Vec<u32> {
            s.search_messages(&parse(text).query, 100)
                .unwrap()
                .iter()
                .map(|h| h.uid.unwrap())
                .collect()
        };
        assert_eq!(uids(&s, "hello"), vec![3, 1, 2]);
        assert_eq!(uids(&s, "label:Work"), vec![1, 2]);
        assert_eq!(uids(&s, "label:Work is:unread"), vec![2]);
        assert_eq!(uids(&s, "has:attachment"), vec![3, 2]);
        assert_eq!(uids(&s, "from:sender3"), vec![3]);
        assert_eq!(uids(&s, "subject:hello from:sender2"), vec![2]);
        assert_eq!(uids(&s, "label:Home has:attachment"), vec![3]);
        assert!(uids(&s, "label:Home is:read").is_empty());
        assert!(uids(&s, "is:flagged").is_empty());
    }

    #[test]
    fn date_operators_split_messages_at_local_midnight() {
        use crate::search::parse;
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[msg(1, 1, &["Work"]), msg(2, 2, &["Work"])])
            .unwrap();
        let day = parse("after:2026-11-01").query.after.unwrap();
        s.set_structures(&[
            (1, false, Some(day + 3_600), None),
            (2, false, Some(day - 3_600), None),
        ])
        .unwrap();
        let uids = |text: &str| -> Vec<u32> {
            s.search_messages(&parse(text).query, 100)
                .unwrap()
                .iter()
                .map(|h| h.uid.unwrap())
                .collect()
        };
        assert_eq!(uids("after:2026-11-01"), vec![1]);
        assert_eq!(uids("before:2026-11-01"), vec![2]);
    }

    #[test]
    fn from_finds_mail_by_display_name_or_address() {
        use crate::search::parse;
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let mut m = msg(1, 1, &["Work"]);
        m.from = "Bob Smith".into();
        m.from_email = "bob@example.com".into();
        s.upsert_messages(&[m]).unwrap();
        let found = |text: &str| s.search_messages(&parse(text).query, 10).unwrap().len();
        assert_eq!(found("from:bob@example.com"), 1);
        assert_eq!(found("from:smith"), 1);
        assert_eq!(found("from:carol"), 0);
    }

    #[test]
    fn waiting_for_reply_lists_threads_whose_newest_message_is_mine() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let mut a1 = msg(1, 1, &["Work"]);
        a1.from_email = "bob@example.com".into();
        a1.thrid = 500;
        let mut a2 = msg(2, 2, &["Work"]);
        a2.from_email = "me@example.com".into();
        a2.thrid = 500;
        let mut b1 = msg(3, 3, &["Work"]);
        b1.from_email = "me@example.com".into();
        b1.thrid = 600;
        let mut b2 = msg(4, 4, &["Work"]);
        b2.from_email = "bob@example.com".into();
        b2.thrid = 600;
        s.upsert_messages(&[a1, a2, b1, b2]).unwrap();
        let rows = s.waiting_for_reply("ME@example.com", 10).unwrap();
        let thrids: Vec<u64> = rows.iter().map(|(m, _)| m.thrid).collect();
        assert_eq!(thrids, vec![500]);
        assert_eq!(rows[0].1, 2);
    }

    #[test]
    fn thread_lists_sort_by_date_sender_and_subject_in_either_direction() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let mut messages: Vec<CachedMessage> =
            (1..=3u64).map(|i| msg(i, i as u32, &["Work"])).collect();
        messages[0].from_email = "carol@example.com".into();
        messages[0].subject = "Banana".into();
        messages[1].from_email = "alice@example.com".into();
        messages[1].subject = "apple".into();
        messages[2].from_email = "bob@example.com".into();
        messages[2].subject = "Cherry".into();
        s.upsert_messages(&messages).unwrap();
        let order = |key, descending| ListOrder { key, descending };
        let subjects = |o| -> Vec<String> {
            s.thread_page_sorted(&ThreadFilter::All, o, 0, 10)
                .unwrap()
                .iter()
                .map(|t| t.head.subject.clone())
                .collect()
        };
        // Date, newest first, matches the plain thread page.
        let by_date: Vec<u64> = s
            .thread_page(&ThreadFilter::All, 0, 10)
            .unwrap()
            .iter()
            .map(|t| t.thrid)
            .collect();
        assert_eq!(
            s.thread_order(&ThreadFilter::All, ListOrder::default())
                .unwrap(),
            by_date
        );
        assert_eq!(
            subjects(order(SortKey::Subject, false)),
            vec!["apple", "Banana", "Cherry"]
        );
        assert_eq!(
            subjects(order(SortKey::Subject, true)),
            vec!["Cherry", "Banana", "apple"]
        );
        let senders: Vec<String> = s
            .thread_page_sorted(&ThreadFilter::All, order(SortKey::Sender, false), 0, 10)
            .unwrap()
            .iter()
            .map(|t| t.head.from_email.clone())
            .collect();
        assert_eq!(
            senders,
            vec!["alice@example.com", "bob@example.com", "carol@example.com"]
        );
        // A page reads from its offset.
        let second = s
            .thread_page_sorted(&ThreadFilter::All, order(SortKey::Sender, false), 1, 1)
            .unwrap();
        assert_eq!(second[0].head.from_email, "bob@example.com");
    }

    #[test]
    fn clearing_bodies_keeps_the_headers_and_the_list() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[msg(1, 1, &["Work"]), msg(2, 2, &["Work"])])
            .unwrap();
        s.put_body(1, b"Subject: Hello\r\n\r\nbody one").unwrap();
        s.put_body(2, b"Subject: Hello\r\n\r\nbody two").unwrap();
        assert_eq!(s.clear_bodies().unwrap(), 2);
        assert!(s.get_body(1).unwrap().is_none());
        assert_eq!(s.thread_count(&ThreadFilter::All).unwrap(), 2);
    }

    #[test]
    fn frequent_senders_count_messages_per_address() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let mut messages: Vec<CachedMessage> =
            (1..=4u64).map(|i| msg(i, i as u32, &["Work"])).collect();
        for m in messages.iter_mut().take(3) {
            m.from_email = "bob@example.com".into();
        }
        s.upsert_messages(&messages).unwrap();
        s.set_seen(1, true).unwrap();
        let senders = s.frequent_senders(10).unwrap();
        assert_eq!(senders[0].address, "bob@example.com");
        assert_eq!(senders[0].messages, 3);
        assert_eq!(senders[0].unread, 2);
        assert_eq!(senders[1].messages, 1);
        assert_eq!(s.frequent_senders(1).unwrap().len(), 1);
    }

    #[test]
    fn cursor_pages_in_another_order_follow_the_cache() {
        let dir = std::env::temp_dir().join(format!("cursor-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("order.sqlite");
        let _ = std::fs::remove_file(&path);
        let mut writer = Store::open(&path).unwrap();
        let messages: Vec<CachedMessage> =
            (1..=60u64).map(|i| msg(i, i as u32, &["Work"])).collect();
        writer.upsert_messages(&messages).unwrap();
        let reader = Store::open_reader(&path).unwrap();
        let order = ListOrder {
            key: SortKey::Sender,
            descending: true,
        };
        let mut cursor = PageCursor::new(SAMPLE_STRIDE);
        let check = |cursor: &mut PageCursor| {
            for start in [0usize, 13, 40, 57] {
                let want: Vec<u64> = reader
                    .thread_page_sorted(&ThreadFilter::All, order, start, 10)
                    .unwrap()
                    .iter()
                    .map(|t| t.thrid)
                    .collect();
                let got: Vec<u64> = cursor
                    .page(&reader, &ThreadFilter::All, order, start, 10)
                    .unwrap()
                    .iter()
                    .map(|t| t.thrid)
                    .collect();
                assert_eq!(got, want, "start {start}");
            }
        };
        check(&mut cursor);
        writer.remove_uids(&[2, 9, 30]).unwrap();
        check(&mut cursor);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sampled_keys_are_every_stride_th_row_of_the_list() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let all: Vec<CachedMessage> = (1..=95u64).map(|i| msg(i, i as u32, &["Work"])).collect();
        s.upsert_messages(&all).unwrap();
        let rows = s.thread_page(&ThreadFilter::All, 0, 200).unwrap();
        let sampled = s.thread_keys_every(&ThreadFilter::All, 10).unwrap();
        let indices: Vec<usize> = sampled.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, (0..95).step_by(10).collect::<Vec<_>>());
        for (index, key) in sampled {
            assert_eq!(key.1, rows[index].thrid, "row {index}");
        }
    }

    #[test]
    fn cursor_pages_match_offset_pages_even_after_the_cache_changes() {
        let dir = std::env::temp_dir().join(format!("cursor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cursor.sqlite");
        let _ = std::fs::remove_file(&path);
        // A writer and a reader on one file, as the app has them.
        let mut writer = Store::open(&path).unwrap();
        let messages: Vec<CachedMessage> =
            (1..=300u64).map(|i| msg(i, i as u32, &["Work"])).collect();
        writer.upsert_messages(&messages).unwrap();
        let reader = Store::open_reader(&path).unwrap();
        // A stride of 7 samples the rows finely, so jumps in this test use the sample.
        let mut cursor = PageCursor::new(7);
        let starts = [0, 40, 80, 120, 7, 250, 260, 299, 150, 151, 0, 3, 200];
        for filter in [ThreadFilter::All, ThreadFilter::Label("Work".into())] {
            let check = |cursor: &mut PageCursor| {
                for &start in &starts {
                    let want: Vec<u64> = reader
                        .thread_page(&filter, start, 10)
                        .unwrap()
                        .iter()
                        .map(|t| t.thrid)
                        .collect();
                    let got: Vec<u64> = cursor
                        .page(&reader, &filter, ListOrder::default(), start, 10)
                        .unwrap()
                        .iter()
                        .map(|t| t.thrid)
                        .collect();
                    assert_eq!(got, want, "start {start} for {filter:?}");
                }
            };
            check(&mut cursor);
            // Removing messages moves the rows below them, so remembered positions are stale.
            writer.remove_uids(&[5, 6, 7, 100]).unwrap();
            check(&mut cursor);
            writer.upsert_messages(&[msg(301, 301, &["Work"])]).unwrap();
            check(&mut cursor);
        }
        drop(reader);
        drop(writer);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Times the newer queries on a large cache, and the one-time index rebuild a cache from
    /// before schema v15 runs on first start. Run with
    /// `cargo test --release -- --ignored --nocapture query_timing`.
    #[test]
    #[ignore = "timing check; run in release mode"]
    fn query_timing() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let mut all: Vec<CachedMessage> = (1..=200_000u64)
            .map(|i| msg(i, i as u32, &["Work"]))
            .collect();
        for (i, m) in all.iter_mut().enumerate() {
            m.from_email = format!("sender{}@example.com", i % 500);
        }
        let t = std::time::Instant::now();
        for chunk in all.chunks(5_000) {
            s.upsert_messages(chunk).unwrap();
        }
        println!("load 200k messages: {:?}", t.elapsed());
        let time = |label: &str, f: &mut dyn FnMut()| {
            let t = std::time::Instant::now();
            f();
            println!("{label}: {:?}", t.elapsed());
        };
        time("frequent_senders(500)", &mut || {
            s.frequent_senders(500).unwrap();
        });
        time("waiting_for_reply", &mut || {
            s.waiting_for_reply("me@example.com", 500).unwrap();
        });
        time("storage_view(500)", &mut || {
            s.storage_view(500).unwrap();
        });
        let query = crate::search::parse("from:sender7 subject:hello").query;
        time("search from:sender7 subject:hello", &mut || {
            s.search_messages(&query, 500).unwrap();
        });
        time("threads count (All)", &mut || {
            s.thread_count(&ThreadFilter::All).unwrap();
        });
        time("thread_order by date", &mut || {
            s.thread_order(&ThreadFilter::All, ListOrder::default())
                .unwrap();
        });
        time("thread_order by sender", &mut || {
            s.thread_order(
                &ThreadFilter::All,
                ListOrder {
                    key: SortKey::Sender,
                    descending: false,
                },
            )
            .unwrap();
        });
        time("thread_order by subject", &mut || {
            s.thread_order(
                &ThreadFilter::All,
                ListOrder {
                    key: SortKey::Subject,
                    descending: false,
                },
            )
            .unwrap();
        });
        time("v15 FTS rebuild", &mut || {
            let tx = s.conn.transaction().unwrap();
            upgrade_fts_sender_address(&tx).unwrap();
            tx.commit().unwrap();
        });
    }

    /// Times pages of the thread list near the start and the end of a large cache. Run with
    /// `cargo test --release -- --ignored --nocapture thread_page_timing`.
    #[test]
    #[ignore = "timing check; run in release mode"]
    fn thread_page_timing() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let all: Vec<CachedMessage> = (1..=200_000u64)
            .map(|i| msg(i, i as u32, &["Work"]))
            .collect();
        for chunk in all.chunks(5_000) {
            s.upsert_messages(chunk).unwrap();
        }
        // Scrolling down in order, as the list does, with the cursor.
        let mut cursor = PageCursor::new(SAMPLE_STRIDE);
        let mut slowest = std::time::Duration::ZERO;
        let mut total = std::time::Duration::ZERO;
        for start in (0..200_000).step_by(40) {
            let t = std::time::Instant::now();
            cursor
                .page(&s, &ThreadFilter::All, ListOrder::default(), start, 40)
                .unwrap();
            let took = t.elapsed();
            total += took;
            slowest = slowest.max(took);
        }
        println!("scrolling 5000 pages with the cursor: slowest {slowest:?}, total {total:?}");
        // Jumps: the first samples the list, and later ones count from a sampled row.
        let mut jumper = PageCursor::new(SAMPLE_STRIDE);
        for start in [100_000usize, 190_000, 55_555, 190_123] {
            let t = std::time::Instant::now();
            let page = jumper
                .page(&s, &ThreadFilter::All, ListOrder::default(), start, 40)
                .unwrap();
            println!("jump to {start}: {} rows in {:?}", page.len(), t.elapsed());
        }
    }

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
            received: None,
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
    fn repair_rebuilds_summaries_that_disagree_with_the_messages() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        s.upsert_messages(&[msg(1, 1, &["Work"]), msg(2, 2, &["Work"])])
            .unwrap();
        // Damage the summary table directly, as an interrupted write might.
        s.conn.execute("DELETE FROM threads", []).unwrap();
        let before = s.check().unwrap();
        assert!(!before.logical.is_empty());
        assert!(before.physical.is_empty());
        s.repair().unwrap();
        let after = s.check().unwrap();
        assert!(after.logical.is_empty(), "{:?}", after.logical);
        assert_eq!(s.thread_totals().unwrap(), (2, 2));
    }

    #[test]
    fn reader_check_runs_without_writing() {
        let dir = std::env::temp_dir().join(format!("reader-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reader.sqlite");
        let _ = std::fs::remove_file(&path);
        let mut writer = Store::open(&path).unwrap();
        writer
            .upsert_messages(&[msg(1, 1, &["Work"]), msg(2, 2, &["Work"])])
            .unwrap();
        let reader = Store::open_reader(&path).unwrap();
        let report = reader.check().unwrap();
        assert!(report.physical.is_empty());
        assert!(report.logical.is_empty(), "{:?}", report.logical);
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

        s.set_structures(&[
            (2, true, Some(1_000), Some(10)),
            (1, false, Some(2_000), Some(20)),
        ])
        .unwrap();
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
    fn threads_are_listed_by_received_time_when_known() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        // Dated later by its sender, but received first.
        let mut early = msg(1, 1, &[]);
        early.thrid = 500;
        early.sent = 2_000_000_000;
        early.received = Some(1_000);
        // Dated earlier by its sender, but received last.
        let mut late = msg(2, 2, &[]);
        late.thrid = 100;
        late.sent = 1_000;
        late.received = Some(3_000_000_000);
        s.upsert_messages(&[early, late]).unwrap();
        let order: Vec<u64> = s
            .thread_page(&ThreadFilter::All, 0, 10)
            .unwrap()
            .iter()
            .map(|t| t.thrid)
            .collect();
        assert_eq!(order, vec![100, 500]);
        assert!(s.check().unwrap().logical.is_empty());
    }

    #[test]
    fn search_count_is_every_match_not_just_the_rows_returned() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let messages: Vec<CachedMessage> = (1..=12u32)
            .map(|i| msg(u64::from(i), i, &["Work"]))
            .chain([msg(100, 100, &["Home"])])
            .collect();
        s.upsert_messages(&messages).unwrap();
        let query = crate::search::Query {
            labels: vec!["Work".to_string()],
            ..Default::default()
        };
        assert_eq!(s.count_search(&query).unwrap(), 12);
        assert_eq!(s.search_messages(&query, 5).unwrap().len(), 5);
        assert_eq!(s.search_messages(&query, 500).unwrap().len(), 12);
        assert_eq!(s.count_search(&crate::search::Query::default()).unwrap(), 0);
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
                    received: None,
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

#[cfg(all(test, unix))]
mod permission_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_cache_file_and_its_folder_are_owner_only() {
        let dir = std::env::temp_dir().join(format!("gmail-perm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("accounts").join("me@example.com.sqlite");
        let store = Store::open(&path).unwrap();
        drop(store);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        std::fs::remove_dir_all(&dir).ok();
    }
}
