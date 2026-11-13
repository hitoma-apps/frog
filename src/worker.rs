//! Background worker. It owns the mail source and the local cache. It runs
//! commands in order, and uses idle time for background work:
//!
//! 1. fetching headers that aren't cached yet, newest first, until all are held
//! 2. prefetching message bodies
//!
//! It also keeps the connection alive. It sends NOOP after a quiet period, and
//! reconnects when the connection drops, retrying the failed command once.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use eframe::egui;
use gmail_imap_proto::labels::LabelAction;
use gmail_imap_proto::store::{MailboxState, Store, account_cache_path};

use crate::backend::{IdleNotice, MailSource, MailboxInfo, Message, MessageUpdate, SourceError};
use crate::live;

/// How long one IDLE wait lasts before checking for commands.
const IDLE_POLL: Duration = Duration::from_secs(1);
/// Messages per header FETCH, so the list fills in steadily.
const BATCH: usize = 200;
/// Quiet time before a NOOP keeps the connection open.
pub const KEEPALIVE: Duration = Duration::from_secs(240);
/// Reconnect attempts before giving up.
const RECONNECT_ATTEMPTS: u32 = 5;
/// First wait between reconnect attempts. Each later wait doubles: 1s, 2s, 4s, 8s.
const RETRY_BASE: Duration = Duration::from_secs(1);

/// Work for the worker, sent by the GUI.
pub enum Command {
    /// Drops the view and loads the newest `limit` messages, from the cache first.
    Load { limit: usize },
    /// Looks for messages newer than any already loaded.
    CheckNew,
    /// Fetches one body now, ahead of the background queues.
    FetchBody { uid: u32 },
    /// Applies a label change to the given UIDs.
    Modify {
        uids: Vec<u32>,
        action: LabelAction,
        labels: Vec<String>,
    },
}

/// What the worker reports back.
pub enum Event {
    Connected,
    ConnectFailed(String),
    /// The connection dropped, and the worker is trying to reconnect.
    Reconnecting,
    Reconnected,
    /// The mailbox changed under us. The GUI must drop its view.
    Reset,
    LoadStarted {
        total: usize,
    },
    Batch(Vec<Message>),
    LoadFinished,
    CheckFinished(Vec<Message>),
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
    /// Messages were removed on the server. Drop them from the view.
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
pub fn open_cache(email: &str) -> Result<(Store, PathBuf), String> {
    let path = account_cache_path(email)
        .ok_or_else(|| "No data directory found; running without a cache.".to_string())?;
    let store = Store::open(&path)
        .map_err(|e| format!("Could not open the cache; running without it. {e}"))?;
    Ok((store, path))
}

/// The account's cache, plus any startup errors to send once connected.
fn cache_for(email: &str) -> (Option<Store>, Vec<Event>) {
    match open_cache(email) {
        Ok((store, _path)) => (Some(store), Vec::new()),
        Err(e) => (None, vec![Event::Error(e)]),
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
    /// Where the background show of cached headers continues: the next page is
    /// those with UID below this. `None` when the GUI has them all.
    show_cursor: Option<u32>,
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
            || !self.state.prefetch.is_empty()
            || self.state.show_cursor.is_some()
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
                uids,
                action,
                labels,
            } => self.op_modify(src, uids, *action, labels),
        }
    }

    /// One unit of background work: a batch of headers if any are pending,
    /// otherwise one body.
    fn idle_step(&mut self) {
        if self.state.show_cursor.is_some() {
            self.show_next_page();
            return;
        }
        let result = self.with_source(|w, src| {
            if w.state.pending_headers.is_empty() {
                w.op_prefetch_one(src)
            } else {
                w.op_fill_headers(src)
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
            self.state.show_cursor = None;
            self.emit.send(Event::Reset);
        }

        if let Some(store) = self.store.as_mut() {
            let stored = store.mailbox_state();
            let same = matches!(&stored, Ok(Some(st)) if st.uidvalidity == info.uidvalidity);
            if same {
                // A restart with a valid cache: resume from the last synced modseq,
                // so changes made while the app was closed get picked up.
                if self.state.modseq.is_none() {
                    self.state.modseq = stored.ok().flatten().and_then(|st| st.highest_modseq);
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
    fn cached_uid_list(&mut self) -> Vec<u32> {
        let Some(store) = self.store.as_ref() else {
            return Vec::new();
        };
        match store.cached_uids() {
            Ok(uids) => uids,
            Err(e) => {
                self.emit
                    .send(Event::Error(format!("cache read failed: {e}")));
                Vec::new()
            }
        }
    }

    /// The newest cached messages, for showing before the network replies.
    fn cached_messages(&mut self, limit: usize) -> Vec<Message> {
        let Some(store) = self.store.as_ref() else {
            return Vec::new();
        };
        match store.list_messages(None, limit) {
            Ok(list) => list.into_iter().filter_map(Message::from_cached).collect(),
            Err(e) => {
                self.emit
                    .send(Event::Error(format!("cache read failed: {e}")));
                Vec::new()
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
    }

    /// Shows the cache, applies changes made since the last sync, then fetches
    /// whatever headers the cache lacks. The newest `limit` are fetched now. The
    /// rest are queued for the background.
    fn op_load(&mut self, src: &mut dyn MailSource, limit: usize) -> Result<(), SourceError> {
        self.state.limit = limit;
        let info = src.select()?;
        self.begin_mailbox(info);
        self.report_labels(src)?;

        let all = src.search_from(1)?;
        self.state.highest = all.last().copied();
        let server: HashSet<u32> = all.iter().copied().collect();

        // Messages deleted on the server drop out of the cache.
        let stale: Vec<u32> = self
            .cached_uid_list()
            .into_iter()
            .filter(|u| !server.contains(u))
            .collect();
        self.remove_cached(&stale);

        self.emit.send(Event::LoadStarted {
            total: limit.min(all.len()),
        });
        let shown = self.cached_messages(limit);
        // If the cache holds more, the rest are shown in the background.
        self.state.show_cursor = if shown.len() >= limit {
            shown.last().map(|m| m.uid)
        } else {
            None
        };
        if !shown.is_empty() {
            self.emit.send(Event::Batch(shown));
        }

        // Changes made since the last sync: labels and read state, from elsewhere.
        if let (Some(modseq), true) = (self.state.modseq, src.supports_condstore()) {
            let mut fetched = Vec::new();
            self.sync_changes(src, modseq, &mut fetched)?;
            if !fetched.is_empty() {
                self.emit.send(Event::Batch(fetched));
            }
        }

        let cached: HashSet<u32> = self
            .cached_uid_list()
            .into_iter()
            .filter(|u| server.contains(u))
            .collect();
        self.state.cached = cached.len();

        // Headers we don't have, newest first.
        let mut missing: VecDeque<u32> = all
            .iter()
            .rev()
            .filter(|u| !cached.contains(u))
            .copied()
            .collect();
        let urgent: Vec<u32> = (0..limit.min(missing.len()))
            .filter_map(|_| missing.pop_front())
            .collect();
        for chunk in urgent.chunks(BATCH) {
            let msgs = src.fetch_headers(chunk)?;
            self.cache_upsert(&msgs);
            self.state.cached += msgs.len();
            self.emit.send(Event::Batch(msgs));
        }
        self.state.pending_headers = missing;
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
        let mut msgs = if new.is_empty() {
            Vec::new()
        } else {
            src.fetch_headers(&new)?
        };
        self.cache_upsert(&msgs);
        self.state.cached += msgs.len();
        if let Some(&max) = new.last() {
            self.state.highest = Some(max);
        }
        // Newest first at the front of the body queue.
        for &uid in &new {
            self.state.prefetch.push_front(uid);
        }

        // Before changes to older messages are folded in, `msgs` holds only arrivals.
        let arrivals = msgs.clone();
        if let (Some(modseq), true) = (self.state.modseq, src.supports_condstore()) {
            self.sync_changes(src, modseq, &mut msgs)?;
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
        self.emit.send(Event::CheckFinished(msgs));
        self.emit_progress();
        self.save_sync();
        Ok(())
    }

    /// Applies changes made since `modseq`: labels and read state for messages
    /// we have, and full headers for messages we don't.
    fn sync_changes(
        &mut self,
        src: &mut dyn MailSource,
        modseq: u64,
        msgs: &mut Vec<Message>,
    ) -> Result<(), SourceError> {
        let changes = match src.changed_since(modseq) {
            Ok(changes) => changes,
            Err(e @ SourceError::Disconnected(_)) => return Err(e),
            Err(e) => {
                self.emit
                    .send(Event::Error(format!("Could not sync changes: {e}")));
                return Ok(());
            }
        };
        let cached: HashSet<u32> = self.cached_uid_list().into_iter().collect();
        let (known, unknown): (Vec<MessageUpdate>, Vec<MessageUpdate>) =
            changes.into_iter().partition(|c| cached.contains(&c.uid));

        if !unknown.is_empty() {
            let uids: Vec<u32> = unknown.iter().map(|c| c.uid).collect();
            let fetched = src.fetch_headers(&uids)?;
            self.cache_upsert(&fetched);
            self.state.cached += fetched.len();
            // Those messages are no longer waiting for a background header fetch.
            self.state.pending_headers.retain(|u| !uids.contains(u));
            msgs.extend(fetched);
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
        let server: HashSet<u32> = src.search_from(1)?.into_iter().collect();
        let stale: Vec<u32> = self
            .cached_uid_list()
            .into_iter()
            .filter(|u| !server.contains(u))
            .collect();
        self.remove_cached(&stale);
        self.state.cached = self.state.cached.saturating_sub(stale.len());
        self.state.pending_headers.retain(|u| server.contains(u));
        self.state.prefetch.retain(|u| server.contains(u));
        self.state.highest = server.iter().max().copied();
        if !stale.is_empty() {
            self.emit.send(Event::Removed(stale));
        }
        self.emit_progress();
        Ok(())
    }

    /// Sends the next page of cached headers to the GUI, continuing down the cache.
    fn show_next_page(&mut self) {
        let Some(cursor) = self.state.show_cursor.take() else {
            return;
        };
        let Some(store) = self.store.as_ref() else {
            return;
        };
        match store.list_page(Some(cursor), BATCH) {
            Ok(page) => {
                let full = page.len() == BATCH;
                let last = page.last().and_then(|c| c.uid);
                let msgs: Vec<Message> =
                    page.into_iter().filter_map(Message::from_cached).collect();
                if full {
                    self.state.show_cursor = last;
                }
                if !msgs.is_empty() {
                    self.emit.send(Event::Batch(msgs));
                }
            }
            Err(e) => self
                .emit
                .send(Event::Error(format!("cache read failed: {e}"))),
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
                self.emit.send(Event::Batch(msgs));
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
    fn op_prefetch_one(&mut self, src: &mut dyn MailSource) -> Result<(), SourceError> {
        let Some(uid) = self.state.prefetch.pop_front() else {
            return Ok(());
        };
        if let Some(raw) = self.cached_body(uid) {
            self.emit.send(Event::Body { uid, raw });
            return Ok(());
        }
        match src.fetch_body(uid) {
            Ok(raw) => {
                self.cache_body(uid, &raw);
                self.emit.send(Event::Body { uid, raw });
                Ok(())
            }
            // The connection dropped: try this one again after reconnecting.
            Err(e @ SourceError::Disconnected(_)) => {
                self.state.prefetch.push_front(uid);
                Err(e)
            }
            // The server couldn't give this body, typically because it was
            // expunged after it was listed. Skip it. Opening it reports the error.
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
        self.emit.send(Event::Labels { updates, command });
        Ok(())
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
                Event::Progress { .. } | Event::LabelList(_) => continue,
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

    #[test]
    fn loads_in_batches_then_prefetches_every_body() {
        let link = mock_link();
        assert!(matches!(next_significant(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();

        assert!(matches!(
            next_significant(&link),
            Event::LoadStarted { total: 6 }
        ));
        let mut listed = 0;
        loop {
            match next_significant(&link) {
                Event::Batch(m) => listed += m.len(),
                Event::LoadFinished => break,
                _ => panic!("unexpected event while loading"),
            }
        }
        assert_eq!(listed, 6);

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
    }

    #[test]
    fn background_fill_fetches_the_rest_of_the_headers() {
        let link = mock_link();
        assert!(matches!(next_significant(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 2 }).unwrap();

        assert!(matches!(
            next_significant(&link),
            Event::LoadStarted { total: 2 }
        ));
        let mut first_page = 0;
        loop {
            match next_significant(&link) {
                Event::Batch(m) => first_page += m.len(),
                Event::LoadFinished => break,
                _ => panic!("unexpected event while loading"),
            }
        }
        assert_eq!(first_page, 2);

        // The rest arrive in the background, and the count reaches the mailbox size.
        let mut background = 0;
        loop {
            match next(&link) {
                Event::Batch(m) => background += m.len(),
                Event::Progress {
                    cached: 6,
                    total: 6,
                } => break,
                Event::Body { .. } | Event::Progress { .. } | Event::LabelList(_) => {}
                _ => panic!("unexpected event during background fill"),
            }
        }
        assert_eq!(background, 4);
    }

    #[test]
    fn label_change_reports_new_labels() {
        let link = mock_link();
        assert!(matches!(next_significant(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        while !matches!(next_significant(&link), Event::LoadFinished) {}
        link.commands
            .send(Command::Modify {
                uids: vec![3],
                action: LabelAction::Remove,
                labels: vec!["\\Inbox".to_string()],
            })
            .unwrap();
        loop {
            match next_significant(&link) {
                Event::Labels { updates, command } => {
                    assert_eq!(command, r#"UID STORE 3 -X-GM-LABELS ("\\Inbox")"#);
                    assert_eq!(updates.len(), 1);
                    assert!(!updates[0].labels.contains("\\Inbox"));
                    break;
                }
                Event::Body { .. } => continue,
                _ => panic!("unexpected event"),
            }
        }
    }

    #[test]
    fn check_with_nothing_new_reports_empty() {
        let link = mock_link();
        assert!(matches!(next_significant(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        while !matches!(next_significant(&link), Event::LoadFinished) {}
        link.commands.send(Command::CheckNew).unwrap();
        loop {
            match next_significant(&link) {
                Event::CheckFinished(m) => {
                    assert!(m.is_empty());
                    break;
                }
                Event::Body { .. } => continue,
                _ => panic!("unexpected event"),
            }
        }
    }

    #[test]
    fn second_load_shows_cached_headers_before_fetching() {
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

        // Second session: the cached headers appear before any fetch.
        let second = spawn(
            egui::Context::default(),
            Box::new(|| Ok(Box::new(MockSource::demo()) as Box<dyn MailSource>)),
            KEEPALIVE,
            FAST,
            Some(Store::open(&path).unwrap()),
            Vec::new(),
        );
        assert!(matches!(next_significant(&second), Event::Connected));
        second.commands.send(Command::Load { limit: 500 }).unwrap();
        assert!(matches!(
            next_significant(&second),
            Event::LoadStarted { total: 6 }
        ));
        match next_significant(&second) {
            Event::Batch(m) => assert_eq!(m.len(), 6, "all six come from the cache"),
            _ => panic!("expected the cached batch first"),
        }
        assert!(matches!(next_significant(&second), Event::LoadFinished));
        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cached_headers_beyond_the_first_page_reach_the_gui() {
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

        // Second session: a page of two, then the rest from the cache in the background.
        let second = spawn(
            egui::Context::default(),
            Box::new(|| Ok(Box::new(MockSource::demo()) as Box<dyn MailSource>)),
            KEEPALIVE,
            FAST,
            Some(Store::open(&path).unwrap()),
            Vec::new(),
        );
        assert!(matches!(next_significant(&second), Event::Connected));
        second.commands.send(Command::Load { limit: 2 }).unwrap();
        let mut shown: Vec<u32> = Vec::new();
        while shown.len() < 6 {
            if let Event::Batch(m) = next_significant(&second) {
                shown.extend(m.iter().map(|x| x.uid));
            }
        }
        shown.sort_unstable();
        assert_eq!(shown, vec![1, 2, 3, 4, 5, 6]);
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
            Event::Batch(_) => "Batch",
            Event::LoadFinished => "LoadFinished",
            Event::CheckFinished(_) => "CheckFinished",
            Event::NewArrivals(_) => "NewArrivals",
            Event::Progress { .. } => "Progress",
            Event::Body { .. } => "Body",
            Event::Labels { .. } => "Labels",
            Event::LabelList(_) => "LabelList",
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
                Event::CheckFinished(_) => break,
                Event::Body { .. }
                | Event::Progress { .. }
                | Event::LabelList(_)
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
                | Event::CheckFinished(_) => {}
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
                Event::CheckFinished(_) => panic!("the arrival was not reported"),
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
                Event::CheckFinished(_) => break,
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
                | Event::CheckFinished(_)
                | Event::IdleState(_)
                | Event::Labels { .. } => {}
                Event::Error(e) => panic!("error event: {e}"),
                other => panic!("unexpected event: {}", name(&other)),
            }
        }
    }
}
