//! Prototype Gmail label manager.
//!
//! With `GMAIL_USER` and `GMAIL_APP_PASSWORD` set, it signs in at startup.
//! Otherwise it shows a sign-in dialog.
//! Mail access runs on a worker thread (see `worker.rs`).

mod backend;
mod live;
mod worker;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::{Duration, Instant};

use eframe::egui;
use gmail_imap_proto::labels::{LabelAction, SystemLabel};
use gmail_imap_proto::links::gmail_thread_url;
use gmail_imap_proto::render_plaintext;

use backend::Message;
use worker::{Command, Event, Link};

/// Newest messages to list from All Mail on load.
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

    messages: Vec<Message>,
    /// `(loaded, total)` while the initial list is loading.
    loading: Option<(usize, usize)>,
    checking: bool,
    auto_check: bool,
    last_check: Instant,
    /// UIDs whose body has been asked for and not yet delivered.
    requested_bodies: HashSet<u32>,
    /// Cached plaintext of the open message: `(msgid, text)`.
    rendered: Option<(u64, String)>,

    /// `None` shows All Mail, `Some(label)` filters by one raw label.
    filter: Option<String>,
    /// Checked messages, by `X-GM-MSGID`.
    checked: BTreeSet<u64>,
    /// The open message, by `X-GM-MSGID`.
    open: Option<u64>,
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
            messages: Vec::new(),
            loading: None,
            checking: false,
            auto_check: false,
            last_check: Instant::now(),
            requested_bodies: HashSet::new(),
            rendered: None,
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
                if let Some(m) = self.messages.iter_mut().find(|m| m.uid == uid) {
                    m.body = Some(raw);
                }
            }
            Event::Labels { updates, command } => {
                for (uid, labels) in updates {
                    if let Some(m) = self.messages.iter_mut().find(|m| m.uid == uid) {
                        m.labels = labels;
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

    /// Adds messages, replacing any with the same UID, and keeps newest first.
    fn insert(&mut self, msgs: Vec<Message>) {
        for m in msgs {
            match self.messages.iter_mut().find(|x| x.uid == m.uid) {
                Some(existing) => {
                    let body = existing.body.take();
                    *existing = m;
                    existing.body = body;
                }
                None => self.messages.push(m),
            }
        }
        self.messages.sort_by_key(|m| std::cmp::Reverse(m.uid));
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
        let has_body = self
            .messages
            .iter()
            .any(|m| m.uid == uid && m.body.is_some());
        if has_body || self.requested_bodies.contains(&uid) {
            return;
        }
        self.requested_bodies.insert(uid);
        self.send(Command::FetchBody { uid });
    }

    /// Checked messages if any, otherwise the open message. Returns UIDs.
    fn target_uids(&self) -> Vec<u32> {
        let msgids: Vec<u64> = if !self.checked.is_empty() {
            self.checked.iter().copied().collect()
        } else {
            self.open.into_iter().collect()
        };
        self.messages
            .iter()
            .filter(|m| msgids.contains(&m.msgid))
            .map(|m| m.uid)
            .collect()
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

    /// Label name to count, including system labels, for the sidebar.
    fn label_counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for m in &self.messages {
            for l in &m.labels {
                *counts.entry(l.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    fn visible(&self) -> Vec<&Message> {
        self.messages
            .iter()
            .filter(|m| match &self.filter {
                None => true,
                Some(l) => m.labels.contains(l),
            })
            .collect()
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
        }
        let counts = self.label_counts();
        for (raw, count) in &counts {
            let text = format!("{} ({count})", display_label(raw));
            if ui
                .selectable_label(self.filter.as_deref() == Some(raw.as_str()), text)
                .clicked()
            {
                self.filter = Some(raw.clone());
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

    fn message_list(&mut self, ui: &mut egui::Ui) {
        let rows: Vec<(u64, u64, String, String, String, Vec<String>)> = self
            .visible()
            .into_iter()
            .map(|m| {
                (
                    m.msgid,
                    m.thrid,
                    m.from.clone(),
                    m.subject.clone(),
                    m.date.clone(),
                    m.labels.iter().map(|l| display_label(l)).collect(),
                )
            })
            .collect();

        egui::ScrollArea::vertical()
            .id_salt("message_list")
            .show(ui, |ui| {
                if rows.is_empty() {
                    ui.label(if self.loading.is_some() {
                        "Waiting for messages…"
                    } else {
                        "No messages with this label."
                    });
                }
                for (msgid, thrid, from, subject, date, labels) in rows {
                    ui.horizontal(|ui| {
                        let mut checked = self.checked.contains(&msgid);
                        if ui.checkbox(&mut checked, "").changed() {
                            if checked {
                                self.checked.insert(msgid);
                            } else {
                                self.checked.remove(&msgid);
                            }
                        }
                        let open = self.open == Some(msgid);
                        if ui
                            .selectable_label(open, format!("{from} — {subject}"))
                            .on_hover_text(format!("thread {thrid}"))
                            .clicked()
                        {
                            self.open = Some(msgid);
                            self.rendered = None;
                        }
                        ui.weak(date);
                        ui.weak(labels.join(", "));
                    });
                }
            });
    }

    fn reader(&mut self, ui: &mut egui::Ui) {
        let Some(msgid) = self.open else {
            ui.label("Open a message to read it.");
            return;
        };
        let Some((uid, thrid, has_body)) = self
            .messages
            .iter()
            .find(|m| m.msgid == msgid)
            .map(|m| (m.uid, m.thrid, m.body.is_some()))
        else {
            return;
        };

        if !has_body {
            self.request_body(uid);
        } else if self.rendered.as_ref().map(|(id, _)| *id) != Some(msgid) {
            let text = self
                .messages
                .iter()
                .find(|m| m.msgid == msgid)
                .and_then(|m| m.body.as_deref())
                .map(|b| render_plaintext(b).unwrap_or_else(|| "(could not parse message)".into()))
                .unwrap_or_default();
            self.rendered = Some((msgid, text));
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
        if let Some(m) = self.messages.iter().find(|m| m.msgid == msgid) {
            ui.monospace(format!(
                "msgid  {} (0x{:x})\nthrid  {} (0x{:x})",
                m.msgid, m.msgid, m.thrid, m.thrid
            ));
        }
        ui.separator();

        egui::ScrollArea::vertical()
            .id_salt("reader")
            .show(ui, |ui| match &self.rendered {
                Some((id, text)) if *id == msgid => {
                    ui.monospace(text);
                }
                _ => {
                    ui.spinner();
                    ui.label("Loading message…");
                    if let Some(m) = self.messages.iter().find(|m| m.msgid == msgid) {
                        ui.monospace(String::from_utf8_lossy(&m.headers));
                    }
                }
            });
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
    }
}
