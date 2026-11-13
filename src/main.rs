//! Prototype Gmail label manager.
//!
//! With `GMAIL_USER` and `GMAIL_APP_PASSWORD` set, it signs in at startup.
//! Otherwise it shows a sign-in dialog.
//! Mail access runs on a worker thread (see `worker.rs`).
//!
//! The list is sized for large mailboxes. Messages live in a map keyed by UID,
//! label counts are kept up to date as messages change, the visible order is
//! rebuilt only when something changes, and only the rows on screen are drawn.

mod backend;
mod live;
mod reader;
mod worker;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::{Duration, Instant};

use eframe::egui;
use gmail_imap_proto::labels::{LabelAction, SystemLabel};
use gmail_imap_proto::linkcheck::{self, LinkInfo};
use gmail_imap_proto::links::gmail_thread_url;
use gmail_imap_proto::render_plaintext;

use backend::Message;
use reader::{Block, Segment};
use worker::{Command, Event, Link};

/// Newest messages to show on load, before the rest of the cache fills in.
const LIVE_LIMIT: usize = 500;
/// How often the auto-check toggle looks for new mail.
const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(60);

fn main() -> eframe::Result {
    let creds = match (
        std::env::var("GMAIL_USER"),
        std::env::var("GMAIL_APP_PASSWORD"),
    ) {
        (Ok(user), Ok(password)) => Some((user, password)),
        _ => None,
    };

    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Gmail Labels Prototype",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc.egui_ctx.clone(), creds)))),
    )
}

/// Human-readable name for a raw `X-GM-LABELS` entry.
fn display_label(raw: &str) -> String {
    match SystemLabel::from_wire(raw) {
        Some(_) => raw.trim_start_matches('\\').to_string(),
        None => raw.to_string(),
    }
}

/// Adds (`add = true`) or removes one count per label in `labels`.
fn adjust_counts(counts: &mut BTreeMap<String, usize>, labels: &BTreeSet<String>, add: bool) {
    for label in labels {
        if add {
            *counts.entry(label.clone()).or_insert(0) += 1;
        } else if let Some(n) = counts.get_mut(label) {
            *n -= 1;
            if *n == 0 {
                counts.remove(label);
            }
        }
    }
}

/// Draws one line of a message. Long lines wrap, and links are clickable.
/// A link that looks disguised isn't opened here. Its check result goes to
/// `pending` so the confirmation window can show it.
fn render_line(ui: &mut egui::Ui, line: &str, pending: &mut Option<LinkInfo>) {
    if line.is_empty() {
        ui.add_space(ui.text_style_height(&egui::TextStyle::Monospace) * 0.6);
        return;
    }
    let segments = reader::split_links(line);
    if segments.iter().all(|s| matches!(s, Segment::Text(_))) {
        ui.label(egui::RichText::new(line).monospace());
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for segment in segments {
            match segment {
                Segment::Text(text) => {
                    ui.label(egui::RichText::new(text).monospace());
                }
                Segment::Link(url) if linkcheck::is_suspicious(url) => {
                    let info = linkcheck::analyze(url);
                    let hover = format!(
                        "Goes to {}\n{}\nClick to check before opening.",
                        info.target,
                        info.warnings.join("\n")
                    );
                    let text = egui::RichText::new(format!("⚠ {url}"))
                        .monospace()
                        .color(egui::Color32::LIGHT_RED);
                    if ui.link(text).on_hover_text(hover).clicked() {
                        *pending = Some(info);
                    }
                }
                Segment::Link(url) => {
                    ui.hyperlink_to(egui::RichText::new(url).monospace(), url);
                }
            }
        }
    });
}

#[derive(Default)]
struct LoginForm {
    email: String,
    password: String,
    error: Option<String>,
}

struct App {
    ctx: egui::Context,
    /// The worker, once a connection attempt has started.
    link: Option<Link>,
    connecting: bool,
    connected: bool,
    /// The connection dropped and the worker is reconnecting.
    reconnecting: bool,
    /// Messages in the mailbox, from the last SELECT.
    mailbox_total: usize,
    /// Headers held locally, out of `mailbox_total`.
    cached_count: usize,
    /// A notice such as where the cache lives.
    info: Option<String>,
    login: LoginForm,

    /// Messages keyed by UID. Iterating in reverse gives newest first.
    messages: BTreeMap<u32, Message>,
    /// Label name to number of messages with it. Kept in step with `messages`.
    counts: BTreeMap<String, usize>,
    /// UIDs in display order, after the filter. Rebuilt when `view_dirty` is set.
    view: Vec<u32>,
    view_dirty: bool,

    /// `(loaded, total)` while the initial list is loading.
    loading: Option<(usize, usize)>,
    checking: bool,
    auto_check: bool,
    last_check: Instant,
    /// UIDs whose body has been asked for and not yet delivered.
    requested_bodies: HashSet<u32>,
    /// Cached blocks of the open message: `(uid, blocks)`.
    rendered: Option<(u32, Vec<Block>)>,
    /// A suspicious link waiting for the user to confirm it.
    pending_link: Option<LinkInfo>,

    /// `None` shows All Mail, `Some(label)` filters by one raw label.
    filter: Option<String>,
    /// Checked messages, by UID.
    checked: BTreeSet<u32>,
    /// The open message, by UID.
    open: Option<u32>,
    label_input: String,
    /// IMAP commands issued this session, newest last.
    log: Vec<String>,
    status: Option<String>,
}

impl App {
    fn new(ctx: egui::Context, creds: Option<(String, String)>) -> Self {
        let mut app = Self {
            ctx,
            link: None,
            connecting: false,
            connected: false,
            reconnecting: false,
            mailbox_total: 0,
            cached_count: 0,
            info: None,
            login: LoginForm::default(),
            messages: BTreeMap::new(),
            counts: BTreeMap::new(),
            view: Vec::new(),
            view_dirty: true,
            loading: None,
            checking: false,
            auto_check: false,
            last_check: Instant::now(),
            requested_bodies: HashSet::new(),
            rendered: None,
            pending_link: None,
            filter: None,
            checked: BTreeSet::new(),
            open: None,
            label_input: String::new(),
            log: Vec::new(),
            status: None,
        };
        if let Some((user, password)) = creds {
            app.start_gmail(user, password);
        }
        app
    }

    fn start_gmail(&mut self, user: String, password: String) {
        self.login.error = None;
        self.connecting = true;
        self.link = Some(worker::spawn_gmail(user, password, self.ctx.clone()));
    }

    fn send(&self, cmd: Command) {
        if let Some(link) = &self.link {
            let _ = link.commands.send(cmd);
        }
    }

    /// Applies everything the worker has reported since the last frame.
    fn drain_events(&mut self) {
        let events: Vec<Event> = match &self.link {
            Some(link) => link.events.try_iter().collect(),
            None => return,
        };
        for ev in events {
            self.handle_event(ev);
        }
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Connected => {
                self.connecting = false;
                self.connected = true;
                self.loading = Some((0, 0));
                self.send(Command::Load { limit: LIVE_LIMIT });
            }
            Event::ConnectFailed(e) => {
                self.connecting = false;
                self.link = None;
                self.login.error = Some(e);
            }
            Event::Reset => {
                self.messages.clear();
                self.counts.clear();
                self.view_dirty = true;
                self.cached_count = 0;
                self.requested_bodies.clear();
                self.rendered = None;
                self.open = None;
                self.checked.clear();
            }
            Event::Info(msg) => self.info = Some(msg),
            Event::Progress { cached, total } => {
                self.cached_count = cached;
                self.mailbox_total = total;
            }
            Event::Reconnecting => self.reconnecting = true,
            Event::Reconnected => {
                self.reconnecting = false;
                self.status = None;
            }
            Event::LoadStarted { total } => self.loading = Some((0, total)),
            Event::Batch(msgs) => {
                if let Some((loaded, _)) = &mut self.loading {
                    *loaded += msgs.len();
                }
                self.insert(msgs);
            }
            Event::LoadFinished => {
                self.loading = None;
                self.last_check = Instant::now();
            }
            Event::CheckFinished(msgs) => {
                self.checking = false;
                self.last_check = Instant::now();
                self.insert(msgs);
            }
            Event::Body { uid, raw } => {
                self.requested_bodies.remove(&uid);
                if let Some(m) = self.messages.get_mut(&uid) {
                    m.body = Some(raw);
                }
            }
            Event::Labels { updates, command } => {
                for (uid, labels) in updates {
                    if let Some(m) = self.messages.get_mut(&uid) {
                        let old = std::mem::replace(&mut m.labels, labels);
                        adjust_counts(&mut self.counts, &old, false);
                        adjust_counts(&mut self.counts, &m.labels, true);
                        self.view_dirty = true;
                    }
                }
                self.log.push(command);
            }
            Event::Error(e) => {
                self.checking = false;
                self.loading = None;
                self.status = Some(e);
            }
        }
    }

    /// Adds messages, replacing any with the same UID. Bodies already fetched are kept.
    fn insert(&mut self, msgs: Vec<Message>) {
        if msgs.is_empty() {
            return;
        }
        for mut m in msgs {
            match self.messages.get_mut(&m.uid) {
                Some(existing) => {
                    adjust_counts(&mut self.counts, &existing.labels, false);
                    m.body = existing.body.take();
                    *existing = m;
                    adjust_counts(&mut self.counts, &existing.labels, true);
                }
                None => {
                    adjust_counts(&mut self.counts, &m.labels, true);
                    self.messages.insert(m.uid, m);
                }
            }
        }
        self.view_dirty = true;
    }

    /// Rebuilds the visible order if anything changed since the last frame.
    fn refresh_view(&mut self) {
        if !self.view_dirty {
            return;
        }
        self.view_dirty = false;
        let filter = self.filter.as_deref();
        self.view = self
            .messages
            .values()
            .rev()
            .filter(|m| filter.is_none_or(|l| m.labels.contains(l)))
            .map(|m| m.uid)
            .collect();
    }

    fn check_now(&mut self) {
        if !self.connected || self.checking || self.loading.is_some() {
            return;
        }
        self.checking = true;
        self.last_check = Instant::now();
        self.send(Command::CheckNew);
    }

    /// Asks the worker for a message's body unless it is already here or on its way.
    fn request_body(&mut self, uid: u32) {
        let has_body = self.messages.get(&uid).is_some_and(|m| m.body.is_some());
        if has_body || self.requested_bodies.contains(&uid) {
            return;
        }
        self.requested_bodies.insert(uid);
        self.send(Command::FetchBody { uid });
    }

    /// Checked messages if any, otherwise the open message. Returns UIDs.
    fn target_uids(&self) -> Vec<u32> {
        if !self.checked.is_empty() {
            self.checked.iter().copied().collect()
        } else {
            self.open.into_iter().collect()
        }
    }

    fn apply(&mut self, action: LabelAction, labels: Vec<String>) {
        let uids = self.target_uids();
        if uids.is_empty() {
            self.status = Some("Select a message or check some boxes first.".into());
            return;
        }
        self.status = None;
        self.send(Command::Modify {
            uids,
            action,
            labels,
        });
    }

    fn login_dialog(&mut self, ui: &mut egui::Ui) {
        let busy = self.connecting;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.2);
            ui.heading("Sign in to Gmail");
            ui.add_space(12.0);
            ui.set_max_width(360.0);

            ui.label("Email");
            let email = ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut self.login.email)
                    .hint_text("you@gmail.com")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(6.0);

            ui.label("App password");
            let password = ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut self.login.password)
                    .password(true)
                    .hint_text("16-character app password")
                    .desired_width(f32::INFINITY),
            );
            ui.weak("Needs 2-Step Verification. Create one under Google Account > Security > App passwords.");
            ui.add_space(10.0);

            let can_submit = !busy
                && !self.login.email.trim().is_empty()
                && !self.login.password.is_empty();
            let submit = ui.add_enabled(can_submit, egui::Button::new("Sign in"));
            let enter = (email.lost_focus() || password.lost_focus())
                && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (submit.clicked() || enter) && can_submit {
                let user = self.login.email.trim().to_string();
                let pass = self.login.password.clone();
                self.start_gmail(user, pass);
            }

            if busy {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Connecting…");
                });
            }
            if let Some(err) = &self.login.error {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.reconnecting {
                ui.spinner();
                ui.colored_label(egui::Color32::YELLOW, "Connection lost. Reconnecting…");
            }
            if let Some((loaded, total)) = self.loading {
                ui.spinner();
                if total == 0 {
                    ui.label("Loading messages…");
                } else {
                    ui.label(format!("Loading messages… {loaded} of {total}"));
                }
            } else {
                let busy = self.checking;
                let check = ui.add_enabled(!busy, egui::Button::new("Check for new"));
                if check.clicked() {
                    self.check_now();
                }
                if self.checking {
                    ui.spinner();
                    ui.label("Checking…");
                }
                ui.checkbox(&mut self.auto_check, "Check every 60s");
            }
            if let Some(info) = &self.info {
                ui.weak(info);
            }
            ui.weak(format!(
                "{} in mailbox · {} cached",
                self.mailbox_total, self.cached_count
            ));
            if self.cached_count < self.mailbox_total {
                ui.spinner();
                ui.label("Caching headers…");
            }
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.heading("Labels");
        ui.separator();
        if ui
            .selectable_label(self.filter.is_none(), "All Mail")
            .clicked()
        {
            self.filter = None;
            self.view_dirty = true;
        }
        for (raw, count) in &self.counts {
            let text = format!("{} ({count})", display_label(raw));
            if ui
                .selectable_label(self.filter.as_deref() == Some(raw.as_str()), text)
                .clicked()
            {
                self.filter = Some(raw.clone());
                self.view_dirty = true;
            }
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("Archive")
                .on_hover_text("Remove \\Inbox")
                .clicked()
            {
                self.apply(
                    LabelAction::Remove,
                    vec![SystemLabel::Inbox.as_wire().into()],
                );
            }
            if ui.button("Trash").on_hover_text("Add \\Trash").clicked() {
                self.apply(LabelAction::Add, vec![SystemLabel::Trash.as_wire().into()]);
            }
            if ui.button("Star").on_hover_text("Add \\Starred").clicked() {
                self.apply(
                    LabelAction::Add,
                    vec![SystemLabel::Starred.as_wire().into()],
                );
            }
            ui.separator();
            ui.add(
                egui::TextEdit::singleline(&mut self.label_input)
                    .hint_text("label name")
                    .desired_width(160.0),
            );
            let name = self.label_input.trim().to_string();
            let has_name = !name.is_empty();
            if ui
                .add_enabled(has_name, egui::Button::new("Add label"))
                .clicked()
            {
                self.apply(LabelAction::Add, vec![name.clone()]);
            }
            if ui
                .add_enabled(has_name, egui::Button::new("Remove label"))
                .clicked()
            {
                self.apply(LabelAction::Remove, vec![name]);
            }
        });
        if let Some(status) = &self.status {
            ui.colored_label(egui::Color32::LIGHT_RED, status);
        }
    }

    /// The message list. Only the rows on screen are drawn.
    fn message_list(&mut self, ui: &mut egui::Ui) {
        if self.view.is_empty() {
            ui.label(if self.loading.is_some() {
                "Waiting for messages…"
            } else {
                "No messages with this label."
            });
            return;
        }
        let row_height = ui.text_style_height(&egui::TextStyle::Body);
        // Take the order out of `self` so rows can borrow the rest of it.
        let view = std::mem::take(&mut self.view);
        egui::ScrollArea::vertical()
            .id_salt("message_list")
            .show_rows(ui, row_height, view.len(), |ui, range| {
                for &uid in &view[range] {
                    let Some(m) = self.messages.get(&uid) else {
                        continue;
                    };
                    ui.horizontal(|ui| {
                        let mut checked = self.checked.contains(&uid);
                        if ui.checkbox(&mut checked, "").changed() {
                            if checked {
                                self.checked.insert(uid);
                            } else {
                                self.checked.remove(&uid);
                            }
                        }
                        let open = self.open == Some(uid);
                        if ui
                            .selectable_label(open, format!("{} — {}", m.from, m.subject))
                            .on_hover_text(format!("thread {}", m.thrid))
                            .clicked()
                        {
                            self.open = Some(uid);
                            self.rendered = None;
                        }
                        ui.weak(&m.date);
                        let labels: Vec<String> =
                            m.labels.iter().map(|l| display_label(l)).collect();
                        ui.weak(labels.join(", "));
                    });
                }
            });
        self.view = view;
    }

    fn reader(&mut self, ui: &mut egui::Ui) {
        let Some(uid) = self.open else {
            ui.label("Open a message to read it.");
            return;
        };
        let Some((msgid, thrid, has_body)) = self
            .messages
            .get(&uid)
            .map(|m| (m.msgid, m.thrid, m.body.is_some()))
        else {
            return;
        };

        if !has_body {
            self.request_body(uid);
        } else if self.rendered.as_ref().map(|(id, _)| *id) != Some(uid) {
            let blocks = self
                .messages
                .get(&uid)
                .and_then(|m| m.body.as_deref())
                .map(|b| {
                    let text =
                        render_plaintext(b).unwrap_or_else(|| "(could not parse message)".into());
                    reader::blocks(&text)
                })
                .unwrap_or_default();
            self.rendered = Some((uid, blocks));
        }

        let open_url = (thrid != 0).then(|| gmail_thread_url(thrid));
        ui.horizontal(|ui| {
            let button = ui.add_enabled(open_url.is_some(), egui::Button::new("Open in Gmail"));
            if button
                .on_disabled_hover_text("This message has no thread ID.")
                .clicked()
                && let Some(url) = open_url
            {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        });
        ui.monospace(format!(
            "msgid  {msgid} (0x{msgid:x})\nthrid  {thrid} (0x{thrid:x})"
        ));
        ui.separator();

        let mut pending: Option<LinkInfo> = None;
        egui::ScrollArea::vertical()
            .id_salt("reader")
            .show(ui, |ui| match &self.rendered {
                Some((id, blocks)) if *id == uid => {
                    for (i, block) in blocks.iter().enumerate() {
                        match block {
                            Block::Line(line) => render_line(ui, line, &mut pending),
                            Block::Quote(lines) => {
                                ui.push_id(i, |ui| {
                                    egui::CollapsingHeader::new(format!(
                                        "▸ Quoted text ({} lines)",
                                        lines.len()
                                    ))
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for line in lines {
                                            render_line(ui, line, &mut pending);
                                        }
                                    });
                                });
                            }
                        }
                    }
                }
                _ => {
                    ui.spinner();
                    ui.label("Loading message…");
                    if let Some(m) = self.messages.get(&uid) {
                        ui.monospace(String::from_utf8_lossy(&m.headers));
                    }
                }
            });
        if pending.is_some() {
            self.pending_link = pending;
        }
    }

    /// The confirmation shown before opening a link that looks disguised.
    fn link_confirmation(&mut self, ctx: &egui::Context) {
        let Some(info) = self.pending_link.clone() else {
            return;
        };
        let mut open = true;
        let mut decision = None;
        egui::Window::new("Check this link")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("This link may be disguised. Check the real address before opening it.");
                ui.add_space(6.0);
                ui.monospace(format!("Link text:  {}", info.url));
                ui.monospace(format!("Goes to:    {}", info.target));
                ui.add_space(6.0);
                for w in &info.warnings {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("• {w}"));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Open the real address").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(true) => {
                ctx.open_url(egui::OpenUrl::new_tab(info.target.clone()));
                self.pending_link = None;
            }
            Some(false) => self.pending_link = None,
            None if !open => self.pending_link = None,
            None => {}
        }
    }

    fn command_log(&self, ui: &mut egui::Ui) {
        ui.label("IMAP commands sent:");
        egui::ScrollArea::vertical()
            .id_salt("log")
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.log {
                    ui.monospace(line);
                }
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();

        if !self.connected {
            egui::CentralPanel::default().show(ui, |ui| self.login_dialog(ui));
            return;
        }
        self.refresh_view();

        // Auto-check: a timer that only runs while the toggle is on and nothing else is busy.
        if self.auto_check {
            if !self.checking
                && self.loading.is_none()
                && self.last_check.elapsed() >= AUTO_CHECK_INTERVAL
            {
                self.check_now();
            }
            let remaining = AUTO_CHECK_INTERVAL.saturating_sub(self.last_check.elapsed());
            ui.ctx()
                .request_repaint_after(remaining.max(Duration::from_secs(1)));
        }

        egui::Panel::top("status").show(ui, |ui| self.status_bar(ui));

        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(200.0)
            .show(ui, |ui| self.sidebar(ui));

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));

        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(120.0)
            .show(ui, |ui| self.command_log(ui));

        egui::CentralPanel::default().show(ui, |ui| {
            egui::Panel::top("list")
                .resizable(true)
                .default_size(260.0)
                .show(ui, |ui| self.message_list(ui));
            egui::CentralPanel::default().show(ui, |ui| self.reader(ui));
        });
        let ctx = ui.ctx().clone();
        self.link_confirmation(&ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_counts_track_adds_and_removes() {
        let mut counts = BTreeMap::new();
        let a: BTreeSet<String> = ["Work".to_string(), "\\Inbox".to_string()].into();
        let b: BTreeSet<String> = ["Work".to_string()].into();
        adjust_counts(&mut counts, &a, true);
        adjust_counts(&mut counts, &b, true);
        assert_eq!(counts.get("Work"), Some(&2));
        assert_eq!(counts.get("\\Inbox"), Some(&1));

        adjust_counts(&mut counts, &a, false);
        assert_eq!(counts.get("Work"), Some(&1));
        assert_eq!(counts.get("\\Inbox"), None, "zero counts are dropped");
    }
}
