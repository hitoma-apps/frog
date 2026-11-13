//! Prototype Gmail label manager.
//!
//! With `GMAIL_USER` and `GMAIL_APP_PASSWORD` set, it signs in at startup.
//! Otherwise it shows a sign-in dialog.
//! Mail access runs on a worker thread (see `worker.rs`).
//!
//! The thread list is read from the cache a window at a time, so only the rows
//! near the screen are held in memory. Labels and search are answered by the
//! cache's indexes, and only the open conversation's messages are kept whole.
//! Only the rows on screen are drawn.

mod backend;
mod credentials;
mod live;
mod notify;
mod reader;
mod worker;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use gmail_imap_proto::labels::{LabelAction, SystemLabel};
use gmail_imap_proto::linkcheck::{self, LinkInfo};
use gmail_imap_proto::links::{gmail_original_url, gmail_thread_url};
use gmail_imap_proto::render_plaintext;
use gmail_imap_proto::settings::{self, CHECK_INTERVAL_RANGE, PAGE_SIZE_RANGE, Settings};

use backend::Message;
use gmail_imap_proto::store::ThreadFilter;
use reader::{Block, Segment};
use worker::{Command, Event, Link, ThreadEntry};

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

/// Mailboxes always shown in the sidebar, even when the cache has none of their
/// messages. `None` is All Mail, which is every message.
const MAILBOXES: [(&str, Option<&str>); 8] = [
    ("All Mail", None),
    ("Inbox", Some("\\Inbox")),
    ("Starred", Some("\\Starred")),
    ("Important", Some("\\Important")),
    ("Sent", Some("\\Sent")),
    ("Drafts", Some("\\Draft")),
    ("Spam", Some("\\Spam")),
    ("Trash", Some("\\Trash")),
];

/// Most label chips shown on a row. Any more become a "+N" chip.
const MAX_CHIPS: usize = 3;
/// Widest the text of a label chip may be. Longer names are cut off, with the full name on hover.
const CHIP_TEXT_MAX: f32 = 80.0;
/// Width of the date column, on the right of every row.
const DATE_WIDTH: f32 = 210.0;

/// The chips for a message row, in display order. Leaves out the label the view is
/// filtered by. In a mailbox view (All Mail or a system folder) it also leaves out
/// Inbox. System labels come first.
fn row_chips(m: &Message, filter: Option<&str>) -> Vec<String> {
    let mailbox_view = filter.is_none_or(|f| f.starts_with('\\'));
    let inbox = SystemLabel::Inbox.as_wire();
    let mut labels: Vec<&String> = m
        .labels
        .iter()
        .filter(|l| Some(l.as_str()) != filter)
        .filter(|l| !(mailbox_view && l.as_str() == inbox))
        .collect();
    labels.sort_by_key(|l| (!l.starts_with('\\'), l.to_lowercase()));
    labels.into_iter().cloned().collect()
}

/// Thread rows read per request. Rows near the screen are read in chunks this size.
const WINDOW_CHUNK: usize = 200;
/// Rows read on each side of the visible rows, so a little scrolling needs no wait.
const WINDOW_PAD: usize = 100;

/// Gap between the items of a row.
const ROW_GAP: f32 = 4.0;
/// Width of the checkbox column, on the far left of every row.
const CHECK_WIDTH: f32 = 24.0;

/// The width a chip needs: its text, cut to `CHIP_TEXT_MAX`, plus padding.
fn chip_width(ui: &egui::Ui, text: &str, font: &egui::FontId) -> f32 {
    let w = ui.ctx().fonts_mut(|f| {
        f.layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::WHITE)
            .size()
            .x
    });
    w.min(CHIP_TEXT_MAX) + 12.0
}

/// `text`, cut short with an ellipsis so it fits in `max_w`.
fn fit_text(ui: &egui::Ui, text: &str, font: &egui::FontId, max_w: f32) -> String {
    let width = |s: &str| {
        ui.ctx().fonts_mut(|f| {
            f.layout_no_wrap(s.to_owned(), font.clone(), egui::Color32::WHITE)
                .size()
                .x
        })
    };
    if width(text) <= max_w {
        return text.to_owned();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while chars.pop().is_some() {
        let candidate: String = chars.iter().collect::<String>() + "…";
        if width(&candidate) <= max_w {
            return candidate;
        }
    }
    "…".to_owned()
}

/// Paints one label chip into `rect`.
fn paint_chip(ui: &mut egui::Ui, rect: egui::Rect, text: &str, font: &egui::FontId) {
    let fill = ui.visuals().widgets.inactive.weak_bg_fill;
    let color = ui.visuals().text_color();
    ui.painter()
        .rect_filled(rect.shrink2(egui::vec2(0.0, 2.0)), 8.0, fill);
    let shown = fit_text(ui, text, font, rect.width() - 12.0);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        shown,
        font.clone(),
        color,
    );
}

/// The title of a conversation row: the newest message's sender and subject, with
/// the message count when there's more than one. Unread conversations are emphasized.
fn row_title(m: &Message, count: usize, unread: bool) -> egui::RichText {
    let text = if count > 1 {
        format!("({count}) {} — {}", m.from, m.subject)
    } else {
        format!("{} — {}", m.from, m.subject)
    };
    let title = egui::RichText::new(text);
    if unread { title.strong() } else { title }
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
    /// Keep the password in the OS credential store after signing in.
    remember: bool,
    error: Option<String>,
}

struct App {
    ctx: egui::Context,
    settings: Settings,
    settings_path: Option<PathBuf>,
    settings_open: bool,
    /// The IMAP command log is open in its own window.
    log_open: bool,
    /// The signed-in account, shown in the window title.
    account: Option<String>,
    /// The account the window title was last set for.
    title_for: Option<String>,
    /// Size of the cache file on disk, refreshed every few seconds.
    cache_bytes: u64,
    cache_checked: Option<Instant>,
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
    login: LoginForm,

    /// Cached messages per label, from the cache. Shared, so the sidebar can hold it each frame.
    counts: Arc<BTreeMap<String, usize>>,
    /// Cached messages overall, for All Mail.
    messages_total: usize,
    /// Every label on the account, from the server. Includes labels with no cached messages.
    known_labels: Arc<BTreeSet<String>>,

    /// Rows of the thread list, from `window_start` on, read from the cache.
    window_rows: Vec<ThreadEntry>,
    window_start: usize,
    /// Threads in the list the window belongs to.
    window_total: usize,
    /// The list the window rows belong to. `None` until the first answer for a list.
    window_filter: Option<ThreadFilter>,
    /// The last window asked for, so the same one isn't asked for twice.
    sent_window: Option<(ThreadFilter, usize, usize)>,
    /// Something on screen has changed. Refreshes are throttled.
    view_stale: bool,
    /// When the list and conversation were last refreshed.
    view_asked: Instant,

    /// `(loaded, total)` while the initial list is loading.
    loading: Option<(usize, usize)>,
    /// The server pushes changes (IDLE), so periodic checks are off.
    idle_on: bool,
    checking: bool,
    auto_check: bool,
    last_check: Instant,
    /// UIDs whose body has been asked for and not yet delivered.
    requested_bodies: HashSet<u32>,
    /// Rendered blocks of messages in the reader, by UID.
    rendered: HashMap<u32, Vec<Block>>,
    /// A suspicious link waiting for the user to confirm it.
    pending_link: Option<LinkInfo>,

    /// Search text. A non-empty search replaces the label list.
    search: String,
    /// `None` shows All Mail, `Some(label)` filters by one raw label.
    filter: Option<String>,
    /// Checked conversations, by thread ID. An action on one applies to all its messages.
    checked: BTreeSet<u64>,
    /// The open conversation, by thread ID.
    open_thread: Option<u64>,
    /// The messages of the open conversation, by UID. Bodies arrive separately.
    thread_msgs: BTreeMap<u32, Message>,
    label_input: String,
    /// IMAP commands issued this session, newest last.
    log: Vec<String>,
    status: Option<String>,
}

impl App {
    fn new(ctx: egui::Context, creds: Option<(String, String)>) -> Self {
        let settings_path = settings::default_path();
        let settings = settings_path
            .as_deref()
            .map(Settings::load_from)
            .unwrap_or_default();
        let mut login = LoginForm {
            remember: settings.remember_password,
            ..Default::default()
        };
        if let Some(email) = &settings.email {
            login.email = email.clone();
            if settings.remember_password
                && let Some(password) = credentials::load_password(email)
            {
                login.password = password;
            }
        }
        let mut app = Self {
            ctx,
            settings,
            settings_path,
            settings_open: false,
            log_open: false,
            account: None,
            title_for: None,
            cache_bytes: 0,
            cache_checked: None,
            link: None,
            connecting: false,
            connected: false,
            reconnecting: false,
            mailbox_total: 0,
            cached_count: 0,
            login,
            counts: Arc::default(),
            messages_total: 0,
            known_labels: Arc::default(),
            window_rows: Vec::new(),
            window_start: 0,
            window_total: 0,
            window_filter: None,
            sent_window: None,
            view_stale: false,
            view_asked: Instant::now(),
            loading: None,
            idle_on: false,
            checking: false,
            auto_check: false,
            last_check: Instant::now(),
            requested_bodies: HashSet::new(),
            rendered: HashMap::new(),
            pending_link: None,
            search: String::new(),
            filter: None,
            checked: BTreeSet::new(),
            open_thread: None,
            thread_msgs: BTreeMap::new(),
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
        self.account = Some(user.clone());
        if self.settings.email.as_deref() != Some(user.as_str()) {
            self.settings.email = Some(user.clone());
            self.save_settings();
        }
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
                self.send(Command::Load {
                    limit: self.settings.page_size,
                });
            }
            Event::ConnectFailed(e) => {
                self.connecting = false;
                self.link = None;
                self.login.error = Some(e);
            }
            Event::Reset => {
                self.counts = Arc::default();
                self.messages_total = 0;
                self.cached_count = 0;
                self.change_list();
                self.thread_msgs.clear();
                self.requested_bodies.clear();
                self.rendered.clear();
                self.open_thread = None;
                self.checked.clear();
                self.view_stale = true;
            }
            Event::Progress { cached, total } => {
                self.cached_count = cached;
                self.mailbox_total = total;
                if self.loading.is_some() {
                    self.loading = Some((cached, total));
                }
                self.view_stale = true;
            }
            Event::Reconnecting => self.reconnecting = true,
            Event::Reconnected => {
                self.reconnecting = false;
                self.status = None;
            }
            Event::LoadStarted { total } => self.loading = Some((self.cached_count, total)),
            Event::LoadFinished => {
                self.loading = None;
                self.last_check = Instant::now();
                self.view_stale = true;
            }
            Event::CheckFinished => {
                self.checking = false;
                self.last_check = Instant::now();
                self.view_stale = true;
            }
            Event::NewArrivals(msgs) => {
                if self.settings.notifications {
                    let items: Vec<notify::NewMail> = msgs
                        .iter()
                        .filter(|m| !m.seen && m.labels.contains(SystemLabel::Inbox.as_wire()))
                        .map(|m| notify::NewMail {
                            from: m.from.clone(),
                            subject: m.subject.clone(),
                        })
                        .collect();
                    if let Err(e) = notify::new_mail(&items) {
                        eprintln!("Could not show a notification: {e}");
                    }
                }
            }
            Event::Body { uid, raw } => {
                self.requested_bodies.remove(&uid);
                if let Some(m) = self.thread_msgs.get_mut(&uid) {
                    m.body = Some(raw);
                }
            }
            Event::Labels { updates, command } => {
                for u in updates {
                    if let Some(m) = self.thread_msgs.get_mut(&u.uid) {
                        m.labels = u.labels;
                        m.seen = u.seen;
                    }
                }
                self.log.push(command);
                self.view_stale = true;
            }
            Event::Removed(uids) => {
                for uid in uids {
                    self.thread_msgs.remove(&uid);
                    self.requested_bodies.remove(&uid);
                    self.rendered.remove(&uid);
                }
                self.view_stale = true;
            }
            Event::IdleState(on) => self.idle_on = on,
            Event::LabelList(labels) => self.known_labels = Arc::new(labels.into_iter().collect()),
            Event::LabelCounts { labels, messages } => {
                self.counts = Arc::new(labels.into_iter().collect());
                self.messages_total = messages;
            }
            Event::Window {
                filter,
                start,
                threads,
                total,
            } => {
                // An answer for a list that has since changed is out of date, so it's dropped.
                if filter == self.thread_filter() {
                    self.window_filter = Some(filter);
                    self.window_start = start;
                    self.window_rows = threads;
                    self.window_total = total;
                }
            }
            Event::Thread { thrid, messages } => {
                if self.open_thread == Some(thrid) {
                    // Bodies already fetched are kept: they aren't in the cache's headers.
                    let mut old = std::mem::take(&mut self.thread_msgs);
                    for mut m in messages {
                        if let Some(prev) = old.remove(&m.uid) {
                            m.body = prev.body;
                        }
                        self.thread_msgs.insert(m.uid, m);
                    }
                    let present = &self.thread_msgs;
                    self.rendered.retain(|uid, _| present.contains_key(uid));
                }
            }
            Event::Error(e) => {
                self.checking = false;
                self.loading = None;
                self.status = Some(e);
            }
        }
    }

    /// The threads the list shows: the search, if there is one, else the label.
    fn thread_filter(&self) -> ThreadFilter {
        let text = self.search.trim();
        if !text.is_empty() {
            return ThreadFilter::Search(text.to_string());
        }
        match &self.filter {
            None => ThreadFilter::All,
            Some(label) => ThreadFilter::Label(label.clone()),
        }
    }

    /// Drops the list rows, so the next list is read from the cache.
    fn change_list(&mut self) {
        self.window_filter = None;
        self.window_rows.clear();
        self.window_total = 0;
        self.sent_window = None;
    }

    /// Shows the threads with one label, or All Mail for `None`. Clears the search.
    fn set_label_filter(&mut self, key: Option<String>) {
        self.filter = key;
        self.search.clear();
        self.change_list();
    }

    /// Asks the worker for rows of the thread list, unless that exact range was just asked for.
    fn request_window(&mut self, start: usize, count: usize) {
        let filter = self.thread_filter();
        let key = (filter.clone(), start, count);
        if self.sent_window.as_ref() == Some(&key) {
            return;
        }
        self.sent_window = Some(key);
        self.send(Command::Window {
            filter,
            start,
            count,
        });
    }

    /// Opens a conversation. Its messages are read from the cache, and their bodies follow.
    fn open_conversation(&mut self, thrid: u64) {
        self.open_thread = Some(thrid);
        self.thread_msgs.clear();
        self.rendered.clear();
        self.requested_bodies.clear();
        self.send(Command::Thread { thrid });
    }

    /// Re-reads the rows on screen and the open conversation after a change. Repeats are
    /// throttled, so a long load doesn't ask for a refresh on every batch.
    fn refresh_views(&mut self) {
        if !self.view_stale {
            return;
        }
        if self.view_asked.elapsed() < Duration::from_millis(250) {
            self.ctx.request_repaint_after(Duration::from_millis(250));
            return;
        }
        self.view_stale = false;
        self.view_asked = Instant::now();
        if let Some((_, start, count)) = self.sent_window.take() {
            self.request_window(start, count);
        }
        if let Some(thrid) = self.open_thread {
            self.send(Command::Thread { thrid });
        }
    }

    /// Asks the worker for a message's body unless it is already here or on its way.
    fn request_body(&mut self, uid: u32) {
        let has_body = self.thread_msgs.get(&uid).is_some_and(|m| m.body.is_some());
        if has_body || self.requested_bodies.contains(&uid) {
            return;
        }
        self.requested_bodies.insert(uid);
        self.send(Command::FetchBody { uid });
    }

    /// The conversations an action applies to: the checked ones, or else the open one.
    fn target_thrids(&self) -> Vec<u64> {
        if !self.checked.is_empty() {
            self.checked.iter().copied().collect()
        } else {
            self.open_thread.into_iter().collect()
        }
    }

    fn apply(&mut self, action: LabelAction, labels: Vec<String>) {
        let thrids = self.target_thrids();
        if thrids.is_empty() {
            self.status = Some("Select a message or check some boxes first.".into());
            return;
        }
        self.status = None;
        self.send(Command::Modify {
            thrids,
            action,
            labels,
        });
    }

    fn check_now(&mut self) {
        if !self.connected || self.checking || self.loading.is_some() {
            return;
        }
        self.checking = true;
        self.last_check = Instant::now();
        self.send(Command::CheckNew);
    }

    fn check_interval(&self) -> Duration {
        Duration::from_secs(self.settings.check_interval_secs)
    }

    fn save_settings(&mut self) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        if let Err(e) = self.settings.save_to(&path) {
            self.status = Some(format!("Could not save settings: {e}"));
        }
    }

    /// Saves or forgets the password after a sign-in, according to the checkbox.
    fn remember_password(&mut self, user: &str, pass: &str) {
        self.settings.remember_password = self.login.remember;
        let result = if self.login.remember {
            credentials::save_password(user, pass)
        } else {
            credentials::forget_password(user)
        };
        if let Err(e) = result {
            self.status = Some(e);
        }
        self.save_settings();
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut forget = false;
        egui::Window::new("Settings")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("Check for new mail every (seconds)");
                        ui.add(
                            egui::DragValue::new(&mut self.settings.check_interval_secs)
                                .range(*CHECK_INTERVAL_RANGE.start()..=*CHECK_INTERVAL_RANGE.end()),
                        );
                        ui.end_row();

                        ui.label("Messages per page");
                        ui.add(
                            egui::DragValue::new(&mut self.settings.page_size)
                                .range(*PAGE_SIZE_RANGE.start()..=*PAGE_SIZE_RANGE.end()),
                        );
                        ui.end_row();
                    });
                ui.add_space(8.0);
                ui.checkbox(
                    &mut self.settings.remember_password,
                    "Remember password on this computer",
                );
                ui.checkbox(
                    &mut self.settings.notifications,
                    "Show desktop notifications for new mail",
                );
                ui.add_space(8.0);
                ui.label(
                    "Gmail session key, for \"View original\" (the ik= value from a Gmail URL):",
                );
                let mut key = self.settings.gmail_session_key.clone().unwrap_or_default();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut key)
                            .hint_text("optional")
                            .desired_width(240.0),
                    )
                    .changed()
                {
                    let key = key.trim();
                    self.settings.gmail_session_key = (!key.is_empty()).then(|| key.to_string());
                }
                ui.weak("The page size applies the next time the mailbox loads.");
                ui.add_space(8.0);
                if let Some(email) = self.settings.email.clone()
                    && ui.button("Forget saved password").clicked()
                {
                    forget = true;
                    self.status = credentials::forget_password(&email).err();
                }
            });
        if forget {
            self.settings.remember_password = false;
            self.login.remember = false;
        }
        self.settings = std::mem::take(&mut self.settings).clamped();
        if !open {
            self.settings_open = false;
            self.login.remember = self.settings.remember_password;
            self.save_settings();
        }
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
            ui.checkbox(&mut self.login.remember, "Remember password on this computer");
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
                self.remember_password(&user, &pass);
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

    /// Where this account's cache file lives, if the account is known.
    fn cache_path(&self) -> Option<PathBuf> {
        self.account
            .as_deref()
            .and_then(gmail_imap_proto::store::account_cache_path)
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
                if !self.idle_on {
                    ui.checkbox(&mut self.auto_check, "Check every 60s");
                }
            }

            let cached = ui.label(format!(
                "{} cached ({}) / {} in mailbox",
                self.cached_count,
                fmt_bytes(self.cache_bytes),
                self.mailbox_total
            ));
            if let Some(path) = self.cache_path() {
                cached.on_hover_text(format!("Cache file: {}", path.display()));
            }
            if self.cached_count < self.mailbox_total {
                ui.spinner();
                ui.label("Caching headers…");
            }

            // Right-aligned: the first button added sits at the far right.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    self.settings_open = true;
                }
                if ui.button("Log").clicked() {
                    self.log_open = !self.log_open;
                }
            });
        });
    }

    /// Pins a label to the top of the sidebar, or unpins it.
    fn toggle_favorite(&mut self, key: &str) {
        let favs = &mut self.settings.favorite_labels;
        if let Some(i) = favs.iter().position(|f| f == key) {
            favs.remove(i);
        } else {
            favs.push(key.to_string());
        }
        self.save_settings();
    }

    /// One sidebar row: a favorite toggle, then the label, which filters the list.
    /// `key` is `None` for All Mail, which can't be favorited.
    fn label_row(&mut self, ui: &mut egui::Ui, key: Option<&str>, name: &str, count: usize) {
        let is_fav = key.is_some_and(|k| self.settings.favorite_labels.iter().any(|f| f == k));
        let selected = self.filter.as_deref() == key;
        let mut toggle = false;
        ui.horizontal(|ui| {
            if key.is_some() {
                let star = if is_fav { "★" } else { "☆" };
                let hover = if is_fav {
                    "Unpin from favorites"
                } else {
                    "Pin to favorites"
                };
                if ui.small_button(star).on_hover_text(hover).clicked() {
                    toggle = true;
                }
            }
            // Truncate to the space the sidebar has, so a long name can't stop it shrinking.
            let width = ui.available_width().max(40.0);
            let height = ui.spacing().interact_size.y;
            let text = format!("{name} ({count})");
            let row = egui::Button::selectable(selected, text.clone()).truncate();
            if ui
                .add_sized([width, height], row)
                .on_hover_text(text)
                .clicked()
            {
                self.set_label_filter(key.map(str::to_owned));
            }
        });
        if toggle && let Some(k) = key {
            self.toggle_favorite(k);
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.heading("Labels");
        ui.separator();
        // Shared handles, so the rows can call `&mut self` methods without copying the labels.
        let counts = Arc::clone(&self.counts);
        let known = Arc::clone(&self.known_labels);
        egui::ScrollArea::vertical()
            .id_salt("sidebar")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let favorites = self.settings.favorite_labels.clone();
                if !favorites.is_empty() {
                    ui.weak("Favorites");
                    for key in &favorites {
                        let count = counts.get(key).copied().unwrap_or(0);
                        self.label_row(ui, Some(key), &display_label(key), count);
                    }
                    ui.separator();
                }

                ui.weak("Mailboxes");
                for (name, key) in MAILBOXES {
                    let count = match key {
                        None => self.messages_total,
                        Some(k) => counts.get(k).copied().unwrap_or(0),
                    };
                    self.label_row(ui, key, name, count);
                }

                // Labels from the server, plus any seen in the cache, with counts.
                let mut user: BTreeMap<String, usize> = counts
                    .iter()
                    .filter(|(k, _)| !k.starts_with('\\'))
                    .map(|(k, c)| (k.clone(), *c))
                    .collect();
                for label in known.iter() {
                    user.entry(label.clone()).or_insert(0);
                }
                if !user.is_empty() {
                    ui.separator();
                    ui.weak("Labels");
                    for (k, count) in &user {
                        self.label_row(ui, Some(k), k, *count);
                    }
                }
            });
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
            let search = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search mail")
                    .desired_width(180.0),
            );
            if search.changed() {
                self.change_list();
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

    /// The thread list. Rows are read from the cache a window at a time, and only
    /// the rows on screen are drawn. Every row is exactly one widget tall, so the
    /// virtual list can place rows without measuring them.
    fn message_list(&mut self, ui: &mut egui::Ui) {
        let filter = self.thread_filter();
        if self.window_filter.as_ref() != Some(&filter) {
            // A new list, or one that was reset: read its first rows.
            self.request_window(0, WINDOW_CHUNK);
            ui.label("Loading…");
            return;
        }
        if self.window_total == 0 {
            ui.label(match filter {
                ThreadFilter::Search(_) => "No messages match this search.",
                _ if self.loading.is_some() => "Waiting for messages…",
                _ => "No messages with this label.",
            });
            return;
        }

        let row_height = ui.spacing().interact_size.y;
        let filter_label: Option<String> = match &filter {
            ThreadFilter::Label(label) => Some(label.clone()),
            _ => None,
        };
        let total = self.window_total;
        let start = self.window_start;
        // Take the rows out of `self` so they can be drawn while `self` is used.
        let rows = std::mem::take(&mut self.window_rows);
        let mut wanted: Option<(usize, usize)> = None;
        let mut clicked_label: Option<String> = None;
        let mut opened: Option<u64> = None;
        egui::ScrollArea::vertical()
            .id_salt("message_list")
            .show_rows(ui, row_height, total, |ui, range| {
                wanted = Some((range.start, range.len()));
                for i in range {
                    let Some(entry) = i.checked_sub(start).and_then(|k| rows.get(k)) else {
                        // Not read yet. Keep the row's place so the list doesn't jump.
                        let _ = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), row_height),
                            egui::Sense::hover(),
                        );
                        continue;
                    };
                    let m = &entry.head;
                    let thrid = entry.thrid;
                    let count = entry.count;
                    let unread = entry.unread > 0;
                    ui.horizontal(|ui| {
                        // The gap between items is set here, and the widths below assume it.
                        ui.spacing_mut().item_spacing.x = ROW_GAP;
                        // Leave room for the scrollbar, which can sit over the right edge.
                        let pane_w = ui.available_width() - ui.spacing().scroll.allocated_width();
                        let mut checked = self.checked.contains(&thrid);
                        let box_changed = ui
                            .add_sized(
                                [CHECK_WIDTH, row_height],
                                egui::Checkbox::without_text(&mut checked),
                            )
                            .changed();
                        if box_changed {
                            if checked {
                                self.checked.insert(thrid);
                            } else {
                                self.checked.remove(&thrid);
                            }
                        }
                        let open = self.open_thread == Some(thrid);
                        // Every item has a fixed width, so the row can't grow frame to frame.
                        // The date is the fixed column on the right. The subject takes the rest.
                        let title_text = row_title(m, count, unread);
                        let chips = row_chips(m, filter_label.as_deref());
                        let (shown, hidden) = chips.split_at(chips.len().min(MAX_CHIPS));
                        // (raw label to filter by, shown text, tooltip). The "+N" chip has no label.
                        let mut tags: Vec<(Option<String>, String, String)> = shown
                            .iter()
                            .map(|raw| (Some(raw.clone()), display_label(raw), display_label(raw)))
                            .collect();
                        if !hidden.is_empty() {
                            let names: Vec<String> =
                                hidden.iter().map(|r| display_label(r)).collect();
                            tags.push((None, format!("+{}", hidden.len()), names.join("\n")));
                        }
                        let font = egui::TextStyle::Small.resolve(ui.style());
                        let widths: Vec<f32> = tags
                            .iter()
                            .map(|(_, t, _)| chip_width(ui, t, &font))
                            .collect();
                        // Gaps: subject to first chip, between chips, last chip to date.
                        // Items in order: checkbox, subject, chips, date. That's one gap
                        // between each pair.
                        let gaps = (tags.len() as f32 + 2.0) * ROW_GAP;
                        let title_w =
                            (pane_w - CHECK_WIDTH - DATE_WIDTH - widths.iter().sum::<f32>() - gaps)
                                .max(40.0);

                        let band = if open {
                            ui.visuals().selection.bg_fill
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        let title = egui::Frame::new()
                            .fill(band)
                            .corner_radius(4)
                            .show(ui, |ui| {
                                ui.set_min_size(egui::vec2(title_w, row_height));
                                ui.set_max_width(title_w);
                                ui.add(
                                    egui::Label::new(title_text)
                                        .truncate()
                                        .sense(egui::Sense::click()),
                                )
                            })
                            .inner;
                        if title.on_hover_text(format!("thread {thrid}")).clicked() {
                            opened = Some(thrid);
                        }

                        for ((raw, text, tip), w) in tags.iter().zip(&widths) {
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(*w, row_height),
                                egui::Sense::click(),
                            );
                            paint_chip(ui, rect, text, &font);
                            if resp.on_hover_text(tip).clicked()
                                && let Some(label) = raw
                            {
                                clicked_label = Some(label.clone());
                            }
                        }

                        let date = egui::RichText::new(&m.date).weak();
                        ui.add_sized([DATE_WIDTH, row_height], egui::Label::new(date).truncate());
                    });
                }
            });
        self.window_rows = rows;

        if let Some(thrid) = opened {
            self.open_conversation(thrid);
        }
        // Clicking a label chip shows the threads with that label. Otherwise, read the
        // rows on screen if they aren't held yet.
        if let Some(label) = clicked_label {
            self.set_label_filter(Some(label));
        } else if let Some((first, len)) = wanted {
            let covered = first >= self.window_start
                && first + len <= self.window_start + self.window_rows.len();
            if !covered {
                let from = first.saturating_sub(WINDOW_PAD);
                let count = (len + 2 * WINDOW_PAD).max(WINDOW_CHUNK);
                self.request_window(from, count);
            }
        }
    }

    /// The reader: every message of the open conversation, newest first.
    fn reader(&mut self, ui: &mut egui::Ui) {
        let Some(thrid) = self.open_thread else {
            ui.label("Open a message to read it.");
            return;
        };
        let mut thread: Vec<u32> = self.thread_msgs.keys().copied().collect();
        // Newest first, by when each message was sent.
        thread.sort_by_key(|uid| {
            std::cmp::Reverse(
                self.thread_msgs
                    .get(uid)
                    .map_or((0i64, 0u32), |m| (m.sent, m.uid)),
            )
        });

        // Every label on the conversation, system labels first.
        let mut labels: BTreeSet<String> = BTreeSet::new();
        for m in self.thread_msgs.values() {
            labels.extend(m.labels.iter().cloned());
        }
        let mut sorted_labels: Vec<&String> = labels.iter().collect();
        sorted_labels.sort_by_key(|l| (!l.starts_with('\\'), l.to_lowercase()));
        let chip_names: Vec<(String, String)> = sorted_labels
            .iter()
            .map(|l| ((*l).clone(), display_label(l)))
            .collect();

        // Bodies for the whole conversation, fetched in the background, and
        // each one rendered once.
        for &uid in &thread {
            self.request_body(uid);
        }
        for &uid in &thread {
            if self.rendered.contains_key(&uid) {
                continue;
            }
            let Some(body) = self.thread_msgs.get(&uid).and_then(|m| m.body.as_deref()) else {
                continue;
            };
            let text = render_plaintext(body).unwrap_or_else(|| "(could not parse message)".into());
            self.rendered.insert(uid, reader::blocks(&text));
        }
        if self.rendered.len() > 300 {
            let present = &self.thread_msgs;
            self.rendered.retain(|uid, _| present.contains_key(uid));
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
            ui.weak(format!(
                "thread {thrid} (0x{thrid:x}) · {} messages in this conversation",
                thread.len()
            ));
        });

        // The conversation's labels, as chips. A chip that's cut short has the full name on hover.
        let font = egui::TextStyle::Small.resolve(ui.style());
        let chip_h = ui.text_style_height(&egui::TextStyle::Small) + 6.0;
        let mut clicked_label: Option<String> = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
            for (raw, name) in &chip_names {
                let w = chip_width(ui, name, &font);
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(w, chip_h), egui::Sense::click());
                paint_chip(ui, rect, name, &font);
                if resp.on_hover_text(name).clicked() {
                    clicked_label = Some(raw.clone());
                }
            }
        });
        if let Some(label) = clicked_label {
            self.set_label_filter(Some(label));
        }
        ui.separator();
        if thread.is_empty() {
            ui.spinner();
            ui.label("Loading conversation…");
            return;
        }

        let newest = thread.first().copied();
        let session_key = self.settings.gmail_session_key.clone();
        let mut pending: Option<LinkInfo> = None;
        egui::ScrollArea::vertical()
            .id_salt("reader")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for &uid in &thread {
                    let Some(m) = self.thread_msgs.get(&uid) else {
                        continue;
                    };
                    let title = format!("{} — {}   {}", m.from, m.subject, m.date);
                    let title = if m.seen {
                        egui::RichText::new(title)
                    } else {
                        egui::RichText::new(title).strong()
                    };
                    let expanded = Some(uid) == newest;
                    ui.push_id(uid, |ui| {
                        egui::CollapsingHeader::new(title)
                            .default_open(expanded)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.monospace(format!("msgid {} (0x{:x})", m.msgid, m.msgid));
                                    if ui.small_button("View original").clicked() {
                                        let url =
                                            gmail_original_url(m.msgid, session_key.as_deref());
                                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                                    }
                                });
                                match self.rendered.get(&uid) {
                                    Some(blocks) => {
                                        for (i, block) in blocks.iter().enumerate() {
                                            match block {
                                                Block::Line(line) => {
                                                    render_line(ui, line, &mut pending)
                                                }
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
                                    None => {
                                        ui.spinner();
                                        ui.label("Loading message…");
                                        ui.monospace(format!(
                                            "From:    {}\nSubject: {}\nDate:    {}",
                                            m.from, m.subject, m.date
                                        ));
                                    }
                                }
                            });
                    });
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

    /// The IMAP commands sent this session, in their own window.
    fn log_window(&mut self, ctx: &egui::Context) {
        if !self.log_open {
            return;
        }
        let mut open = true;
        egui::Window::new("IMAP log")
            .default_size([720.0, 420.0])
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("log")
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.monospace(line);
                        }
                    });
            });
        if !open {
            self.log_open = false;
        }
    }
}

/// Bytes as a size for people: GB with one decimal, otherwise MB.
fn fmt_bytes(bytes: u64) -> String {
    const MB: f64 = 1_048_576.0;
    const GB: f64 = 1_073_741_824.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

/// The size of a SQLite cache on disk, including its write-ahead log.
fn cache_file_bytes(path: &Path) -> u64 {
    let wal = path.with_extension("sqlite-wal");
    [path.to_path_buf(), wal]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();

        if !self.connected {
            egui::CentralPanel::default().show(ui, |ui| self.login_dialog(ui));
            return;
        }
        self.refresh_views();

        // Window title: the app name and the signed-in account.
        if self.connected
            && let Some(account) = &self.account
            && self.title_for.as_deref() != Some(account.as_str())
        {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Title(format!(
                    "Gmail Labels Prototype — {account}"
                )));
            self.title_for = Some(account.clone());
        }
        // The cache file's size, checked every few seconds.
        if self.connected
            && self
                .cache_checked
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(5))
        {
            self.cache_checked = Some(Instant::now());
            self.cache_bytes = self.cache_path().map_or(0, |p| cache_file_bytes(&p));
        }

        // Auto-check: a timer that only runs while the toggle is on and nothing else is busy.
        if self.auto_check && !self.idle_on {
            if !self.checking
                && self.loading.is_none()
                && self.last_check.elapsed() >= self.check_interval()
            {
                self.check_now();
            }
            let remaining = self
                .check_interval()
                .saturating_sub(self.last_check.elapsed());
            ui.ctx()
                .request_repaint_after(remaining.max(Duration::from_secs(1)));
        }

        egui::Panel::top("status").show(ui, |ui| self.status_bar(ui));

        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(200.0)
            .min_size(120.0)
            .show(ui, |ui| self.sidebar(ui));

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));

        egui::CentralPanel::default().show(ui, |ui| {
            egui::Panel::top("list")
                .resizable(true)
                .default_size(260.0)
                .show(ui, |ui| self.message_list(ui));
            egui::CentralPanel::default().show(ui, |ui| self.reader(ui));
        });
        let ctx = ui.ctx().clone();
        self.link_confirmation(&ctx);
        self.settings_window(&ctx);
        self.log_window(&ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_with(labels: &[&str]) -> Message {
        Message::from_headers(
            1,
            2,
            3,
            labels.iter().map(|s| s.to_string()).collect(),
            true,
            b"From: a@example.com\r\nSubject: hi\r\n\r\n".to_vec(),
        )
    }

    #[test]
    fn chips_leave_out_the_current_label() {
        let m = message_with(&["Work", "\\Starred", "Home"]);
        assert_eq!(row_chips(&m, Some("Work")), vec!["\\Starred", "Home"]);
    }

    #[test]
    fn chips_leave_out_inbox_only_in_mailbox_views() {
        let m = message_with(&["\\Inbox", "Work"]);
        // All Mail and system folders are mailbox views: no Inbox chip.
        assert_eq!(row_chips(&m, None), vec!["Work"]);
        assert_eq!(row_chips(&m, Some("\\Starred")), vec!["Work"]);
        // A label view keeps Inbox.
        assert_eq!(row_chips(&m, Some("Work")), vec!["\\Inbox"]);
    }

    #[test]
    fn system_labels_come_first() {
        let m = message_with(&["Zeta", "\\Important", "Alpha"]);
        assert_eq!(row_chips(&m, None), vec!["\\Important", "Alpha", "Zeta"]);
    }
}
