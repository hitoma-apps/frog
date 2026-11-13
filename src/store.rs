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
use rusqlite::{Connection, OptionalExtension, params};

/// Default body cache size, in compressed bytes.
pub const DEFAULT_BODY_CAP: u64 = 1 << 30;

/// zstd level. Mail is text, so a low level compresses well and stays fast.
const ZSTD_LEVEL: i32 = 3;

const SCHEMA_VERSION: i64 = 4;

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
        for m in msgs {
            let msgid = to_i64(m.msgid)?;
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
        tx.commit()?;
        Ok(())
    }

    /// Replaces one message's labels, for example after a server-confirmed change.
    pub fn set_labels(&mut self, msgid: u64, labels: &BTreeSet<String>) -> Result<()> {
        let msgid = to_i64(msgid)?;
        let tx = self.conn.transaction()?;
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
        tx.commit()?;
        Ok(())
    }

    /// Records whether a message has been read.
    pub fn set_seen(&mut self, msgid: u64, seen: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET seen = ?2 WHERE msgid = ?1",
            params![to_i64(msgid)?, seen],
        )?;
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
        for uid in uids {
            tx.execute("DELETE FROM messages WHERE uid = ?1", params![uid])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Clears every UID. Call this when UIDVALIDITY changes. The messages
    /// themselves are kept, and the next sync assigns new UIDs.
    pub fn invalidate_uids(&mut self) -> Result<()> {
        self.conn.execute("UPDATE messages SET uid = NULL", [])?;
        self.conn.execute("DELETE FROM sync", [])?;
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
    fn migrating_from_v3_fills_in_sent_times() {
        let mut s = Store::open_in_memory(DEFAULT_BODY_CAP).unwrap();
        let mut m = msg(1, 1, &["Work"]);
        m.headers = b"Date: Tue, 10 Nov 2026 09:30:00 +0000\r\nSubject: Hello 1\r\n\r\n".to_vec();
        m.sent = 0;
        s.upsert_messages(&[m]).unwrap();
        // Put the database back in its v3 shape: no sent column, version 3.
        s.conn
            .execute_batch("ALTER TABLE messages DROP COLUMN sent; PRAGMA user_version = 3;")
            .unwrap();
        s.migrate().unwrap();
        assert_eq!(s.list_page(None, 10).unwrap()[0].sent, 1_794_303_000);
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
                    thrid: 9_000_000_000 + uid,
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

        // The startup show: walk the whole cache in pages of 5000, as the worker does.
        let t = Instant::now();
        let (mut before, mut seen) = (None, 0usize);
        loop {
            let page = s.list_page(before, 5000).unwrap();
            if page.is_empty() {
                break;
            }
            seen += page.len();
            before = page.last().and_then(|m| m.uid);
        }
        println!(
            "list_page walk ({seen} rows, pages of 5000): {:?}",
            t.elapsed()
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
