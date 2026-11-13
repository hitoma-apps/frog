//! Background worker. It owns the mail source, runs commands in order, and
//! prefetches message bodies whenever it has no command waiting.
//!
//! It also keeps the connection alive. It sends NOOP after a quiet period, and
//! reconnects when the connection drops, retrying the failed command once.

use std::collections::{BTreeSet, VecDeque};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use eframe::egui;
use gmail_imap_proto::labels::LabelAction;

use crate::backend::{MailSource, Message, MockSource, SourceError};
use crate::live;

/// Messages per header FETCH, so the list fills in steadily.
const BATCH: usize = 50;
/// Quiet time before a NOOP keeps the connection open.
pub const KEEPALIVE: Duration = Duration::from_secs(240);
/// Reconnect attempts before giving up.
const RECONNECT_ATTEMPTS: u32 = 5;
/// First wait between reconnect attempts. Each later wait doubles: 1s, 2s, 4s, 8s.
const RETRY_BASE: Duration = Duration::from_secs(1);

/// Work for the worker, sent by the GUI.
pub enum Command {
    /// Drops the cache and loads the newest `limit` messages.
    Load { limit: usize },
    /// Looks for messages newer than any already loaded.
    CheckNew,
    /// Fetches one body now, ahead of the prefetch queue.
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
    /// The mailbox changed under us. The GUI must drop its cache.
    Reset,
    LoadStarted {
        total: usize,
    },
    Batch(Vec<Message>),
    LoadFinished,
    CheckFinished(Vec<Message>),
    Body {
        uid: u32,
        raw: Vec<u8>,
    },
    Labels {
        updates: Vec<(u32, BTreeSet<String>)>,
        command: String,
    },
    Error(String),
}

/// The GUI's end of a worker.
pub struct Link {
    pub commands: Sender<Command>,
    pub events: Receiver<Event>,
}

/// Opens a new connection. Called once at start, and again on each reconnect.
type Connector = Box<dyn FnMut() -> Result<Box<dyn MailSource>, String> + Send>;

/// Starts a worker that signs in to Gmail.
pub fn spawn_gmail(user: String, password: String, ctx: egui::Context) -> Link {
    spawn(
        ctx,
        Box::new(move || live::connect_gmail(&user, &password)),
        KEEPALIVE,
        RETRY_BASE,
    )
}

/// Starts a worker on the in-memory demo mailbox.
pub fn spawn_mock(ctx: egui::Context) -> Link {
    spawn(
        ctx,
        Box::new(|| Ok(Box::new(MockSource::demo()) as Box<dyn MailSource>)),
        KEEPALIVE,
        RETRY_BASE,
    )
}

fn spawn(
    ctx: egui::Context,
    connect: Connector,
    keepalive: Duration,
    retry_base: Duration,
) -> Link {
    let (cmd_tx, cmd_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    let emit = Emitter { tx: ev_tx, ctx };
    thread::spawn(move || {
        let mut connect = connect;
        match connect() {
            Ok(source) => {
                emit.send(Event::Connected);
                Worker {
                    source: Some(source),
                    connect,
                    state: State::default(),
                    emit,
                    keepalive,
                    retry_base,
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
    limit: usize,
    uidvalidity: Option<u32>,
    /// Highest UID known to exist.
    highest: Option<u32>,
    /// Bodies to fetch when idle, next first.
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
}

impl Worker {
    fn run(mut self, commands: Receiver<Command>) {
        loop {
            if self.state.prefetch.is_empty() {
                match commands.recv_timeout(self.keepalive) {
                    Ok(cmd) => self.dispatch(cmd),
                    Err(RecvTimeoutError::Timeout) => self.keepalive_tick(),
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match commands.try_recv() {
                    Ok(cmd) => self.dispatch(cmd),
                    Err(TryRecvError::Empty) => self.prefetch_one(),
                    Err(TryRecvError::Disconnected) => return,
                }
            }
        }
    }

    /// Runs one command. A dropped connection gets one reconnect and one retry.
    fn dispatch(&mut self, cmd: Command) {
        let mut retried = false;
        loop {
            if self.source.is_none() && !self.reconnect() {
                return;
            }
            match self.run_command(&cmd) {
                Ok(()) => return,
                Err(SourceError::Disconnected(_)) if !retried => {
                    retried = true;
                    self.source = None;
                }
                Err(e) => {
                    self.emit.send(Event::Error(e.to_string()));
                    return;
                }
            }
        }
    }

    fn run_command(&mut self, cmd: &Command) -> Result<(), SourceError> {
        let Some(source) = self.source.as_deref_mut() else {
            return Err(SourceError::Disconnected("not connected".into()));
        };
        let state = &mut self.state;
        let emit = &self.emit;
        match cmd {
            Command::Load { limit } => load(source, state, emit, *limit),
            Command::CheckNew => check(source, state, emit),
            Command::FetchBody { uid } => {
                state.prefetch.retain(|&u| u != *uid);
                let raw = source.fetch_body(*uid)?;
                emit.send(Event::Body { uid: *uid, raw });
                Ok(())
            }
            Command::Modify {
                uids,
                action,
                labels,
            } => {
                let (command, updates) = source.modify_labels(uids, *action, labels)?;
                emit.send(Event::Labels { updates, command });
                Ok(())
            }
        }
    }

    /// Fetches the next queued body. A drop here reconnects and keeps the queue.
    fn prefetch_one(&mut self) {
        let Some(uid) = self.state.prefetch.pop_front() else {
            return;
        };
        if self.source.is_none() && !self.reconnect() {
            self.state.prefetch.clear();
            return;
        }
        let Some(source) = self.source.as_deref_mut() else {
            return;
        };
        match source.fetch_body(uid) {
            Ok(raw) => self.emit.send(Event::Body { uid, raw }),
            Err(SourceError::Disconnected(_)) => {
                self.state.prefetch.push_front(uid);
                self.source = None;
                if !self.reconnect() {
                    self.state.prefetch.clear();
                }
            }
            Err(e) => {
                self.state.prefetch.clear();
                self.emit
                    .send(Event::Error(format!("Background download stopped: {e}")));
            }
        }
    }

    /// Sends NOOP after a quiet period. A drop found here is reconnected now,
    /// so the next command doesn't pay for it.
    fn keepalive_tick(&mut self) {
        let Some(source) = self.source.as_deref_mut() else {
            return;
        };
        match source.noop() {
            Ok(()) => {}
            Err(SourceError::Disconnected(_)) => {
                self.source = None;
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
}

fn load(
    source: &mut dyn MailSource,
    state: &mut State,
    emit: &Emitter,
    limit: usize,
) -> Result<(), SourceError> {
    state.limit = limit;
    state.uidvalidity = Some(source.select()?);
    let all = source.search_from(1)?;
    state.highest = all.last().copied();

    let newest: Vec<u32> = all.iter().rev().take(limit).copied().collect();
    emit.send(Event::LoadStarted {
        total: newest.len(),
    });
    for chunk in newest.chunks(BATCH) {
        emit.send(Event::Batch(source.fetch_headers(chunk)?));
    }
    state.prefetch = newest.iter().copied().collect();
    emit.send(Event::LoadFinished);
    Ok(())
}

fn check(
    source: &mut dyn MailSource,
    state: &mut State,
    emit: &Emitter,
) -> Result<(), SourceError> {
    let Some(highest) = state.highest else {
        return load(source, state, emit, state.limit);
    };
    let uidvalidity = source.select()?;
    if state.uidvalidity != Some(uidvalidity) {
        emit.send(Event::Reset);
        return load(source, state, emit, state.limit);
    }

    let new: Vec<u32> = source
        .search_from(highest + 1)?
        .into_iter()
        .filter(|&u| u > highest)
        .collect();
    let messages = if new.is_empty() {
        Vec::new()
    } else {
        source.fetch_headers(&new)?
    };
    if let Some(&max) = new.last() {
        state.highest = Some(max);
    }
    // Newest first at the front of the queue.
    for &uid in &new {
        state.prefetch.push_front(uid);
    }
    emit.send(Event::CheckFinished(messages));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        fn select(&mut self) -> Result<u32, SourceError> {
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
        ) -> Result<(String, crate::backend::LabelUpdates), SourceError> {
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

    fn mock_link() -> Link {
        spawn_mock(egui::Context::default())
    }

    #[test]
    fn loads_in_batches_then_prefetches_every_body() {
        let link = mock_link();
        assert!(matches!(next(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();

        assert!(matches!(next(&link), Event::LoadStarted { total: 6 }));
        let mut listed = 0;
        loop {
            match next(&link) {
                Event::Batch(m) => listed += m.len(),
                Event::LoadFinished => break,
                _ => panic!("unexpected event while loading"),
            }
        }
        assert_eq!(listed, 6);

        let mut bodies = HashSet::new();
        while bodies.len() < 6 {
            match next(&link) {
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
    fn label_change_reports_new_labels() {
        let link = mock_link();
        assert!(matches!(next(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        while !matches!(next(&link), Event::LoadFinished) {}
        link.commands
            .send(Command::Modify {
                uids: vec![3],
                action: LabelAction::Remove,
                labels: vec!["\\Inbox".to_string()],
            })
            .unwrap();
        loop {
            match next(&link) {
                Event::Labels { updates, command } => {
                    assert_eq!(command, r#"UID STORE 3 -X-GM-LABELS ("\\Inbox")"#);
                    assert_eq!(updates.len(), 1);
                    assert!(!updates[0].1.contains("\\Inbox"));
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
        assert!(matches!(next(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        while !matches!(next(&link), Event::LoadFinished) {}
        link.commands.send(Command::CheckNew).unwrap();
        loop {
            match next(&link) {
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
    fn reconnects_and_retries_after_a_dropped_connection() {
        let mut connects = 0;
        let connect: Connector = Box::new(move || {
            connects += 1;
            Ok(Box::new(Flaky {
                inner: MockSource::demo(),
                fail: connects == 1,
            }) as Box<dyn MailSource>)
        });
        let link = spawn(egui::Context::default(), connect, KEEPALIVE, FAST);
        assert!(matches!(next(&link), Event::Connected));

        link.commands.send(Command::Load { limit: 500 }).unwrap();
        assert!(matches!(next(&link), Event::Reconnecting));
        assert!(matches!(next(&link), Event::Reconnected));
        assert!(matches!(next(&link), Event::LoadStarted { total: 6 }));
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
        );
        assert!(matches!(next(&link), Event::Connected));
        assert!(matches!(next(&link), Event::Reconnecting));
        assert!(matches!(next(&link), Event::Reconnected));
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
        let link = spawn(egui::Context::default(), connect, KEEPALIVE, FAST);
        assert!(matches!(next(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        assert!(matches!(next(&link), Event::Reconnecting));
        match next(&link) {
            Event::Error(e) => assert!(e.contains("sign-in refused"), "{e}"),
            _ => panic!("expected a reconnect error"),
        }
    }
}
