//! Prototype Gmail label manager. Runs on the mock backend for now.

mod backend;

use std::collections::{BTreeMap, BTreeSet};

use eframe::egui;
use gmail_imap_proto::labels::{LabelAction, SystemLabel};
use gmail_imap_proto::render_plaintext;

use backend::{MailBackend, Message, MockBackend};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Gmail Labels Prototype",
        options,
        Box::new(|_cc| Ok(Box::new(App::new(Box::new(MockBackend::demo()))))),
    )
}

/// Human-readable name for a raw `X-GM-LABELS` entry.
fn display_label(raw: &str) -> String {
    match SystemLabel::from_wire(raw) {
        Some(_) => raw.trim_start_matches('\\').to_string(),
        None => raw.to_string(),
    }
}

struct App {
    backend: Box<dyn MailBackend>,
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
    fn new(backend: Box<dyn MailBackend>) -> Self {
        let messages = backend.messages();
        Self {
            backend,
            messages,
            filter: None,
            checked: BTreeSet::new(),
            open: None,
            label_input: String::new(),
            log: Vec::new(),
            status: None,
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
        match self.backend.modify_labels(&targets, action, &labels) {
            Ok(cmd) => {
                self.log.push(cmd);
                self.messages = self.backend.messages();
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
        let text = render_plaintext(&msg.raw).unwrap_or_else(|| "(could not parse message)".into());
        egui::ScrollArea::vertical()
            .id_salt("reader")
            .show(ui, |ui| {
                ui.monospace(text);
            });
    }

    fn command_log(&self, ui: &mut egui::Ui) {
        ui.label("IMAP commands sent (mock):");
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
