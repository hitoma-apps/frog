//! Background worker. It owns the mail source and the local cache. It runs
//! commands in order, and uses idle time for background work:
//!
//! 1. fetching headers that aren't cached yet, newest first, until all are held
//! 2. prefetching message bodies
//!
//! It also keeps the connection alive. It sends NOOP after a quiet period, and
//! reconnects when the connection drops, retrying the failed command once.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use eframe::egui;
use gmail_imap_proto::labels::LabelAction;
use gmail_imap_proto::store::{
    DEFAULT_BODY_CAP, MailboxState, Store, ThreadFilter, account_cache_path,
};
use gmail_imap_proto::uidset::UidSet;

use crate::backend::{IdleNotice, MailSource, MailboxInfo, Message, MessageUpdate, SourceError};
use crate::live;

/// How long one IDLE wait lasts before checking for commands.
const IDLE_POLL: Duration = Duration::from_secs(1);
/// Messages per header FETCH, so the list fills in steadily.
const BATCH: usize = 200;
/// Bodies per request during background prefetch. Small, so a request from the reader
/// doesn't wait long behind one.
const BODY_BATCH: usize = 10;
/// Quiet time before a NOOP keeps the connection open.
pub const KEEPALIVE: Duration = Duration::from_secs(240);
/// Reconnect attempts before giving up.
const RECONNECT_ATTEMPTS: u32 = 5;
/// First wait between reconnect attempts. Each later wait doubles: 1s, 2s, 4s, 8s.
const RETRY_BASE: Duration = Duration::from_secs(1);

/// Work for the worker, sent by the GUI.
pub enum Command {
    /// Fetches the headers the cache lacks, newest first, after syncing changes.
    Load { limit: usize },
    /// Looks for messages newer than any already loaded.
    CheckNew,
    /// Fetches one body now, ahead of the background queues.
    FetchBody { uid: u32 },
    /// Applies a label change to every message in the given threads.
    Modify {
        thrids: Vec<u64>,
        action: LabelAction,
        labels: Vec<String>,
    },
    /// Marks every message in the given threads read or unread. The cache changes at once,
    /// and the server is told next, retrying until it confirms.
    SetSeen { thrids: Vec<u64>, seen: bool },
    /// The IDs of the threads in a range of the list, to check or uncheck them all.
    ThreadRange {
        filter: ThreadFilter,
        start: usize,
        count: usize,
        checked: bool,
    },
    /// Reads one window of the thread list from the cache.
    Window {
        filter: ThreadFilter,
        start: usize,
        count: usize,
    },
    /// Reads every cached message of one thread.
    Thread { thrid: u64 },
}

/// A row of the thread list: a conversation, its counts, and its newest message.
pub struct ThreadEntry {
    pub thrid: u64,
    /// Cached messages in the thread.
    pub count: usize,
    /// How many of them are unread.
    pub unread: usize,
    /// Whether any of them has an attachment.
    pub attachments: bool,
    pub head: Message,
}

/// What the worker reports back. Message data goes out only when the GUI asks for it.
pub enum Event {
    Connected,
    ConnectFailed(String),
    /// The connection dropped, and the worker is trying to reconnect.
    Reconnecting,
    Reconnected,
    /// The mailbox changed under us. The GUI must drop what it shows.
    Reset,
    /// A load has started. `total` is the number of messages in the mailbox.
    LoadStarted {
        total: usize,
    },
    LoadFinished,
    /// A check finished. Anything new is in the cache.
    CheckFinished,
    /// Messages that arrived in a check, newer than anything we had. Used for notifications.
    NewArrivals(Vec<Message>),
    /// `cached` headers are held locally, out of `total` messages in the mailbox.
    Progress {
        cached: usize,
        total: usize,
    },
    Body {
        uid: u32,
        raw: Vec<u8>,
    },
    /// Every label on the account, including ones with no messages.
    LabelList(Vec<String>),
    Labels {
        updates: Vec<MessageUpdate>,
        command: String,
    },
    /// Per label: threads with it, and how many of those are unread. Then the totals.
    LabelCounts {
        labels: Vec<(String, usize, usize)>,
        threads: usize,
        unread: usize,
    },
    /// The threads in a range of the list, for a shift-click. `checked` is the state to apply.
    ThreadRange {
        filter: ThreadFilter,
        thrids: Vec<u64>,
        checked: bool,
    },
    /// One window of the thread list, starting at `start`, out of `total` threads.
    Window {
        filter: ThreadFilter,
        start: usize,
        threads: Vec<ThreadEntry>,
        total: usize,
    },
    /// Every cached message of one thread, newest first.
    Thread {
        thrid: u64,
        messages: Vec<Message>,
    },
    /// Messages were removed on the server.
    Removed(Vec<u32>),
    /// Whether live updates (IDLE) are running, so the GUI can stop polling.
    IdleState(bool),
    Error(String),
}

/// The GUI's end of a worker.
pub struct Link {
    pub commands: Sender<Command>,
    pub events: Receiver<Event>,
}

/// Opens a new connection. Called once at start, and again on each reconnect.
type Connector = Box<dyn FnMut() -> Result<Box<dyn MailSource>, String> + Send>;

/// Opens the account's cache file, returning the path too. The error is for the user.
/// Opens the account's cache, checking it first if it wasn't closed cleanly. Returns the
/// notes about any repair or replacement, for the user.
pub fn open_cache(email: &str) -> Result<(Store, Vec<String>), String> {
    let path = account_cache_path(email).ok_or_else(|| {
        "No data directory found; the cache is kept in memory for this session.".to_string()
    })?;
    Store::open_checked(&path).map_err(|e| {
        format!("Could not open the cache; it is kept in memory for this session. {e}")
    })
}

/// The account's cache, plus any startup errors to send once connected. If the cache
/// file can't be used, a cache in memory keeps the list working for this session.
fn cache_for(email: &str) -> (Option<Store>, Vec<Event>) {
    match open_cache(email) {
        Ok((store, notes)) => (Some(store), notes.into_iter().map(Event::Error).collect()),
        Err(e) => (
            Store::open_in_memory(DEFAULT_BODY_CAP).ok(),
            vec![Event::Error(e)],
        ),
    }
}

/// Starts a worker that signs in to Gmail and uses that account's cache.
pub fn spawn_gmail(user: String, password: String, ctx: egui::Context) -> Link {
    let (store, startup) = cache_for(&user);
    spawn(
        ctx,
        Box::new(move || live::connect_gmail(&user, &password)),
        KEEPALIVE,
        RETRY_BASE,
        store,
        startup,
    )
}

fn spawn(
    ctx: egui::Context,
    connect: Connector,
    keepalive: Duration,
    retry_base: Duration,
    store: Option<Store>,
    startup: Vec<Event>,
) -> Link {
    let (cmd_tx, cmd_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    let emit = Emitter { tx: ev_tx, ctx };
    thread::spawn(move || {
        let mut connect = connect;
        match connect() {
            Ok(source) => {
                emit.send(Event::Connected);
                for ev in startup {
                    emit.send(ev);
                }
                Worker {
                    source: Some(source),
                    connect,
                    state: State::default(),
                    emit,
                    keepalive,
                    retry_base,
                    store,
                }
                .run(cmd_rx);
            }
            Err(e) => emit.send(Event::ConnectFailed(e)),
        }
    });
    Link {
        commands: cmd_tx,
        events: ev_rx,
    }
}

/// Sends events and wakes the GUI.
struct Emitter {
    tx: Sender<Event>,
    ctx: egui::Context,
}

impl Emitter {
    fn send(&self, ev: Event) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }
}

#[derive(Default)]
struct State {
    /// Newest messages to show on load.
    limit: usize,
    uidvalidity: Option<u32>,
    /// Highest UID known to exist.
    highest: Option<u32>,
    /// Last synced HIGHESTMODSEQ, for CONDSTORE. `None` until the server reports one.
    modseq: Option<u64>,
    /// An expunge was seen, so the next check must reconcile UIDs.
    reconcile: bool,
    /// IDLE failed on this server. Periodic checks are used instead.
    idle_off: bool,
    /// Whether the GUI has been told live updates are on.
    idle_reported: bool,
    /// Messages in the mailbox, as reported by SELECT.
    total: usize,
    /// Headers held locally, from the cache or fetched.
    cached: usize,
    /// UIDs whose headers haven't been fetched, newest first.
    pending_headers: VecDeque<u32>,
    /// UIDs whose bodies to prefetch when idle, next first.
    prefetch: VecDeque<u32>,
    /// UIDs of cached messages whose attachment information is still unknown, next first.
    pending_structure: VecDeque<u32>,
}

struct Worker {
    /// `None` after a drop, until a reconnect succeeds.
    source: Option<Box<dyn MailSource>>,
    connect: Connector,
    state: State,
    emit: Emitter,
    keepalive: Duration,
    retry_base: Duration,
    /// `None` if the cache couldn't be opened. Everything still works without it.
    store: Option<Store>,
}

impl Worker {
    fn run(mut self, commands: Receiver<Command>) {
        self.emit_label_counts();
        loop {
            self.report_idle_state();
            if self.idle_work_pending() {
                match commands.try_recv() {
                    Ok(cmd) => self.dispatch(cmd),
                    Err(TryRecvError::Empty) => self.idle_step(),
                    Err(TryRecvError::Disconnected) => return,
                }
            } else if self.can_idle() {
                match commands.try_recv() {
                    Ok(cmd) => self.dispatch(cmd),
                    Err(TryRecvError::Empty) => self.idle_tick(),
                    Err(TryRecvError::Disconnected) => return,
                }
            } else {
                match commands.recv_timeout(self.keepalive) {
                    Ok(cmd) => self.dispatch(cmd),
                    Err(RecvTimeoutError::Timeout) => self.keepalive_tick(),
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            self.drain_notices();
        }
    }

    /// Whether to wait for server pushes. That needs a loaded mailbox, a server
    /// that supports IDLE, and no IDLE failure so far.
    fn can_idle(&self) -> bool {
        !self.state.idle_off
            && self.state.limit > 0
            && self.source.as_ref().is_some_and(|s| s.supports_idle())
    }

    fn report_idle_state(&mut self) {
        let idle = self.can_idle();
        if idle != self.state.idle_reported {
            self.state.idle_reported = idle;
            self.emit.send(Event::IdleState(idle));
        }
    }

    /// Waits for the server to push a change. Changes trigger a sync.
    fn idle_tick(&mut self) {
        match self.with_source(|_, src| src.idle_poll(IDLE_POLL)) {
            Ok(notices) => self.sync_from(&notices),
            Err(SourceError::Disconnected(_)) => {
                self.reconnect();
            }
            Err(e) => {
                // IDLE didn't work here. Fall back to periodic checks.
                self.state.idle_off = true;
                self.emit.send(Event::Error(format!(
                    "Live updates stopped ({e}); checking periodically instead."
                )));
            }
        }
    }

    /// Notices that arrived while a command ended IDLE.
    fn drain_notices(&mut self) {
        let notices = match self.source.as_deref_mut() {
            Some(src) => src.take_notices(),
            None => Vec::new(),
        };
        self.sync_from(&notices);
    }

    /// Runs a sync after the server reported a change.
    fn sync_from(&mut self, notices: &[IdleNotice]) {
        if notices.is_empty() || self.state.limit == 0 {
            return;
        }
        if notices.contains(&IdleNotice::Expunged) {
            self.state.reconcile = true;
        }
        self.dispatch(Command::CheckNew);
    }

    fn idle_work_pending(&self) -> bool {
        !self.state.pending_headers.is_empty()
            || !self.state.pending_structure.is_empty()
            || !self.state.prefetch.is_empty()
            || self.store_has_pending_seen()
    }

    /// The cached messages whose attachment information is unknown, from the cache.
    fn missing_attachment_info(&self) -> VecDeque<u32> {
        match self
            .store
            .as_ref()
            .map(|s| s.uids_missing_attachment_info())
        {
            Some(Ok(uids)) => uids.into_iter().collect(),
            _ => VecDeque::new(),
        }
    }

    /// Whether read-state changes are waiting to reach the server.
    fn store_has_pending_seen(&self) -> bool {
        self.store
            .as_ref()
            .is_some_and(|s| s.has_pending_seen().unwrap_or(false))
    }

    /// Runs `f` with the source taken out of the worker. A dropped connection is
    /// discarded here, so the caller reconnects.
    fn with_source<T>(
        &mut self,
        f: impl FnOnce(&mut Self, &mut dyn MailSource) -> Result<T, SourceError>,
    ) -> Result<T, SourceError> {
        let mut source = self
            .source
            .take()
            .ok_or_else(|| SourceError::Disconnected("not connected".into()))?;
        let result = f(self, source.as_mut());
        if !matches!(result, Err(SourceError::Disconnected(_))) {
            self.source = Some(source);
        }
        result
    }

    /// Runs one command. A dropped connection gets one reconnect and one retry.
    fn dispatch(&mut self, cmd: Command) {
        // Reads from the cache need no connection, so they never wait on a reconnect.
        match &cmd {
            Command::Window {
                filter,
                start,
                count,
            } => {
                self.op_window(filter, *start, *count);
                return;
            }
            Command::Thread { thrid } => {
                self.op_thread(*thrid);
                return;
            }
            Command::ThreadRange {
                filter,
                start,
                count,
                checked,
            } => {
                self.op_thread_range(filter, *start, *count, *checked);
                return;
            }
            _ => {}
        }
        let mut retried = false;
        loop {
            if self.source.is_none() && !self.reconnect() {
                return;
            }
            match self.with_source(|w, src| w.run_command(src, &cmd)) {
                Ok(()) => return,
                Err(SourceError::Disconnected(_)) if !retried => retried = true,
                Err(e) => {
                    self.emit.send(Event::Error(e.to_string()));
                    return;
                }
            }
        }
    }

    fn run_command(&mut self, src: &mut dyn MailSource, cmd: &Command) -> Result<(), SourceError> {
        match cmd {
            Command::Load { limit } => self.op_load(src, *limit),
            Command::CheckNew => self.op_check(src),
            Command::FetchBody { uid } => self.op_fetch_body(src, *uid),
            Command::Modify {
                thrids,
                action,
                labels,
            } => {
                let uids = match self.store.as_ref() {
                    Some(store) => store.uids_for_threads(thrids),
                    None => Ok(Vec::new()),
                };
                match uids {
                    Ok(uids) if uids.is_empty() => Ok(()),
                    Ok(uids) => self.op_modify(src, &uids, *action, labels),
                    Err(e) => {
                        self.emit
                            .send(Event::Error(format!("cache read failed: {e}")));
                        Ok(())
                    }
                }
            }
            Command::SetSeen { thrids, seen } => self.op_set_seen(src, thrids, *seen),
            // Cache reads are answered in `dispatch`, before a connection is needed.
            Command::Window { .. } | Command::Thread { .. } | Command::ThreadRange { .. } => Ok(()),
        }
    }

    /// One unit of background work: a batch of headers if any are pending,
    /// otherwise one body.
    fn idle_step(&mut self) {
        // Read-state changes come first: they're what the user has asked for.
        if self.store_has_pending_seen() {
            match self.with_source(|w, src| w.flush_seen(src)) {
                Ok(()) => {}
                // The changes stay queued, and the next step tries again after a reconnect.
                Err(SourceError::Disconnected(_)) => {
                    self.reconnect();
                }
                Err(e) => self
                    .emit
                    .send(Event::Error(format!("Read state not sent: {e}"))),
            }
            return;
        }
        let result = self.with_source(|w, src| {
            if !w.state.pending_headers.is_empty() {
                w.op_fill_headers(src)
            } else if !w.state.pending_structure.is_empty() {
                w.op_fill_structure(src)
            } else {
                w.op_prefetch_batch(src)
            }
        });
        match result {
            Ok(()) => {}
            Err(SourceError::Disconnected(_)) => {
                // Reconnect now, so the queued work carries on. Give up if that fails.
                if !self.reconnect() {
                    self.clear_background();
                }
            }
            Err(e) => {
                self.clear_background();
                self.emit
                    .send(Event::Error(format!("Background sync stopped: {e}")));
            }
        }
    }

    fn clear_background(&mut self) {
        self.state.pending_headers.clear();
        self.state.prefetch.clear();
    }

    /// Sends NOOP after a quiet period. A drop found here is reconnected now,
    /// so the next command doesn't pay for it.
    fn keepalive_tick(&mut self) {
        if self.source.is_none() {
            return;
        }
        match self.with_source(|_, src| src.noop()) {
            Ok(()) => {}
            Err(SourceError::Disconnected(_)) => {
                self.reconnect();
            }
            Err(e) => self.emit.send(Event::Error(e.to_string())),
        }
    }

    /// Opens a new connection with backoff. Returns whether it succeeded.
    fn reconnect(&mut self) -> bool {
        self.source = None;
        self.emit.send(Event::Reconnecting);
        let mut last_error = String::new();
        for attempt in 0..RECONNECT_ATTEMPTS {
            if attempt > 0 {
                thread::sleep(self.retry_base * 2u32.pow(attempt - 1));
            }
            match (self.connect)() {
                Ok(source) => {
                    self.source = Some(source);
                    self.emit.send(Event::Reconnected);
                    return true;
                }
                Err(e) => last_error = e,
            }
        }
        self.emit
            .send(Event::Error(format!("Could not reconnect: {last_error}")));
        false
    }

    fn emit_progress(&self) {
        self.emit.send(Event::Progress {
            cached: self.state.cached,
            total: self.state.total,
        });
        self.emit_label_counts();
    }

    /// Sends the cache's label totals, so the GUI doesn't have to count messages.
    fn emit_label_counts(&self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let counts = store
            .label_thread_counts()
            .and_then(|labels| store.thread_totals().map(|totals| (labels, totals)));
        match counts {
            Ok((labels, (threads, unread))) => self.emit.send(Event::LabelCounts {
                labels,
                threads,
                unread,
            }),
            Err(e) => self
                .emit
                .send(Event::Error(format!("cache read failed: {e}"))),
        }
    }

    /// Records the mailbox's size and UIDVALIDITY. Returns `true` if UIDVALIDITY
    /// changed, which means every UID in the view is stale.
    fn begin_mailbox(&mut self, info: MailboxInfo) -> bool {
        self.state.total = info.exists as usize;
        let changed = self
            .state
            .uidvalidity
            .is_some_and(|v| v != info.uidvalidity);
        if changed {
            self.clear_background();
            self.state.cached = 0;
            self.state.highest = None;
            self.state.modseq = None;
            self.emit.send(Event::Reset);
        }

        if let Some(store) = self.store.as_mut() {
            let stored = match store.mailbox_state() {
                Ok(stored) => stored,
                // Not knowing the stored state is not the same as a different mailbox.
                // Keep the cache as it is: invalidating it would mean re-fetching every
                // header. Without a modseq, changes made elsewhere wait for a later sync.
                Err(e) => {
                    self.emit.send(Event::Error(format!(
                        "Could not read the cache's sync state ({e}). The cache is kept as it is."
                    )));
                    self.state.uidvalidity = Some(info.uidvalidity);
                    return changed;
                }
            };
            if let Some(st) = stored.filter(|st| st.uidvalidity == info.uidvalidity) {
                // A restart with a valid cache: resume from the last synced modseq,
                // so changes made while the app was closed get picked up.
                if self.state.modseq.is_none() {
                    self.state.modseq = st.highest_modseq;
                }
            } else {
                self.state.modseq = None;
                // The cache's UIDs belong to a different UIDVALIDITY. Keep the
                // messages, drop the UIDs, and let the sync assign new ones.
                if let Err(e) = store.invalidate_uids() {
                    self.emit
                        .send(Event::Error(format!("cache reset failed: {e}")));
                }
                // Record the UIDVALIDITY now, not at the end of the load. If the
                // app quits mid-load, the next start can then keep the UIDs.
                let state = MailboxState {
                    uidvalidity: info.uidvalidity,
                    highest_uid: None,
                    highest_modseq: None,
                };
                if let Err(e) = store.set_mailbox_state(state) {
                    self.emit
                        .send(Event::Error(format!("cache write failed: {e}")));
                }
            }
        }
        self.state.uidvalidity = Some(info.uidvalidity);
        changed
    }

    fn save_sync(&mut self) {
        let (Some(uidvalidity), Some(store)) = (self.state.uidvalidity, self.store.as_ref()) else {
            return;
        };
        let state = MailboxState {
            uidvalidity,
            highest_uid: self.state.highest,
            highest_modseq: self.state.modseq,
        };
        if let Err(e) = store.set_mailbox_state(state) {
            self.emit
                .send(Event::Error(format!("cache write failed: {e}")));
        }
    }

    /// Writes headers to the cache. A failure is reported but doesn't stop the sync.
    fn cache_upsert(&mut self, msgs: &[Message]) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        let cached: Vec<_> = msgs.iter().map(Message::to_cached).collect();
        if let Err(e) = store.upsert_messages(&cached) {
            self.emit
                .send(Event::Error(format!("cache write failed: {e}")));
        }
    }

    /// UIDs with cached headers.
    fn cached_uid_set(&mut self) -> UidSet {
        let Some(store) = self.store.as_ref() else {
            return UidSet::new();
        };
        match store.cached_uid_set() {
            Ok(uids) => uids,
            Err(e) => {
                self.emit
                    .send(Event::Error(format!("cache read failed: {e}")));
                UidSet::new()
            }
        }
    }

    fn cached_body(&mut self, uid: u32) -> Option<Vec<u8>> {
        let store = self.store.as_mut()?;
        let result = store.msgid_for_uid(uid).and_then(|msgid| match msgid {
            Some(msgid) => store.get_body(msgid),
            None => Ok(None),
        });
        match result {
            Ok(body) => body,
            Err(e) => {
                self.emit
                    .send(Event::Error(format!("cache read failed: {e}")));
                None
            }
        }
    }

    /// Writes a batch of bodies to the cache in one transaction.
    fn cache_bodies(&mut self, bodies: &[(u32, Vec<u8>)]) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        let mut pairs: Vec<(u64, &[u8])> = Vec::with_capacity(bodies.len());
        for (uid, raw) in bodies {
            match store.msgid_for_uid(*uid) {
                Ok(Some(msgid)) => pairs.push((msgid, raw.as_slice())),
                Ok(None) => {}
                Err(e) => {
                    self.emit
                        .send(Event::Error(format!("cache write failed: {e}")));
                    return;
                }
            }
        }
        if let Err(e) = store.put_bodies(&pairs) {
            self.emit
                .send(Event::Error(format!("cache write failed: {e}")));
        }
    }

    fn cache_body(&mut self, uid: u32, raw: &[u8]) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        let result = store.msgid_for_uid(uid).and_then(|msgid| match msgid {
            Some(msgid) => store.put_body(msgid, raw),
            None => Ok(()),
        });
        if let Err(e) = result {
            self.emit
                .send(Event::Error(format!("cache write failed: {e}")));
        }
    }

    /// Writes a message's labels and read state to the cache.
    fn cache_update(&mut self, u: &MessageUpdate) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        let result = store.msgid_for_uid(u.uid).and_then(|msgid| match msgid {
            Some(msgid) => store
                .set_labels(msgid, &u.labels)
                .and_then(|()| store.set_seen(msgid, u.seen)),
            None => Ok(()),
        });
        if let Err(e) = result {
            self.emit
                .send(Event::Error(format!("cache write failed: {e}")));
        }
    }

    fn remove_cached(&mut self, uids: &[u32]) {
        if uids.is_empty() {
            return;
        }
        let Some(store) = self.store.as_mut() else {
            return;
        };
        if let Err(e) = store.remove_uids(uids) {
            self.emit
                .send(Event::Error(format!("cache write failed: {e}")));
        }
        self.emit_label_counts();
    }

    /// Applies changes made since the last sync, then fetches whatever headers the
    /// cache lacks. The newest `limit` are fetched now. The rest are queued for the
    /// background. The GUI reads the cache itself, so only progress is sent.
    fn op_load(&mut self, src: &mut dyn MailSource, limit: usize) -> Result<(), SourceError> {
        self.state.limit = limit;
        let info = src.select()?;
        self.begin_mailbox(info);
        self.report_labels(src)?;

        let all = src.search_from(1)?;
        self.state.highest = all.last().copied();
        let server: UidSet = all.iter().copied().collect();

        // Messages deleted on the server drop out of the cache.
        let stale: Vec<u32> = self
            .cached_uid_set()
            .iter()
            .filter(|&u| !server.contains(u))
            .collect();
        self.remove_cached(&stale);

        self.emit.send(Event::LoadStarted { total: all.len() });

        // Changes made since the last sync: labels and read state, from elsewhere.
        if let (Some(modseq), true) = (self.state.modseq, src.supports_condstore()) {
            self.sync_changes(src, modseq)?;
        }

        let cached = self.cached_uid_set();
        self.state.cached = all.iter().filter(|&&u| cached.contains(u)).count();
        self.emit_progress();

        // Headers we don't have, newest first.
        let mut missing: VecDeque<u32> = all
            .iter()
            .rev()
            .filter(|&&u| !cached.contains(u))
            .copied()
            .collect();
        let urgent: Vec<u32> = (0..limit.min(missing.len()))
            .filter_map(|_| missing.pop_front())
            .collect();
        for chunk in urgent.chunks(BATCH) {
            let msgs = src.fetch_headers(chunk)?;
            self.cache_upsert(&msgs);
            self.state.cached += msgs.len();
            self.emit_progress();
        }
        self.state.pending_headers = missing;
        self.state.pending_structure = self.missing_attachment_info();
        self.state.prefetch = all.iter().rev().take(limit).copied().collect();
        self.state.modseq = info.highest_modseq;

        self.emit.send(Event::LoadFinished);
        self.emit_progress();
        self.save_sync();
        Ok(())
    }

    /// Sends the account's full label list. A failure here doesn't stop the
    /// mailbox sync, so only a dropped connection is passed on.
    fn report_labels(&mut self, src: &mut dyn MailSource) -> Result<(), SourceError> {
        match src.list_labels() {
            Ok(labels) => self.emit.send(Event::LabelList(labels)),
            Err(e @ SourceError::Disconnected(_)) => return Err(e),
            Err(e) => self
                .emit
                .send(Event::Error(format!("Could not list labels: {e}"))),
        }
        Ok(())
    }

    /// Looks for messages newer than the highest UID we know about, then for
    /// changes to messages we have (labels and read state, via CONDSTORE), and
    /// removals after an expunge.
    fn op_check(&mut self, src: &mut dyn MailSource) -> Result<(), SourceError> {
        let info = src.select()?;
        if self.begin_mailbox(info) {
            return self.op_load(src, self.state.limit);
        }
        self.report_labels(src)?;
        let Some(highest) = self.state.highest else {
            return self.op_load(src, self.state.limit);
        };

        let new: Vec<u32> = src
            .search_from(highest + 1)?
            .into_iter()
            .filter(|&u| u > highest)
            .collect();
        let arrivals = if new.is_empty() {
            Vec::new()
        } else {
            src.fetch_headers(&new)?
        };
        self.cache_upsert(&arrivals);
        self.state.cached += arrivals.len();
        if let Some(&max) = new.last() {
            self.state.highest = Some(max);
        }
        // Newest first at the front of the body queue.
        for &uid in &new {
            self.state.prefetch.push_front(uid);
        }

        if let (Some(modseq), true) = (self.state.modseq, src.supports_condstore()) {
            self.sync_changes(src, modseq)?;
        }

        if std::mem::take(&mut self.state.reconcile) {
            self.reconcile(src)?;
        }
        if let Some(modseq) = info.highest_modseq {
            self.state.modseq = Some(modseq);
        }

        if !arrivals.is_empty() {
            self.emit.send(Event::NewArrivals(arrivals));
        }
        self.emit.send(Event::CheckFinished);
        self.emit_progress();
        self.save_sync();
        Ok(())
    }

    /// Applies changes made since `modseq`: labels and read state for messages
    /// we have, and full headers for messages we don't.
    fn sync_changes(&mut self, src: &mut dyn MailSource, modseq: u64) -> Result<(), SourceError> {
        let changes = match src.changed_since(modseq) {
            Ok(changes) => changes,
            Err(e @ SourceError::Disconnected(_)) => return Err(e),
            Err(e) => {
                self.emit
                    .send(Event::Error(format!("Could not sync changes: {e}")));
                return Ok(());
            }
        };
        // Nothing changed (the usual case for a periodic check): nothing to look up.
        if changes.is_empty() {
            return Ok(());
        }
        let cached = self.cached_uid_set();
        let (known, unknown): (Vec<MessageUpdate>, Vec<MessageUpdate>) =
            changes.into_iter().partition(|c| cached.contains(c.uid));

        if !unknown.is_empty() {
            let uids: Vec<u32> = unknown.iter().map(|c| c.uid).collect();
            let fetched = src.fetch_headers(&uids)?;
            self.cache_upsert(&fetched);
            self.state.cached += fetched.len();
            // Those messages are no longer waiting for a background header fetch.
            self.state.pending_headers.retain(|u| !uids.contains(u));
        }
        for c in &known {
            self.cache_update(c);
        }
        if !known.is_empty() {
            self.emit.send(Event::Labels {
                updates: known,
                command: format!("UID FETCH 1:* (CHANGEDSINCE {modseq})"),
            });
        }
        Ok(())
    }

    /// After an expunge: drops cached messages the server no longer has.
    fn reconcile(&mut self, src: &mut dyn MailSource) -> Result<(), SourceError> {
        let server: UidSet = src.search_from(1)?.into_iter().collect();
        let stale: Vec<u32> = self
            .cached_uid_set()
            .iter()
            .filter(|&u| !server.contains(u))
            .collect();
        self.remove_cached(&stale);
        self.state.cached = self.state.cached.saturating_sub(stale.len());
        self.state.pending_headers.retain(|&u| server.contains(u));
        self.state.prefetch.retain(|&u| server.contains(u));
        self.state.highest = server.iter().max();
        if !stale.is_empty() {
            self.emit.send(Event::Removed(stale));
        }
        self.emit_progress();
        Ok(())
    }

    /// Fetches the attachment information for the next batch of cached messages that lack it.
    fn op_fill_structure(&mut self, src: &mut dyn MailSource) -> Result<(), SourceError> {
        let n = BATCH.min(self.state.pending_structure.len());
        let batch: Vec<u32> = self.state.pending_structure.drain(..n).collect();
        match src.fetch_structures(&batch) {
            Ok(flags) => {
                if let Some(store) = self.store.as_mut()
                    && let Err(e) = store.set_attachments(&flags)
                {
                    self.emit
                        .send(Event::Error(format!("cache write failed: {e}")));
                }
                self.emit_progress();
                Ok(())
            }
            Err(e @ SourceError::Disconnected(_)) => {
                for uid in batch.iter().rev() {
                    self.state.pending_structure.push_front(*uid);
                }
                Err(e)
            }
            // This source can't read attachment information, or the server refused the
            // batch. The messages stay unknown, and the next load tries them again.
            Err(_) => Ok(()),
        }
    }

    /// Fetches the next batch of pending headers. On failure the batch goes back
    /// on the queue.
    fn op_fill_headers(&mut self, src: &mut dyn MailSource) -> Result<(), SourceError> {
        let n = BATCH.min(self.state.pending_headers.len());
        let batch: Vec<u32> = self.state.pending_headers.drain(..n).collect();
        match src.fetch_headers(&batch) {
            Ok(msgs) => {
                self.cache_upsert(&msgs);
                self.state.cached += msgs.len();
                self.emit_progress();
                Ok(())
            }
            Err(e) => {
                for uid in batch.iter().rev() {
                    self.state.pending_headers.push_front(*uid);
                }
                Err(e)
            }
        }
    }

    /// Fetches one body, from the cache if possible. On failure it goes back on the queue.
    /// Fetches the next batch of bodies that aren't cached yet, in one request. Cached
    /// bodies are sent straight away. A batch is small, so a request from the reader
    /// waits behind one request at most.
    fn op_prefetch_batch(&mut self, src: &mut dyn MailSource) -> Result<(), SourceError> {
        let mut wanted: Vec<u32> = Vec::new();
        while wanted.len() < BODY_BATCH {
            let Some(uid) = self.state.prefetch.pop_front() else {
                break;
            };
            match self.cached_body(uid) {
                Some(raw) => self.emit.send(Event::Body { uid, raw }),
                None => wanted.push(uid),
            }
        }
        if wanted.is_empty() {
            return Ok(());
        }
        match src.fetch_bodies(&wanted) {
            Ok(bodies) => {
                // Anything the server didn't return was expunged since it was listed.
                // It's skipped here, and opening it reports the error.
                self.cache_bodies(&bodies);
                for (uid, raw) in bodies {
                    self.emit.send(Event::Body { uid, raw });
                }
                Ok(())
            }
            // The connection dropped: try the batch again after reconnecting.
            Err(e @ SourceError::Disconnected(_)) => {
                for uid in wanted.into_iter().rev() {
                    self.state.prefetch.push_front(uid);
                }
                Err(e)
            }
            // Any other failure skips the batch. Each message can still be opened on demand.
            Err(_) => Ok(()),
        }
    }

    fn op_fetch_body(&mut self, src: &mut dyn MailSource, uid: u32) -> Result<(), SourceError> {
        self.state.prefetch.retain(|&u| u != uid);
        if let Some(raw) = self.cached_body(uid) {
            self.emit.send(Event::Body { uid, raw });
            return Ok(());
        }
        let raw = src.fetch_body(uid)?;
        self.cache_body(uid, &raw);
        self.emit.send(Event::Body { uid, raw });
        Ok(())
    }

    /// Queues a read-state change in the cache, then sends it.
    fn op_set_seen(
        &mut self,
        src: &mut dyn MailSource,
        thrids: &[u64],
        seen: bool,
    ) -> Result<(), SourceError> {
        if let Some(store) = self.store.as_mut()
            && let Err(e) = store.set_seen_for_threads(thrids, seen)
        {
            self.emit
                .send(Event::Error(format!("cache write failed: {e}")));
            return Ok(());
        }
        self.flush_seen(src)
    }

    /// Sends queued read-state changes to the server. A change leaves the queue only once
    /// the server confirms it, or refuses it outright, so a dropped connection loses nothing.
    fn flush_seen(&mut self, src: &mut dyn MailSource) -> Result<(), SourceError> {
        let Some(store) = self.store.as_ref() else {
            return Ok(());
        };
        let pending = match store.pending_seen() {
            Ok(pending) => pending,
            Err(e) => {
                self.emit
                    .send(Event::Error(format!("cache read failed: {e}")));
                return Ok(());
            }
        };
        for seen in [true, false] {
            let group: Vec<(u64, u32)> = pending
                .iter()
                .filter(|p| p.2 == seen)
                .map(|p| (p.0, p.1))
                .collect();
            if group.is_empty() {
                continue;
            }
            let msgids: Vec<u64> = group.iter().map(|g| g.0).collect();
            let uids: Vec<u32> = group.iter().map(|g| g.1).collect();
            match src.set_seen(&uids, seen) {
                Ok((command, updates)) => {
                    self.clear_pending(&msgids);
                    for u in &updates {
                        self.cache_update(u);
                    }
                    self.emit.send(Event::Labels { updates, command });
                }
                Err(e @ SourceError::Disconnected(_)) => return Err(e),
                // The server refuses this change, for example because the message is gone.
                // Drop it, and say so.
                Err(e) => {
                    self.clear_pending(&msgids);
                    self.emit
                        .send(Event::Error(format!("Could not update read state: {e}")));
                }
            }
        }
        Ok(())
    }

    fn clear_pending(&mut self, msgids: &[u64]) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        if let Err(e) = store.clear_pending_seen(msgids) {
            self.emit
                .send(Event::Error(format!("cache write failed: {e}")));
        }
    }

    fn op_modify(
        &mut self,
        src: &mut dyn MailSource,
        uids: &[u32],
        action: LabelAction,
        labels: &[String],
    ) -> Result<(), SourceError> {
        let (command, updates) = src.modify_labels(uids, action, labels)?;
        for u in &updates {
            self.cache_update(u);
        }
        self.emit_label_counts();
        self.emit.send(Event::Labels { updates, command });
        Ok(())
    }

    /// Sends the thread IDs of a range of the list, read from the cache.
    fn op_thread_range(&self, filter: &ThreadFilter, start: usize, count: usize, checked: bool) {
        let thrids: Vec<u64> = match self.store.as_ref() {
            Some(store) => match store.thread_page(filter, start, count) {
                Ok(page) => page.into_iter().map(|t| t.thrid).collect(),
                Err(e) => {
                    self.emit
                        .send(Event::Error(format!("cache read failed: {e}")));
                    return;
                }
            },
            None => Vec::new(),
        };
        self.emit.send(Event::ThreadRange {
            filter: filter.clone(),
            thrids,
            checked,
        });
    }

    /// Sends one window of the thread list, read from the cache.
    fn op_window(&self, filter: &ThreadFilter, start: usize, count: usize) {
        let Some(store) = self.store.as_ref() else {
            self.emit.send(Event::Window {
                filter: filter.clone(),
                start,
                threads: Vec::new(),
                total: 0,
            });
            return;
        };
        let result = store.thread_count(filter).and_then(|total| {
            store
                .thread_page(filter, start, count)
                .map(|page| (total, page))
        });
        match result {
            Ok((total, page)) => {
                let threads: Vec<ThreadEntry> = page
                    .into_iter()
                    .filter_map(|t| {
                        let head = Message::from_cached(t.head)?;
                        Some(ThreadEntry {
                            thrid: t.thrid,
                            count: t.count,
                            unread: t.unread,
                            attachments: t.attachments,
                            head,
                        })
                    })
                    .collect();
                self.emit.send(Event::Window {
                    filter: filter.clone(),
                    start,
                    threads,
                    total,
                });
            }
            Err(e) => self
                .emit
                .send(Event::Error(format!("cache read failed: {e}"))),
        }
    }

    /// Sends every cached message of one thread.
    fn op_thread(&self, thrid: u64) {
        let messages: Vec<Message> = match self.store.as_ref() {
            Some(store) => match store.thread_messages(thrid) {
                Ok(list) => list.into_iter().filter_map(Message::from_cached).collect(),
                Err(e) => {
                    self.emit
                        .send(Event::Error(format!("cache read failed: {e}")));
                    return;
                }
            },
            None => Vec::new(),
        };
        self.emit.send(Event::Thread { thrid, messages });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockSource;
    use gmail_imap_proto::store::DEFAULT_BODY_CAP;
    use std::collections::HashSet;

    /// Wraps the demo mailbox and fails with a dropped connection while `fail` is set.
    struct Flaky {
        inner: MockSource,
        fail: bool,
    }

    impl Flaky {
        fn check(&self) -> Result<(), SourceError> {
            if self.fail {
                Err(SourceError::Disconnected("dropped".into()))
            } else {
                Ok(())
            }
        }
    }

    impl MailSource for Flaky {
        fn select(&mut self) -> Result<MailboxInfo, SourceError> {
            self.check()?;
            self.inner.select()
        }
        fn noop(&mut self) -> Result<(), SourceError> {
            self.check()?;
            self.inner.noop()
        }
        fn search_from(&mut self, start: u32) -> Result<Vec<u32>, SourceError> {
            self.check()?;
            self.inner.search_from(start)
        }
        fn fetch_headers(&mut self, uids: &[u32]) -> Result<Vec<Message>, SourceError> {
            self.check()?;
            self.inner.fetch_headers(uids)
        }
        fn fetch_body(&mut self, uid: u32) -> Result<Vec<u8>, SourceError> {
            self.check()?;
            self.inner.fetch_body(uid)
        }
        fn modify_labels(
            &mut self,
            uids: &[u32],
            action: LabelAction,
            labels: &[String],
        ) -> Result<(String, Vec<MessageUpdate>), SourceError> {
            self.check()?;
            self.inner.modify_labels(uids, action, labels)
        }
    }

    /// Retry wait for tests, so reconnect tests don't sleep for seconds.
    const FAST: Duration = Duration::from_millis(1);

    fn next(link: &Link) -> Event {
        link.events
            .recv_timeout(Duration::from_secs(5))
            .expect("worker went quiet")
    }

    /// The next event that isn't progress, which tests don't care about.
    fn next_significant(link: &Link) -> Event {
        loop {
            match next(link) {
                Event::Progress { .. } | Event::LabelList(_) | Event::LabelCounts { .. } => {
                    continue;
                }
                ev => return ev,
            }
        }
    }

    /// A worker on the mock mailbox, with an in-memory cache.
    fn mock_link() -> Link {
        spawn(
            egui::Context::default(),
            Box::new(|| Ok(Box::new(MockSource::demo()) as Box<dyn MailSource>)),
            KEEPALIVE,
            FAST,
            Some(Store::open_in_memory(DEFAULT_BODY_CAP).unwrap()),
            Vec::new(),
        )
    }

    /// Reads one window of the thread list through the worker.
    fn window(
        link: &Link,
        filter: ThreadFilter,
        start: usize,
        count: usize,
    ) -> (Vec<ThreadEntry>, usize) {
        link.commands
            .send(Command::Window {
                filter,
                start,
                count,
            })
            .unwrap();
        loop {
            if let Event::Window { threads, total, .. } = next(link) {
                return (threads, total);
            }
        }
    }

    /// The whole thread list, read as one window.
    fn threads(link: &Link) -> (Vec<ThreadEntry>, usize) {
        window(link, ThreadFilter::All, 0, 500)
    }

    /// Cached messages in a list of threads.
    fn cached_messages(threads: &[ThreadEntry]) -> usize {
        threads.iter().map(|t| t.count).sum()
    }

    /// The thread that holds the message with this UID.
    fn thread_of(link: &Link, uid: u32) -> u64 {
        let (list, _) = threads(link);
        for t in list {
            link.commands
                .send(Command::Thread { thrid: t.thrid })
                .unwrap();
            loop {
                if let Event::Thread { thrid, messages } = next(link) {
                    if messages.iter().any(|m| m.uid == uid) {
                        return thrid;
                    }
                    break;
                }
            }
        }
        panic!("no thread holds uid {uid}");
    }

    /// Resident memory of this process, in MB (Linux; 0 elsewhere).
    fn rss_mb() -> f64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|kb| kb.parse::<f64>().ok())
            })
            .map_or(0.0, |kb| kb / 1024.0)
    }

    /// Where the memory goes with a large cache: a 250k-message cache file, the worker
    /// on it, and the GUI's reads. Run with `--ignored --nocapture`. Not a pass/fail test.
    #[test]
    #[ignore]
    fn memory_breakdown_with_a_large_cache() {
        const N: u32 = 250_000;
        let dir = std::env::temp_dir().join(format!("gmail-memory-test-{}", std::process::id()));
        let path = dir.join("cache.sqlite");
        let _ = std::fs::remove_file(&path);
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

        let start = rss_mb();
        let sent = gmail_imap_proto::store::sent_timestamp(&headers);
        {
            let mut store = Store::open(&path).unwrap();
            let labels: std::collections::BTreeSet<String> =
                ["\\Inbox".to_string(), "Work".to_string()].into();
            for first in (1..=N).step_by(200) {
                let batch: Vec<gmail_imap_proto::store::CachedMessage> = (first
                    ..(first + 200).min(N + 1))
                    .map(|uid| gmail_imap_proto::store::CachedMessage {
                        msgid: 1_000_000_000_000 + u64::from(uid),
                        attachment: None,
                        thrid: 9_000_000_000 + u64::from(uid / 3),
                        uid: Some(uid),
                        labels: labels.clone(),
                        from: "A Person".into(),
                        from_email: "a@example.com".into(),
                        subject: format!("Subject line for message {uid}"),
                        date: "Tue, 10 Nov 2026 09:30:00 +0000".into(),
                        headers: headers.clone(),
                        sent,
                        seen: uid % 3 == 0,
                    })
                    .collect();
                store.upsert_messages(&batch).unwrap();
            }
        }
        let after_fill = rss_mb();
        let file_mb = std::fs::metadata(&path).map_or(0.0, |m| m.len() as f64 / 1_048_576.0);

        // The worker on that cache. A mock source: this test never loads, so nothing is removed.
        let link = spawn(
            egui::Context::default(),
            Box::new(|| Ok(Box::new(MockSource::demo()) as Box<dyn MailSource>)),
            KEEPALIVE,
            FAST,
            Some(Store::open(&path).unwrap()),
            Vec::new(),
        );
        assert!(matches!(next_significant(&link), Event::Connected));
        let after_open = rss_mb();

        // The GUI's first window, then a scroll through the whole list one window at a time.
        let (first, total) = window(&link, ThreadFilter::All, 0, 200);
        let after_first_window = rss_mb();
        let mut offset = 0;
        while offset < total {
            let (page, _) = window(&link, ThreadFilter::All, offset, 200);
            if page.is_empty() {
                break;
            }
            offset += page.len();
        }
        let after_scroll = rss_mb();

        // One conversation open in the reader.
        let thrid = first[0].thrid;
        link.commands.send(Command::Thread { thrid }).unwrap();
        let thread_msgs = loop {
            if let Event::Thread { messages, .. } = next(&link) {
                break messages;
            }
        };
        let after_thread = rss_mb();

        // Bytes held per row of the list, from the strings it keeps.
        let row_heap: usize = first
            .iter()
            .map(|t| {
                let m = &t.head;
                m.from.capacity()
                    + m.subject.capacity()
                    + m.date.capacity()
                    + m.labels.iter().map(String::len).sum::<usize>()
            })
            .sum::<usize>()
            / first.len().max(1);
        let uids: UidSet = (1..=N).collect();

        println!("\n== memory breakdown, {N} cached messages in {file_mb:.0} MB on disk ==");
        println!("process before the test        {start:>8.1} MB");
        println!(
            "after writing the cache        {after_fill:>8.1} MB  (includes the writer's transient batches)"
        );
        println!("worker started on the cache    {after_open:>8.1} MB");
        println!("first window of 200 threads    {after_first_window:>8.1} MB  ({total} threads)");
        println!("scrolled the whole list        {after_scroll:>8.1} MB");
        println!(
            "one conversation open          {after_thread:>8.1} MB  ({} messages)",
            thread_msgs.len()
        );
        println!("\nper-object sizes:");
        println!(
            "  Message struct (inline)      {:>8} B",
            std::mem::size_of::<Message>()
        );
        println!(
            "  ThreadEntry struct (inline)  {:>8} B",
            std::mem::size_of::<ThreadEntry>()
        );
        println!("  heap per listed row (avg)    {row_heap:>8} B  (sender, subject, date, labels)");
        println!(
            "  UidSet for {N} UIDs           {:>8} KB",
            uids.len() / 8 / 1024
        );
        println!(
            "  HashSet<u32> for {N} UIDs     ~{:>7} KB  (estimate: buckets x 5 bytes)",
            (N as usize).next_power_of_two() * 5 / 1024
        );

        drop(link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_then_prefetches_every_body() {
        let link = mock_link();
        assert!(matches!(next_significant(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();

        assert!(matches!(
            next_significant(&link),
            Event::LoadStarted { total: 6 }
        ));
        while !matches!(next_significant(&link), Event::LoadFinished) {}

        let mut bodies = HashSet::new();
        while bodies.len() < 6 {
            match next_significant(&link) {
                Event::Body { uid, raw } => {
                    assert!(!raw.is_empty());
                    bodies.insert(uid);
                }
                _ => panic!("expected only body events"),
            }
        }
        assert_eq!(bodies, (1..=6).collect());

        // The headers went to the cache, and the list reads them from there.
        let (list, _) = threads(&link);
        assert_eq!(cached_messages(&list), 6);
    }

    #[test]
    fn background_fill_fetches_the_rest_of_the_headers() {
        let link = mock_link();
        assert!(matches!(next_significant(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 2 }).unwrap();

        assert!(matches!(
            next_significant(&link),
            Event::LoadStarted { total: 6 }
        ));
        // The newest two are cached before the load finishes.
        let mut cached_at_finish = None;
        loop {
            match next(&link) {
                Event::Progress { cached, .. } => cached_at_finish = Some(cached),
                Event::LoadFinished => break,
                Event::Body { .. } | Event::LabelList(_) | Event::LabelCounts { .. } => {}
                _ => panic!("unexpected event while loading"),
            }
        }
        assert_eq!(cached_at_finish, Some(2));

        // The rest arrive in the background, and the count reaches the mailbox size.
        loop {
            match next(&link) {
                Event::Progress {
                    cached: 6,
                    total: 6,
                } => break,
                Event::Body { .. }
                | Event::Progress { .. }
                | Event::LabelList(_)
                | Event::LabelCounts { .. } => {}
                _ => panic!("unexpected event during background fill"),
            }
        }
        let (list, _) = threads(&link);
        assert_eq!(cached_messages(&list), 6);
    }

    #[test]
    fn label_change_reports_new_labels() {
        let link = mock_link();
        assert!(matches!(next_significant(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        while !matches!(next_significant(&link), Event::LoadFinished) {}

        let thrid = thread_of(&link, 3);
        link.commands
            .send(Command::Modify {
                thrids: vec![thrid],
                action: LabelAction::Remove,
                labels: vec!["\\Inbox".to_string()],
            })
            .unwrap();
        loop {
            match next_significant(&link) {
                Event::Labels { updates, command } => {
                    assert!(command.starts_with("UID STORE"), "{command}");
                    let three = updates.iter().find(|u| u.uid == 3).expect("uid 3 updated");
                    assert!(!three.labels.contains("\\Inbox"));
                    break;
                }
                Event::Body { .. } => continue,
                _ => panic!("unexpected event"),
            }
        }
    }

    #[test]
    fn read_state_reaches_the_server_and_the_cache() {
        let link = mock_link();
        assert!(matches!(next_significant(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        while !matches!(next_significant(&link), Event::LoadFinished) {}

        let thrid = thread_of(&link, 2);
        link.commands
            .send(Command::SetSeen {
                thrids: vec![thrid],
                seen: true,
            })
            .unwrap();
        loop {
            match next_significant(&link) {
                Event::Labels { updates, command } => {
                    assert!(command.contains("\\Seen"), "{command}");
                    assert!(
                        updates.iter().all(|u| u.seen),
                        "every message in the thread is read"
                    );
                    break;
                }
                Event::Body { .. } => continue,
                _ => panic!("unexpected event"),
            }
        }
        // The cache agrees with the server: nothing in the thread is unread.
        let (list, _) = threads(&link);
        assert!(
            list.iter()
                .filter(|t| t.thrid == thrid)
                .all(|t| t.unread == 0)
        );
    }

    #[test]
    fn a_range_of_threads_is_read_in_list_order() {
        let link = mock_link();
        assert!(matches!(next_significant(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        while !matches!(next_significant(&link), Event::LoadFinished) {}

        let (list, _) = threads(&link);
        link.commands
            .send(Command::ThreadRange {
                filter: ThreadFilter::All,
                start: 0,
                count: 3,
                checked: true,
            })
            .unwrap();
        loop {
            if let Event::ThreadRange {
                thrids, checked, ..
            } = next(&link)
            {
                assert!(checked);
                let expected: Vec<u64> = list.iter().take(3).map(|t| t.thrid).collect();
                assert_eq!(thrids, expected);
                break;
            }
        }
    }

    #[test]
    fn check_with_nothing_new_reports_no_arrivals() {
        let link = mock_link();
        assert!(matches!(next_significant(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        while !matches!(next_significant(&link), Event::LoadFinished) {}

        link.commands.send(Command::CheckNew).unwrap();
        loop {
            match next_significant(&link) {
                Event::CheckFinished => break,
                Event::Body { .. } => continue,
                Event::NewArrivals(_) => panic!("nothing is new"),
                _ => panic!("unexpected event"),
            }
        }
    }

    #[test]
    fn a_later_session_reads_the_cache_before_any_load() {
        let dir = std::env::temp_dir().join(format!("gmail-worker-test-{}", std::process::id()));
        let path = dir.join("cache.sqlite");
        let _ = std::fs::remove_file(&path);

        // First session: sync everything into the cache.
        let first = spawn(
            egui::Context::default(),
            Box::new(|| Ok(Box::new(MockSource::demo()) as Box<dyn MailSource>)),
            KEEPALIVE,
            FAST,
            Some(Store::open(&path).unwrap()),
            Vec::new(),
        );
        assert!(matches!(next_significant(&first), Event::Connected));
        first.commands.send(Command::Load { limit: 500 }).unwrap();
        loop {
            if matches!(
                next(&first),
                Event::Progress {
                    cached: 6,
                    total: 6
                }
            ) {
                break;
            }
        }
        drop(first);

        // Second session: the list shows the cache at once, before any load.
        let second = spawn(
            egui::Context::default(),
            Box::new(|| Ok(Box::new(MockSource::demo()) as Box<dyn MailSource>)),
            KEEPALIVE,
            FAST,
            Some(Store::open(&path).unwrap()),
            Vec::new(),
        );
        assert!(matches!(next_significant(&second), Event::Connected));
        let (list, _) = threads(&second);
        assert_eq!(cached_messages(&list), 6, "all six come from the cache");
        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_list_reads_any_slice_of_the_cache() {
        let dir =
            std::env::temp_dir().join(format!("gmail-worker-page-test-{}", std::process::id()));
        let path = dir.join("cache.sqlite");
        let _ = std::fs::remove_file(&path);

        // First session: cache all six headers.
        let first = spawn(
            egui::Context::default(),
            Box::new(|| Ok(Box::new(MockSource::demo()) as Box<dyn MailSource>)),
            KEEPALIVE,
            FAST,
            Some(Store::open(&path).unwrap()),
            Vec::new(),
        );
        assert!(matches!(next_significant(&first), Event::Connected));
        first.commands.send(Command::Load { limit: 500 }).unwrap();
        loop {
            if matches!(
                next(&first),
                Event::Progress {
                    cached: 6,
                    total: 6
                }
            ) {
                break;
            }
        }
        drop(first);

        // Second session: a window of two, then the rest, together making the whole list.
        let second = spawn(
            egui::Context::default(),
            Box::new(|| Ok(Box::new(MockSource::demo()) as Box<dyn MailSource>)),
            KEEPALIVE,
            FAST,
            Some(Store::open(&path).unwrap()),
            Vec::new(),
        );
        assert!(matches!(next_significant(&second), Event::Connected));
        let (all, total) = threads(&second);
        assert_eq!(all.len(), total);
        let (first_two, _) = window(&second, ThreadFilter::All, 0, 2);
        let (rest, _) = window(&second, ThreadFilter::All, 2, 500);
        assert_eq!(first_two.len(), total.min(2));
        assert_eq!(rest.len(), total.saturating_sub(2));
        let joined: Vec<u64> = first_two
            .iter()
            .chain(rest.iter())
            .map(|t| t.thrid)
            .collect();
        let whole: Vec<u64> = all.iter().map(|t| t.thrid).collect();
        assert_eq!(joined, whole);
        assert_eq!(cached_messages(&all), 6);
        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconnects_and_retries_after_a_dropped_connection() {
        let mut connects = 0;
        let connect: Connector = Box::new(move || {
            connects += 1;
            Ok(Box::new(Flaky {
                inner: MockSource::demo(),
                fail: connects == 1,
            }) as Box<dyn MailSource>)
        });
        let link = spawn(
            egui::Context::default(),
            connect,
            KEEPALIVE,
            FAST,
            None,
            Vec::new(),
        );
        assert!(matches!(next_significant(&link), Event::Connected));

        link.commands.send(Command::Load { limit: 500 }).unwrap();
        assert!(matches!(next_significant(&link), Event::Reconnecting));
        assert!(matches!(next_significant(&link), Event::Reconnected));
        assert!(matches!(
            next_significant(&link),
            Event::LoadStarted { total: 6 }
        ));
    }

    #[test]
    fn keepalive_detects_a_dropped_connection_while_idle() {
        let mut connects = 0;
        let connect: Connector = Box::new(move || {
            connects += 1;
            Ok(Box::new(Flaky {
                inner: MockSource::demo(),
                fail: connects == 1,
            }) as Box<dyn MailSource>)
        });
        let link = spawn(
            egui::Context::default(),
            connect,
            Duration::from_millis(20),
            FAST,
            None,
            Vec::new(),
        );
        assert!(matches!(next_significant(&link), Event::Connected));
        assert!(matches!(next_significant(&link), Event::Reconnecting));
        assert!(matches!(next_significant(&link), Event::Reconnected));
    }

    #[test]
    fn gives_up_after_repeated_reconnect_failures() {
        let mut connects = 0;
        let connect: Connector = Box::new(move || {
            connects += 1;
            if connects == 1 {
                Ok(Box::new(Flaky {
                    inner: MockSource::demo(),
                    fail: true,
                }) as Box<dyn MailSource>)
            } else {
                Err("sign-in refused".to_string())
            }
        });
        let link = spawn(
            egui::Context::default(),
            connect,
            KEEPALIVE,
            FAST,
            None,
            Vec::new(),
        );
        assert!(matches!(next_significant(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        assert!(matches!(next_significant(&link), Event::Reconnecting));
        match next_significant(&link) {
            Event::Error(e) => assert!(e.contains("sign-in refused"), "{e}"),
            _ => panic!("expected a reconnect error"),
        }
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use crate::backend::MockSource;
    use gmail_imap_proto::store::DEFAULT_BODY_CAP;
    use std::collections::{BTreeSet, VecDeque};
    use std::sync::{Arc, Mutex};

    /// The demo mailbox, plus a scripted IDLE: each `idle_poll` returns the next
    /// scripted notices, or waits a moment and returns none.
    struct Pushing {
        inner: MockSource,
        script: Arc<Mutex<VecDeque<Vec<IdleNotice>>>>,
    }

    impl MailSource for Pushing {
        fn select(&mut self) -> Result<MailboxInfo, SourceError> {
            self.inner.select()
        }
        fn noop(&mut self) -> Result<(), SourceError> {
            self.inner.noop()
        }
        fn search_from(&mut self, start: u32) -> Result<Vec<u32>, SourceError> {
            self.inner.search_from(start)
        }
        fn fetch_headers(&mut self, uids: &[u32]) -> Result<Vec<Message>, SourceError> {
            self.inner.fetch_headers(uids)
        }
        fn fetch_body(&mut self, uid: u32) -> Result<Vec<u8>, SourceError> {
            self.inner.fetch_body(uid)
        }
        fn modify_labels(
            &mut self,
            uids: &[u32],
            action: LabelAction,
            labels: &[String],
        ) -> Result<(String, Vec<MessageUpdate>), SourceError> {
            self.inner.modify_labels(uids, action, labels)
        }
        fn supports_condstore(&self) -> bool {
            self.inner.supports_condstore()
        }
        fn changed_since(&mut self, modseq: u64) -> Result<Vec<MessageUpdate>, SourceError> {
            self.inner.changed_since(modseq)
        }
        fn list_labels(&mut self) -> Result<Vec<String>, SourceError> {
            self.inner.list_labels()
        }
        fn supports_idle(&self) -> bool {
            true
        }
        fn idle_poll(&mut self, wait: Duration) -> Result<Vec<IdleNotice>, SourceError> {
            let next = self.script.lock().unwrap().pop_front();
            match next {
                Some(notices) => Ok(notices),
                None => {
                    thread::sleep(wait.min(Duration::from_millis(20)));
                    Ok(Vec::new())
                }
            }
        }
    }

    /// The variant name of an event, for readable test failures.
    fn name(ev: &Event) -> &'static str {
        match ev {
            Event::Connected => "Connected",
            Event::ConnectFailed(_) => "ConnectFailed",
            Event::Reconnecting => "Reconnecting",
            Event::Reconnected => "Reconnected",
            Event::Reset => "Reset",
            Event::LoadStarted { .. } => "LoadStarted",
            Event::LoadFinished => "LoadFinished",
            Event::CheckFinished => "CheckFinished",
            Event::Window { .. } => "Window",
            Event::ThreadRange { .. } => "ThreadRange",
            Event::Thread { .. } => "Thread",
            Event::NewArrivals(_) => "NewArrivals",
            Event::Progress { .. } => "Progress",
            Event::Body { .. } => "Body",
            Event::Labels { .. } => "Labels",
            Event::LabelList(_) => "LabelList",
            Event::LabelCounts { .. } => "LabelCounts",
            Event::Removed(_) => "Removed",
            Event::IdleState(_) => "IdleState",
            Event::Error(_) => "Error",
        }
    }

    fn next(link: &Link) -> Event {
        link.events
            .recv_timeout(Duration::from_secs(5))
            .expect("worker went quiet")
    }

    /// Starts a worker on `source`, loads it, and waits until the load is done.
    fn loaded(source: &MockSource, script: Arc<Mutex<VecDeque<Vec<IdleNotice>>>>) -> Link {
        let pushing = Pushing {
            inner: source.clone(),
            script,
        };
        let mut slot = Some(pushing);
        let link = spawn(
            egui::Context::default(),
            Box::new(move || {
                let s = slot.take().ok_or("already connected once")?;
                Ok(Box::new(s) as Box<dyn MailSource>)
            }),
            KEEPALIVE,
            Duration::from_millis(1),
            Some(Store::open_in_memory(DEFAULT_BODY_CAP).unwrap()),
            Vec::new(),
        );
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        loop {
            if matches!(next(&link), Event::LoadFinished) {
                break;
            }
        }
        link
    }

    #[test]
    fn a_change_made_elsewhere_reaches_the_gui_on_check() {
        let source = MockSource::demo();
        let handle = source.handle();
        let link = loaded(&source, Arc::new(Mutex::new(VecDeque::new())));

        handle.set_labels(4, &["Archive"]);
        handle.set_seen(1, true);
        link.commands.send(Command::CheckNew).unwrap();

        let mut updates = Vec::new();
        loop {
            match next(&link) {
                Event::Labels { updates: u, .. } => updates.extend(u),
                Event::CheckFinished => break,
                Event::Body { .. }
                | Event::Progress { .. }
                | Event::LabelList(_)
                | Event::LabelCounts { .. }
                | Event::IdleState(_) => {}
                other => panic!("unexpected event: {}", name(&other)),
            }
        }
        let four = updates.iter().find(|u| u.uid == 4).expect("uid 4 updated");
        assert_eq!(four.labels, BTreeSet::from(["Archive".to_string()]));
        assert!(updates.iter().find(|u| u.uid == 1).unwrap().seen);
        assert!(updates.iter().all(|u| u.uid == 1 || u.uid == 4));
    }

    #[test]
    fn idle_notice_triggers_a_sync_and_idle_is_reported() {
        let source = MockSource::demo();
        let handle = source.handle();
        let script = Arc::new(Mutex::new(VecDeque::new()));
        let link = loaded(&source, script.clone());

        // Once the mailbox is loaded, the worker reports that it's idle.
        loop {
            if let Event::IdleState(on) = next(&link) {
                assert!(on);
                break;
            }
        }

        // The server pushes a change. The worker syncs without being asked.
        handle.set_labels(2, &["Done"]);
        script.lock().unwrap().push_back(vec![IdleNotice::Changed]);
        loop {
            match next(&link) {
                Event::Labels { updates, .. } => {
                    assert_eq!(updates[0].uid, 2);
                    assert!(updates[0].labels.contains("Done"));
                    break;
                }
                Event::Body { .. }
                | Event::Progress { .. }
                | Event::LabelList(_)
                | Event::LabelCounts { .. }
                | Event::CheckFinished => {}
                other => panic!("unexpected event: {}", name(&other)),
            }
        }
    }

    #[test]
    fn the_full_label_list_reaches_the_gui() {
        let source = MockSource::demo();
        let pushing = Pushing {
            inner: source.clone(),
            script: Arc::new(Mutex::new(VecDeque::new())),
        };
        let mut slot = Some(pushing);
        let link = spawn(
            egui::Context::default(),
            Box::new(move || Ok(Box::new(slot.take().unwrap()) as Box<dyn MailSource>)),
            KEEPALIVE,
            Duration::from_millis(1),
            None,
            Vec::new(),
        );
        assert!(matches!(next(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        let mut saw_list = false;
        loop {
            match next(&link) {
                Event::LabelList(labels) => {
                    assert!(labels.contains(&"Unused".to_string()), "{labels:?}");
                    assert!(labels.contains(&"Work/Projects".to_string()), "{labels:?}");
                    assert!(!labels.iter().any(|l| l.starts_with('\\')), "{labels:?}");
                    saw_list = true;
                }
                Event::LoadFinished => break,
                _ => {}
            }
        }
        assert!(saw_list, "the label list arrives before the load finishes");
    }

    #[test]
    fn delivered_mail_is_reported_as_an_arrival() {
        let source = MockSource::demo();
        let handle = source.handle();
        let link = loaded(&source, Arc::new(Mutex::new(VecDeque::new())));
        let uid = handle.deliver("Bob <bob@example.com>", "Lunch?", &["\\Inbox"]);
        link.commands.send(Command::CheckNew).unwrap();
        loop {
            match next(&link) {
                Event::NewArrivals(msgs) => {
                    assert_eq!(msgs.len(), 1);
                    assert_eq!(msgs[0].uid, uid);
                    assert_eq!(msgs[0].subject, "Lunch?");
                    break;
                }
                Event::CheckFinished => panic!("the arrival was not reported"),
                _ => {}
            }
        }
    }

    #[test]
    fn label_changes_to_old_mail_are_not_arrivals() {
        let source = MockSource::demo();
        let handle = source.handle();
        let link = loaded(&source, Arc::new(Mutex::new(VecDeque::new())));
        handle.set_labels(2, &["\\Inbox", "Changed"]);
        link.commands.send(Command::CheckNew).unwrap();
        loop {
            match next(&link) {
                Event::CheckFinished => break,
                Event::NewArrivals(_) => panic!("an old message was reported as new"),
                _ => {}
            }
        }
    }

    #[test]
    fn expunge_removes_the_message_from_the_view() {
        let source = MockSource::demo();
        let handle = source.handle();
        let script = Arc::new(Mutex::new(VecDeque::new()));
        let link = loaded(&source, script.clone());
        handle.expunge(3);
        script.lock().unwrap().push_back(vec![IdleNotice::Expunged]);
        loop {
            match next(&link) {
                Event::Removed(uids) => {
                    assert_eq!(uids, vec![3]);
                    break;
                }
                Event::Body { .. }
                | Event::Progress { .. }
                | Event::LabelList(_)
                | Event::LabelCounts { .. }
                | Event::CheckFinished
                | Event::IdleState(_)
                | Event::Labels { .. } => {}
                Event::Error(e) => panic!("error event: {e}"),
                other => panic!("unexpected event: {}", name(&other)),
            }
        }
    }
}
