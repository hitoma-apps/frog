//! Background worker. It owns the mail source, runs commands in order, and
//! prefetches message bodies whenever it has no command waiting.

use std::collections::{BTreeSet, VecDeque};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;

use eframe::egui;
use gmail_imap_proto::labels::LabelAction;

use crate::backend::{MailSource, Message, MockSource};
use crate::live;

/// Messages per header FETCH, so the list fills in steadily.
const BATCH: usize = 50;

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

/// Starts a worker that signs in to Gmail.
pub fn spawn_gmail(user: String, password: String, ctx: egui::Context) -> Link {
    spawn(ctx, move || live::connect_gmail(&user, &password))
}

/// Starts a worker on the in-memory demo mailbox.
pub fn spawn_mock(ctx: egui::Context) -> Link {
    spawn(ctx, || {
        Ok(Box::new(MockSource::demo()) as Box<dyn MailSource>)
    })
}

fn spawn<F>(ctx: egui::Context, connect: F) -> Link
where
    F: FnOnce() -> Result<Box<dyn MailSource>, String> + Send + 'static,
{
    let (cmd_tx, cmd_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    let emit = Emitter { tx: ev_tx, ctx };
    thread::spawn(move || match connect() {
        Ok(source) => {
            emit.send(Event::Connected);
            run(source, cmd_rx, emit);
        }
        Err(e) => emit.send(Event::ConnectFailed(e)),
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

fn run(mut source: Box<dyn MailSource>, commands: Receiver<Command>, emit: Emitter) {
    let mut state = State::default();
    loop {
        if state.prefetch.is_empty() {
            match commands.recv() {
                Ok(cmd) => handle(cmd, source.as_mut(), &mut state, &emit),
                Err(_) => return,
            }
            continue;
        }
        match commands.try_recv() {
            Ok(cmd) => handle(cmd, source.as_mut(), &mut state, &emit),
            Err(TryRecvError::Empty) => {
                let uid = state.prefetch.pop_front().expect("checked non-empty");
                match source.fetch_body(uid) {
                    Ok(raw) => emit.send(Event::Body { uid, raw }),
                    Err(e) => {
                        state.prefetch.clear();
                        emit.send(Event::Error(format!("Background download stopped: {e}")));
                    }
                }
            }
            Err(TryRecvError::Disconnected) => return,
        }
    }
}

fn handle(cmd: Command, source: &mut dyn MailSource, state: &mut State, emit: &Emitter) {
    let result = match cmd {
        Command::Load { limit } => load(source, state, emit, limit),
        Command::CheckNew => check(source, state, emit),
        Command::FetchBody { uid } => {
            state.prefetch.retain(|&u| u != uid);
            source
                .fetch_body(uid)
                .map(|raw| emit.send(Event::Body { uid, raw }))
        }
        Command::Modify {
            uids,
            action,
            labels,
        } => source
            .modify_labels(&uids, action, &labels)
            .map(|(command, updates)| emit.send(Event::Labels { updates, command })),
    };
    if let Err(e) = result {
        emit.send(Event::Error(e));
    }
}

fn load(
    source: &mut dyn MailSource,
    state: &mut State,
    emit: &Emitter,
    limit: usize,
) -> Result<(), String> {
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

fn check(source: &mut dyn MailSource, state: &mut State, emit: &Emitter) -> Result<(), String> {
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
    use std::time::Duration;

    fn next(link: &Link) -> Event {
        link.events
            .recv_timeout(Duration::from_secs(5))
            .expect("worker went quiet")
    }

    #[test]
    fn loads_in_batches_then_prefetches_every_body() {
        let link = spawn_mock(egui::Context::default());
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

        // With nothing else queued, the worker downloads every body in the background.
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
        let link = spawn_mock(egui::Context::default());
        assert!(matches!(next(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        loop {
            if matches!(next(&link), Event::LoadFinished) {
                break;
            }
        }
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
        let link = spawn_mock(egui::Context::default());
        assert!(matches!(next(&link), Event::Connected));
        link.commands.send(Command::Load { limit: 500 }).unwrap();
        loop {
            if matches!(next(&link), Event::LoadFinished) {
                break;
            }
        }
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
}
