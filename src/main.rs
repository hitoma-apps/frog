//! Prototype Gmail label manager.
//!
//! With `GMAIL_USER` and `GMAIL_APP_PASSWORD` set, it signs in at startup.
//! Otherwise it shows a sign-in dialog, which also offers demo data.

mod backend;
mod live;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui;
use gmail_imap_proto::labels::{LabelAction, SystemLabel};
use gmail_imap_proto::links::gmail_thread_url;
use gmail_imap_proto::render_plaintext;

use backend::{MailBackend, Message, MockBackend};

type ConnectResult = Result<Box<dyn MailBackend + Send>, String>;

fn main() -> eframe::Result {
    let (initial, initial_error) = match (
        std::env::var("GMAIL_USER"),
        std::env::var("GMAIL_APP_PASSWORD"),
    ) {
        (Ok(user), Ok(password)) => match live::connect_gmail(&user, &password) {
            Ok(b) => (Some(b as Box<dyn MailBackend>), None),
            Err(e) => (None, Some(e)),
        },
        _ => (None, None),
    };

    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Gmail Labels Prototype",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(initial, initial_error)))),
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
    /// `None` until the user signs in or picks demo data.
    backend: Option<Box<dyn MailBackend>>,
    login: LoginForm,
    /// A sign-in running on a background thread.
    pending: Option<Receiver<ConnectResult>>,
    messages: Vec<Message>,
    /// `None` shows All Mail, `Some(label)` filters by one raw label.
    filter: Option<String>,
    checked: BTreeSet<u64>,
    open: Option<u64>,
    label_input: String,
    /// IMAP commands issued this session, newest last.
    log: Vec<String>,
    status: Option<String>,
}

impl App {
    fn new(backend: Option<Box<dyn MailBackend>>, login_error: Option<String>) -> Self {
        let messages = backend.as_ref().map(|b| b.messages()).unwrap_or_default();
        Self {
            backend,
            login: LoginForm {
                error: login_error,
                ..Default::default()
            },
            pending: None,
            messages,
            filter: None,
            checked: BTreeSet::new(),
            open: None,
            label_input: String::new(),
            log: Vec::new(),
            status: None,
        }
    }

    fn set_backend(&mut self, backend: Box<dyn MailBackend>) {
        self.messages = backend.messages();
        self.backend = Some(backend);
        self.login.password.clear();
        self.login.error = None;
    }

    /// Starts a sign-in on a background thread so the window stays responsive.
    fn start_sign_in(&mut self, ctx: &egui::Context) {
        let email = self.login.email.trim().to_string();
        let password = self.login.password.clone();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(live::connect_gmail(&email, &password));
            ctx.request_repaint();
        });
        self.login.error = None;
        self.pending = Some(rx);
    }

    /// Picks up a finished sign-in, if there is one.
    fn poll_sign_in(&mut self) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(Ok(backend)) => {
                self.pending = None;
                self.set_backend(backend);
            }
            Ok(Err(e)) => {
                self.pending = None;
                self.login.error = Some(e);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                self.login.error = Some("Sign-in stopped unexpectedly.".into());
            }
        }
    }

    /// Checked messages if any, otherwise the open message.
    fn targets(&self) -> Vec<u64> {
        if !self.checked.is_empty() {
            self.checked.iter().copied().collect()
        } else {
            self.open.into_iter().collect()
        }
    }

    fn apply(&mut self, action: LabelAction, labels: Vec<String>) {
        let targets = self.targets();
        if targets.is_empty() {
            self.status = Some("Select a message or check some boxes first.".into());
            return;
        }
        let Some(backend) = self.backend.as_mut() else {
            return;
        };
        match backend.modify_labels(&targets, action, &labels) {
            Ok(cmd) => {
                self.log.push(cmd);
                self.messages = backend.messages();
                self.status = None;
            }
            Err(e) => self.status = Some(e),
        }
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
        let busy = self.pending.is_some();
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

            let submit = ui.add_enabled(
                !busy && !self.login.email.trim().is_empty() && !self.login.password.is_empty(),
                egui::Button::new("Sign in"),
            );
            let enter = (email.lost_focus() || password.lost_focus())
                && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (submit.clicked() || enter) && !busy && !self.login.password.is_empty() {
                self.start_sign_in(ui.ctx());
            }

            if ui.add_enabled(!busy, egui::Button::new("Use demo data")).clicked() {
                self.set_backend(Box::new(MockBackend::demo()));
            }

            if busy {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Connecting to imap.gmail.com…");
                });
            }
            if let Some(err) = &self.login.error {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::LIGHT_RED, err);
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
        let rows: Vec<(u64, u64, String, String, Vec<String>)> = self
            .visible()
            .into_iter()
            .map(|m| {
                (
                    m.msgid,
                    m.thrid,
                    m.from.clone(),
                    m.subject.clone(),
                    m.labels.iter().map(|l| display_label(l)).collect(),
                )
            })
            .collect();

        egui::ScrollArea::vertical()
            .id_salt("message_list")
            .show(ui, |ui| {
                if rows.is_empty() {
                    ui.label("No messages with this label.");
                }
                for (msgid, thrid, from, subject, labels) in rows {
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
                        }
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
        let Some(msg) = self.messages.iter().find(|m| m.msgid == msgid) else {
            return;
        };
        let open_url = (msg.thrid != 0).then(|| gmail_thread_url(msg.thrid));
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
            "msgid  {} (0x{:x})\nthrid  {} (0x{:x})",
            msg.msgid, msg.msgid, msg.thrid, msg.thrid
        ));
        ui.separator();
        let text = render_plaintext(&msg.raw).unwrap_or_else(|| "(could not parse message)".into());
        egui::ScrollArea::vertical()
            .id_salt("reader")
            .show(ui, |ui| {
                ui.monospace(text);
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
        self.poll_sign_in();

        if self.backend.is_none() {
            egui::CentralPanel::default().show(ui, |ui| self.login_dialog(ui));
            return;
        }

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
