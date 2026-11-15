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
use gmail_imap_proto::auth::AuthCheck;
use gmail_imap_proto::export;
use gmail_imap_proto::impersonation;
use gmail_imap_proto::labels::{LabelAction, SystemLabel};
use gmail_imap_proto::linkcheck::{self, LinkInfo};
use gmail_imap_proto::links::gmail_thread_url;
use gmail_imap_proto::render_plaintext;
use gmail_imap_proto::search;
use gmail_imap_proto::settings::{self, CHECK_INTERVAL_RANGE, PAGE_SIZE_RANGE, Settings};
use gmail_imap_proto::store::SearchHit;

use backend::Message;
use gmail_imap_proto::store::{StorageView, ThreadFilter};
use reader::{Block, Segment};
use worker::{Command, Event, Link, ThreadEntry};

/// The help window's text. Plain text: `== Heading ==` lines, `- ` bullets and paragraphs.
const HELP_TEXT: &str = include_str!("help.txt");

/// How many of the largest messages the storage view lists.
const STORAGE_ROWS: usize = 500;
/// How many messages a search lists.
const SEARCH_ROWS: usize = 500;

fn main() -> eframe::Result {
    let creds = match (
        std::env::var("GMAIL_USER"),
        std::env::var("GMAIL_APP_PASSWORD"),
    ) {
        (Ok(user), Ok(password)) => Some((user, password)),
        _ => None,
    };

    // The window opens where it was last time. Settings are read here, before the app exists.
    let settings = settings::default_path()
        .as_deref()
        .map(settings::Settings::load_from)
        .unwrap_or_default()
        .clamped();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Gmail Labels Prototype")
        .with_inner_size(settings.window.size);
    if let Some(position) = settings.window.position {
        viewport = viewport.with_position(position);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
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

/// One level of the label tree: sub-labels by name, and the label that is exactly this path.
#[derive(Default)]
struct LabelNode {
    label: Option<String>,
    children: BTreeMap<String, LabelNode>,
}

/// The sidebar's label tree and the counts it shows, kept until the labels or counts change.
struct LabelCache {
    tree: LabelNode,
    counts: BTreeMap<String, (usize, usize)>,
}

/// The labels as a tree, split at "/". Gmail's IMAP server uses "/" to nest labels.
fn label_tree<'a>(labels: impl Iterator<Item = &'a String>) -> LabelNode {
    let mut root = LabelNode::default();
    for raw in labels {
        let mut node = &mut root;
        for segment in raw.split('/') {
            node = node.children.entry(segment.to_string()).or_default();
        }
        node.label = Some(raw.clone());
    }
    root
}

/// A time for people: the local time, without the time zone. Empty if unknown.
fn format_time(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%a, %d %b %Y %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// A subject without its reply and forward prefixes, lowercased, so that a thread's
/// messages compare as one subject.
fn base_subject(subject: &str) -> String {
    let mut s = subject.trim().to_lowercase();
    loop {
        let before = s.len();
        for prefix in ["re:", "fwd:", "fw:"] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim_start().to_string();
            }
        }
        if s.len() == before {
            return s;
        }
    }
}

/// Mailboxes always shown in the sidebar, even when the cache has none of their
/// messages. `None` is All Mail, which is every message.
const MAILBOXES: [(&str, Option<&str>); 8] = [
    ("Inbox", Some("\\Inbox")),
    ("All Mail", None),
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
/// Width of the attachment column, between the subject and the labels.
const ATTACH_WIDTH: f32 = 16.0;

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

/// Width of the accent bar beside each message in the reader.
const ACCENT_WIDTH: f32 = 3.0;
/// Space between the accent bar and the message.
const ACCENT_GAP: f32 = 12.0;

/// Colours for senders. A sender's colour is picked from these by a hash of the name,
/// so each sender keeps the same colour from run to run. They're mid-tone, so they show
/// on both light and dark backgrounds.
const SENDER_COLORS: [egui::Color32; 8] = [
    egui::Color32::from_rgb(231, 111, 81),
    egui::Color32::from_rgb(42, 157, 143),
    egui::Color32::from_rgb(233, 196, 106),
    egui::Color32::from_rgb(138, 177, 125),
    egui::Color32::from_rgb(106, 137, 204),
    egui::Color32::from_rgb(199, 125, 187),
    egui::Color32::from_rgb(244, 162, 97),
    egui::Color32::from_rgb(120, 144, 156),
];

/// Index into `SENDER_COLORS` for a sender. Case and surrounding space don't count.
fn sender_slot(from: &str) -> usize {
    // FNV-1a: the same hash on every run and every platform, unlike the std hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in from.trim().to_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash % SENDER_COLORS.len() as u64) as usize
}

/// The colour for a sender's stripe in the reader.
fn sender_color(from: &str) -> egui::Color32 {
    SENDER_COLORS[sender_slot(from)]
}

/// What a sender's colour is keyed on: the address when there is one. A display name
/// can be anything, so a name that copies a known sender's doesn't get that sender's colour.
fn sender_key(m: &Message) -> &str {
    if m.from_email.is_empty() {
        &m.from
    } else {
        &m.from_email
    }
}

/// The sender as "Name <address>", so the address is in view next to the name. Only the
/// name is shown when there's no address, or when the name is the address.
fn sender_line(m: &Message) -> String {
    if m.from_email.is_empty() || m.from.eq_ignore_ascii_case(&m.from_email) {
        m.from.clone()
    } else {
        format!("{} <{}>", m.from, m.from_email)
    }
}

/// Width of the expand/collapse arrow's gutter in the label tree. Every label row has one.
const ARROW_GUTTER: f32 = 14.0;

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

/// Paints a paperclip-like mark into `rect`: a tall rounded loop.
fn paint_paperclip(ui: &egui::Ui, rect: egui::Rect) {
    let color = ui.visuals().text_color();
    let clip = egui::Rect::from_center_size(rect.center(), egui::vec2(7.0, 12.0));
    ui.painter().rect_stroke(
        clip,
        3.5,
        egui::Stroke::new(1.3, color),
        egui::StrokeKind::Middle,
    );
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
/// The SPF, DKIM and DMARC verdicts for a message, each with its details on hover. A
/// method with a pass counts as a pass. Otherwise the first verdict is shown.
fn auth_line(ui: &mut egui::Ui, checks: &[AuthCheck]) {
    ui.horizontal(|ui| {
        for method in ["spf", "dkim", "dmarc"] {
            let of_method = || checks.iter().filter(move |c| c.method == method);
            let chosen = of_method()
                .find(|c| c.result == "pass")
                .or_else(|| of_method().next());
            let label = method.to_uppercase();
            match chosen {
                Some(check) => {
                    let text = egui::RichText::new(format!("{label}: {}", check.result));
                    let text = match check.result.as_str() {
                        "pass" => text.color(egui::Color32::from_rgb(80, 170, 90)),
                        "none" | "neutral" => text.weak(),
                        _ => text.color(egui::Color32::LIGHT_RED),
                    };
                    ui.label(text).on_hover_text(check.detail.clone());
                }
                None => {
                    ui.weak(format!("{label}: not reported"));
                }
            }
        }
    });
}

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
    /// The help window is open.
    help_open: bool,
    /// The largest cached messages, once the storage view has asked for them.
    storage: Option<Arc<StorageView>>,
    /// Whether the list shows the storage view instead of threads.
    storage_open: bool,
    /// When the storage view last asked the cache, so it refreshes while sizes arrive.
    storage_asked: Instant,
    /// Threads whose notifications were clicked, sent from the notification threads.
    click_tx: std::sync::mpsc::Sender<u64>,
    click_rx: std::sync::mpsc::Receiver<u64>,
    /// The sidebar's labels, built from the counts. `None` until they're needed again.
    label_cache: Option<LabelCache>,
    /// The last search the cache answered, and the messages it found.
    search_results: Option<(search::Query, Arc<Vec<SearchHit>>)>,
    /// The search last sent to the cache, so each one is sent once.
    search_asked: Option<search::Query>,
    /// Whether the sign-in is the automatic one at startup, so the window can say so.
    auto_signin: bool,
    /// Whether the window layout has changed since it was last saved.
    layout_dirty: bool,
    /// When the layout was last saved.
    layout_saved: Instant,
    /// The sign-in, kept in memory for the session so a crashed worker can be restarted.
    credentials: Option<(String, String)>,
    /// Why the background work stopped, if a thread crashed. The status bar offers a restart.
    crashed: Option<String>,
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
    counts: Arc<BTreeMap<String, (usize, usize)>>,
    /// Cached messages overall, for All Mail.
    /// Threads in all, and how many of them are unread.
    totals: (usize, usize),
    /// Every label on the account, from the server. Includes labels with no cached messages.
    known_labels: Arc<BTreeSet<String>>,

    /// Rows of the thread list, from `window_start` on, read from the cache.
    window_rows: Vec<ThreadEntry>,
    window_start: usize,
    /// Threads in the list the window belongs to.
    window_total: usize,
    /// The list the window rows belong to. `None` until the first answer for a list.
    window_filter: Option<ThreadFilter>,
    /// Parent labels in the sidebar whose children are showing.
    label_open: HashSet<String>,
    /// The row of the last checkbox click, for a shift-click to extend from.
    check_anchor: Option<usize>,
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
    /// The message whose original is open in the dialog, by UID.
    original: Option<u32>,
    /// The messages of the open conversation, by UID. Bodies arrive separately.
    thread_msgs: BTreeMap<u32, Message>,
    /// The conversation was just opened, so its first load marks it read. Later loads don't,
    /// so a manual "Mark unread" sticks.
    mark_read_on_open: bool,
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
        let mut auto_login = None;
        if let Some(email) = &settings.email {
            login.email = email.clone();
            if settings.remember_password
                && let Some(password) = credentials::load_password(email)
            {
                if settings.auto_login {
                    auto_login = Some((email.clone(), password.clone()));
                }
                login.password = password;
            }
        }
        let (click_tx, click_rx) = std::sync::mpsc::channel();
        let mut app = Self {
            ctx,
            settings,
            settings_path,
            settings_open: false,
            log_open: false,
            help_open: false,
            account: None,
            credentials: None,
            storage: None,
            storage_open: false,
            storage_asked: Instant::now(),
            click_tx,
            click_rx,
            label_cache: None,
            search_results: None,
            search_asked: None,
            auto_signin: false,
            layout_dirty: false,
            layout_saved: Instant::now(),
            crashed: None,
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
            totals: (0, 0),
            known_labels: Arc::default(),
            window_rows: Vec::new(),
            window_start: 0,
            window_total: 0,
            window_filter: None,
            sent_window: None,
            check_anchor: None,
            label_open: HashSet::new(),
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
            // The Inbox is the first view on sign-in.
            filter: Some("\\Inbox".to_string()),
            checked: BTreeSet::new(),
            open_thread: None,
            thread_msgs: BTreeMap::new(),
            original: None,
            mark_read_on_open: false,
            label_input: String::new(),
            log: Vec::new(),
            status: None,
        };
        // Environment variables take precedence over a remembered password.
        let auto_signin = creds.is_none() && auto_login.is_some();
        if let Some((user, password)) = creds.or(auto_login) {
            app.auto_signin = auto_signin;
            app.start_gmail(user, password);
        }
        app
    }

    fn start_gmail(&mut self, user: String, password: String) {
        self.login.error = None;
        // Kept in memory only, for the session, so a crashed worker can be restarted.
        self.credentials = Some((user.clone(), password.clone()));
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
            // Cache reads go to the reader thread when there is one, so they don't wait
            // behind the worker's network calls.
            let to = match &link.reads {
                Some(reads) if worker::is_cache_read(&cmd) => reads,
                _ => &link.commands,
            };
            let _ = to.send(cmd);
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
                self.label_cache = None;
                self.totals = (0, 0);
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
                    let new: Vec<&Message> = msgs
                        .iter()
                        .filter(|m| !m.seen && m.labels.contains(SystemLabel::Inbox.as_wire()))
                        .collect();
                    let items: Vec<notify::NewMail> = new
                        .iter()
                        .map(|m| notify::NewMail {
                            from: m.from.clone(),
                            subject: m.subject.clone(),
                        })
                        .collect();
                    // Clicking the notification opens the newest message in the batch.
                    let newest = new.iter().max_by_key(|m| m.when()).map(|m| m.thrid);
                    let tx = self.click_tx.clone();
                    let ctx = self.ctx.clone();
                    let on_click = move || {
                        if let Some(thrid) = newest {
                            let _ = tx.send(thrid);
                        }
                        ctx.request_repaint();
                    };
                    if let Err(e) = notify::new_mail(&items, on_click) {
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
            Event::LabelList(labels) => {
                self.known_labels = Arc::new(labels.into_iter().collect());
                self.label_cache = None;
            }
            Event::LabelCounts {
                labels,
                threads,
                unread,
            } => {
                self.counts = Arc::new(
                    labels
                        .into_iter()
                        .map(|(label, t, u)| (label, (t, u)))
                        .collect(),
                );
                self.label_cache = None;
                self.totals = (threads, unread);
            }
            Event::ThreadRange {
                filter,
                thrids,
                checked,
                ..
            } => {
                // A range for a list that has since changed is dropped.
                if filter == self.thread_filter() {
                    for thrid in thrids {
                        if checked {
                            self.checked.insert(thrid);
                        } else {
                            self.checked.remove(&thrid);
                        }
                    }
                }
            }
            Event::Largest(view) => self.storage = Some(Arc::new(view)),
            Event::Search { query, hits } => {
                self.search_results = Some((query, Arc::new(hits)));
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
                    if std::mem::take(&mut self.mark_read_on_open)
                        && self.thread_msgs.values().any(|m| !m.seen)
                    {
                        self.set_read_state(vec![thrid], true);
                    }
                }
            }
            Event::Error(e) => {
                self.checking = false;
                self.loading = None;
                self.status = Some(e);
            }
            Event::Crashed(why) => {
                self.checking = false;
                self.loading = None;
                self.crashed = Some(why);
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

    /// Drops the list rows, so the next list is read from the cache. The row numbers
    /// mean something else in the next list, so the shift-click anchor goes too.
    fn change_list(&mut self) {
        self.storage_open = false;
        self.search_asked = None;
        self.window_filter = None;
        self.window_rows.clear();
        self.window_total = 0;
        self.sent_window = None;
        self.check_anchor = None;
    }

    /// Checks or unchecks the threads in a range of the list, once the worker has them.
    fn request_thread_range(&mut self, start: usize, count: usize, checked: bool) {
        let filter = self.thread_filter();
        self.send(Command::ThreadRange {
            filter,
            start,
            count,
            checked,
        });
    }

    /// Shows the threads with one label, or All Mail for `None`. Clears the search.
    fn set_label_filter(&mut self, key: Option<String>) {
        self.filter = key;
        self.search.clear();
        self.change_list();
    }

    /// Marks conversations read or unread. The open conversation shows the change at once.
    /// The worker writes the cache before it talks to the server, and it handles requests
    /// in order, so the list refreshed after this reads the new state.
    fn set_read_state(&mut self, thrids: Vec<u64>, seen: bool) {
        if self.open_thread.is_some_and(|t| thrids.contains(&t)) {
            for m in self.thread_msgs.values_mut() {
                m.seen = seen;
            }
        }
        self.view_stale = true;
        self.send(Command::SetSeen { thrids, seen });
    }

    /// Marks the checked conversations, or else the open one, read or unread.
    fn apply_read_state(&mut self, seen: bool) {
        let thrids = self.target_thrids();
        if thrids.is_empty() {
            self.status = Some("Select a message or check some boxes first.".into());
            return;
        }
        self.status = None;
        self.set_read_state(thrids, seen);
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
    /// Sends the search box's query to the cache, once for each change to it.
    fn refresh_search(&mut self) {
        let query = search::parse(&self.search).query;
        if query.is_empty() || self.search_asked.as_ref() == Some(&query) {
            return;
        }
        self.search_asked = Some(query.clone());
        self.send(Command::Search {
            query,
            limit: SEARCH_ROWS,
        });
    }

    /// The search results: one row per matching message, newest first. A row opens its
    /// conversation in the reader.
    fn search_list(&mut self, ui: &mut egui::Ui) {
        let parsed = search::parse(&self.search);
        ui.heading(format!("Search: {}", self.search.trim()));
        for warning in &parsed.warnings {
            ui.weak(format!("Ignored: {warning}"));
        }
        if parsed.query.is_empty() {
            ui.weak("Type words or operators, such as from:alice or has:attachment.");
            return;
        }
        let results = match &self.search_results {
            Some((query, hits)) if *query == parsed.query => Arc::clone(hits),
            _ => {
                ui.weak("Searching…");
                return;
            }
        };
        ui.weak(format!(
            "{} messages, newest first. Only cached messages are searched.",
            results.len()
        ));
        ui.separator();
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        egui::ScrollArea::vertical()
            .id_salt("search")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, results.len(), |ui, range| {
                for hit in &results[range] {
                    ui.horizontal(|ui| {
                        ui.weak(format_time(hit.when));
                        let subject = if hit.subject.is_empty() {
                            "(no subject)"
                        } else {
                            hit.subject.as_str()
                        };
                        if ui
                            .selectable_label(false, format!("{} — {}", hit.from, subject))
                            .on_hover_text("Open this conversation")
                            .clicked()
                        {
                            self.open_conversation(hit.thrid);
                        }
                        if !hit.seen {
                            ui.strong("unread");
                        }
                        if hit.attachment {
                            ui.weak("attachment");
                        }
                    });
                }
            });
    }

    /// Opens the thread of each notification the user clicked, and brings the window forward.
    fn open_clicked_notifications(&mut self, ctx: &egui::Context) {
        while let Ok(thrid) = self.click_rx.try_recv() {
            self.open_conversation(thrid);
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    /// Opens the storage view: the largest cached messages, biggest first.
    fn open_storage(&mut self) {
        self.search.clear();
        self.change_list();
        self.storage_open = true;
        self.storage_asked = Instant::now();
        self.send(Command::Largest {
            limit: STORAGE_ROWS,
        });
    }

    /// While sizes are still being backfilled, asks the cache again every few seconds.
    fn refresh_storage(&mut self, ctx: &egui::Context) {
        let complete = self.storage.as_ref().is_some_and(|v| v.sized >= v.total);
        if !self.storage_open || complete {
            return;
        }
        if self.storage_asked.elapsed() >= Duration::from_secs(3) {
            self.storage_asked = Instant::now();
            self.send(Command::Largest {
                limit: STORAGE_ROWS,
            });
        }
        ctx.request_repaint_after(Duration::from_secs(3));
    }

    /// The storage view. Each row shows a message's size and opens its thread.
    fn storage_list(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Largest messages");
            if ui.button("Refresh").clicked() {
                self.storage_asked = Instant::now();
                self.send(Command::Largest {
                    limit: STORAGE_ROWS,
                });
            }
        });
        let Some(view) = self.storage.clone() else {
            ui.weak("Loading…");
            return;
        };
        ui.weak(format!(
            "Sizes are known for {} of {} messages. The list fills in as the cache is checked.",
            view.sized, view.total
        ));
        ui.separator();
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        egui::ScrollArea::vertical()
            .id_salt("storage")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, view.rows.len(), |ui, range| {
                for row in &view.rows[range] {
                    ui.horizontal(|ui| {
                        ui.add_sized([80.0, row_height], egui::Label::new(fmt_bytes(row.size)));
                        let subject = if row.subject.is_empty() {
                            "(no subject)"
                        } else {
                            row.subject.as_str()
                        };
                        if ui
                            .selectable_label(false, format!("{} — {}", row.from, subject))
                            .clicked()
                        {
                            self.open_conversation(row.thrid);
                        }
                        if row.attachment {
                            ui.weak("attachment");
                        }
                        if let Some(t) = row.received {
                            ui.weak(format_time(t));
                        }
                    });
                }
            });
    }

    fn open_conversation(&mut self, thrid: u64) {
        self.open_thread = Some(thrid);
        self.mark_read_on_open = true;
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
                ui.add_enabled(
                    self.settings.remember_password,
                    egui::Checkbox::new(
                        &mut self.settings.auto_login,
                        "Sign in automatically when the app starts",
                    ),
                );
                ui.checkbox(
                    &mut self.settings.notifications,
                    "Show desktop notifications for new mail",
                );
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
                self.auto_signin = false;
                self.start_gmail(user, pass);
            }

            if busy {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    let text = if self.auto_signin {
                        format!("Signing in automatically as {}…", self.login.email.trim())
                    } else {
                        "Connecting…".to_string()
                    };
                    ui.label(text);
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

    /// Starts a new worker after the old one crashed. The cache is checked on the way in,
    /// since the crashed worker didn't close it cleanly.
    fn restart_worker(&mut self) {
        let Some((user, password)) = self.credentials.clone() else {
            self.status = Some("Sign in again to restart.".into());
            return;
        };
        self.crashed = None;
        // Dropping the old link ends the old threads, once they notice.
        self.link = None;
        self.change_list();
        self.thread_msgs.clear();
        self.rendered.clear();
        self.requested_bodies.clear();
        self.open_thread = None;
        self.reconnecting = false;
        self.start_gmail(user, password);
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let mut restart = false;
        if let Some(why) = &self.crashed {
            ui.horizontal(|ui| {
                ui.colored_label(egui::Color32::LIGHT_RED, why);
                if ui
                    .button("Restart")
                    .on_hover_text("Start the background work again. The cache is checked first.")
                    .clicked()
                {
                    restart = true;
                }
            });
        }
        if restart {
            self.restart_worker();
        }
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
                let fraction = self.cached_count as f32 / self.mailbox_total.max(1) as f32;
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(200.0)
                        .text(format!(
                            "Caching headers {} of {}",
                            self.cached_count, self.mailbox_total
                        )),
                );
            }

            // Right-aligned: the first button added sits at the far right.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Settings").clicked() {
                    self.settings_open = true;
                }
                if ui.button("Log").clicked() {
                    self.log_open = !self.log_open;
                }
                if ui.button("Help").clicked() {
                    self.help_open = true;
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
    /// `count` is (threads with the label, of those unread).
    fn label_row(
        &mut self,
        ui: &mut egui::Ui,
        key: Option<&str>,
        name: &str,
        count: (usize, usize),
    ) {
        let is_fav = key.is_some_and(|k| self.settings.favorite_labels.iter().any(|f| f == k));
        let selected = self.filter.as_deref() == key;
        let mut toggle = false;
        ui.horizontal(|ui| {
            // Truncate to the space the sidebar has, so a long name can't stop it shrinking.
            let width = ui.available_width().max(40.0);
            let height = ui.spacing().interact_size.y;
            let (threads, unread) = count;
            let text = format!("{name} ({unread}/{threads})");
            let hover = format!("{name}: {unread} unread of {threads} threads");
            let band = if selected {
                ui.visuals().selection.bg_fill
            } else {
                egui::Color32::TRANSPARENT
            };
            let frame = egui::Frame::new()
                .fill(band)
                .corner_radius(4)
                .show(ui, |ui| {
                    ui.set_min_size(egui::vec2(width, height));
                    ui.set_max_width(width);
                    ui.add(egui::Label::new(text).truncate());
                });
            let response = ui
                .interact(
                    frame.response.rect,
                    ui.scope_id().with(("label", key.unwrap_or(name))),
                    egui::Sense::click(),
                )
                .on_hover_text(hover);
            if response.clicked() {
                self.set_label_filter(key.map(str::to_owned));
            }
            // Right-click toggles the favourite. All Mail has no menu, since it can't be one.
            if key.is_some() {
                response.context_menu(|ui| {
                    let text = if is_fav {
                        "Remove from favorites"
                    } else {
                        "Add to favorites"
                    };
                    if ui.button(text).clicked() {
                        toggle = true;
                        ui.close();
                    }
                });
            }
        });
        if toggle && let Some(k) = key {
            self.toggle_favorite(k);
        }
    }

    /// One saved search in the sidebar. Clicking it runs the search, and the list follows the
    /// cache as it changes. Returns whether the user asked to remove it.
    fn search_row(&mut self, ui: &mut egui::Ui, text: &str) -> bool {
        let selected = self.search.trim() == text;
        let width = ui.available_width().max(40.0);
        let height = ui.spacing().interact_size.y;
        let band = if selected {
            ui.visuals().selection.bg_fill
        } else {
            egui::Color32::TRANSPARENT
        };
        let frame = egui::Frame::new()
            .fill(band)
            .corner_radius(4)
            .show(ui, |ui| {
                ui.set_min_size(egui::vec2(width, height));
                ui.set_max_width(width);
                ui.add(egui::Label::new(text).truncate());
            });
        let response = ui
            .interact(
                frame.response.rect,
                ui.scope_id().with(("search", text)),
                egui::Sense::click(),
            )
            .on_hover_text(format!("Search: {text}"));
        if response.clicked() {
            self.search = text.to_string();
            self.change_list();
        }
        let mut remove = false;
        response.context_menu(|ui| {
            if ui.button("Remove saved search").clicked() {
                remove = true;
                ui.close();
            }
        });
        remove
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        // Shared handles, so the rows can call `&mut self` methods without copying the labels.
        let counts = Arc::clone(&self.counts);
        let known = Arc::clone(&self.known_labels);
        egui::ScrollArea::vertical()
            .id_salt("sidebar")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let favorites = self.settings.favorite_labels.clone();
                if !favorites.is_empty() {
                    ui.heading("Favorites");
                    for key in &favorites {
                        let count = counts.get(key).copied().unwrap_or((0, 0));
                        self.label_row(ui, Some(key), &display_label(key), count);
                    }
                    ui.separator();
                }

                ui.heading("Mailboxes");
                for (name, key) in MAILBOXES {
                    let count = match key {
                        None => self.totals,
                        Some(k) => counts.get(k).copied().unwrap_or((0, 0)),
                    };
                    self.label_row(ui, key, name, count);
                }
                if ui.selectable_label(self.storage_open, "Storage").clicked() {
                    self.open_storage();
                }

                let active = self.search.trim().to_string();
                let recent = self.settings.recent_searches.clone();
                if !self.settings.saved_searches.is_empty()
                    || !active.is_empty()
                    || !recent.is_empty()
                {
                    ui.separator();
                    ui.heading("Searches");
                    let searches = self.settings.saved_searches.clone();
                    let mut remove: Option<usize> = None;
                    for (i, text) in searches.iter().enumerate() {
                        if self.search_row(ui, text) {
                            remove = Some(i);
                        }
                    }
                    if let Some(i) = remove {
                        self.settings.saved_searches.remove(i);
                        self.save_settings();
                    }
                    // The search on screen, in small type under the saved ones.
                    if !active.is_empty() {
                        ui.small(format!("Showing: {active}"));
                    }
                    if !recent.is_empty() {
                        egui::CollapsingHeader::new("Most recent")
                            .id_salt("recent_searches")
                            .default_open(false)
                            .show(ui, |ui| {
                                for text in &recent {
                                    let selected = active == *text;
                                    if ui.selectable_label(selected, text).clicked() {
                                        self.search = text.clone();
                                        self.settings.note_search(text);
                                        self.save_settings();
                                        self.change_list();
                                    }
                                }
                            });
                    }
                }

                // The label tree is built once per change to the labels or their counts, not per
                // frame. It's taken out while it's drawn, since drawing needs `&mut self`.
                if self.label_cache.is_none() {
                    // Labels from the server, plus any seen in the cache, with counts.
                    let mut user: BTreeMap<String, (usize, usize)> = counts
                        .iter()
                        .filter(|(k, _)| !k.starts_with('\\'))
                        .map(|(k, c)| (k.clone(), *c))
                        .collect();
                    for label in known.iter() {
                        user.entry(label.clone()).or_insert((0, 0));
                    }
                    let tree = label_tree(user.keys());
                    self.label_cache = Some(LabelCache { tree, counts: user });
                }
                if let Some(cache) = self.label_cache.take() {
                    if !cache.counts.is_empty() {
                        ui.separator();
                        ui.heading("Labels");
                        self.label_tree(ui, &cache.tree, &cache.counts, "");
                    }
                    self.label_cache = Some(cache);
                }
            });
    }

    /// The label tree: a label with sub-labels opens to show them, and its own row comes first.
    fn label_tree(
        &mut self,
        ui: &mut egui::Ui,
        node: &LabelNode,
        counts: &BTreeMap<String, (usize, usize)>,
        path: &str,
    ) {
        for (segment, child) in &node.children {
            let child_path = if path.is_empty() {
                segment.clone()
            } else {
                format!("{path}/{segment}")
            };
            let count = child
                .label
                .as_ref()
                .and_then(|l| counts.get(l))
                .copied()
                .unwrap_or((0, 0));
            // Every row starts with the same gutter, so the names at one level line up.
            // A parent's arrow sits in that gutter, and leaves leave it empty.
            let open = self.label_open.contains(&child_path);
            ui.horizontal(|ui| {
                let height = ui.spacing().interact_size.y;
                if child.children.is_empty() {
                    ui.allocate_exact_size(egui::vec2(ARROW_GUTTER, height), egui::Sense::hover());
                } else {
                    let arrow = if open { "▾" } else { "▸" };
                    let toggle = ui.add_sized(
                        [ARROW_GUTTER, height],
                        egui::Button::new(arrow).frame(false),
                    );
                    if toggle.clicked() {
                        if open {
                            self.label_open.remove(&child_path);
                        } else {
                            self.label_open.insert(child_path.clone());
                        }
                    }
                }
                match &child.label {
                    Some(raw) => self.label_row(ui, Some(raw.as_str()), segment, count),
                    None => {
                        ui.weak(segment.as_str());
                    }
                }
            });
            if open && !child.children.is_empty() {
                ui.indent(child_path.as_str(), |ui| {
                    self.label_tree(ui, child, counts, &child_path)
                });
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
            if ui.button("Mark read").clicked() {
                self.apply_read_state(true);
            }
            if ui.button("Mark unread").clicked() {
                self.apply_read_state(false);
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
            // Leaving the box, or pressing Enter in it, counts as running the search.
            if search.lost_focus() {
                let text = self.search.clone();
                self.settings.note_search(&text);
                self.save_settings();
            }
            let text = self.search.trim().to_string();
            let saveable = !text.is_empty()
                && !self.settings.saved_searches.contains(&text)
                && self.settings.saved_searches.len() < settings::MAX_SAVED_SEARCHES;
            let save = ui
                .add_enabled(saveable, egui::Button::new("Save search"))
                .on_disabled_hover_text("Type a search that isn't saved yet. Up to 10 are kept.");
            if save.clicked() && self.settings.save_search(&text) {
                self.save_settings();
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
        // The list keeps the height the user set, even while its content is short.
        ui.set_min_height(ui.available_height());
        let filter = self.thread_filter();
        if self.window_filter.as_ref() != Some(&filter) {
            // A new list, or one that was reset: read its first rows.
            self.request_window(0, WINDOW_CHUNK);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading…");
            });
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
        // How many threads the list holds, so a search's size is known at a glance.
        let noun = if self.window_total == 1 {
            "thread"
        } else {
            "threads"
        };
        ui.weak(match &filter {
            ThreadFilter::Search(text) => format!("{} {noun} match \"{text}\"", self.window_total),
            _ => format!("{} {noun}", self.window_total),
        });
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
        // Checkbox clicks in this frame: row, the state it was set to, and whether shift was held.
        let mut box_clicks: Vec<(usize, bool, bool)> = Vec::new();
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
                    // The highlight is painted behind the row, so it covers the whole row.
                    let row_band = if self.open_thread == Some(thrid) {
                        ui.visuals().selection.bg_fill
                    } else {
                        egui::Color32::TRANSPARENT
                    };
                    let highlight = ui.painter().add(egui::Shape::Noop);
                    let row = ui.horizontal(|ui| {
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
                            let shift = ui.input(|input| input.modifiers.shift);
                            box_clicks.push((i, checked, shift));
                        }
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
                        // Items in order: checkbox, subject, attachment, chips, date. That's
                        // one gap between each pair.
                        let gaps = (tags.len() as f32 + 3.0) * ROW_GAP;
                        let title_w = (pane_w
                            - CHECK_WIDTH
                            - DATE_WIDTH
                            - ATTACH_WIDTH
                            - widths.iter().sum::<f32>()
                            - gaps)
                            .max(40.0);

                        let title = egui::Frame::new()
                            .fill(egui::Color32::TRANSPARENT)
                            .corner_radius(4)
                            .show(ui, |ui| {
                                ui.set_min_size(egui::vec2(title_w, row_height));
                                ui.set_max_width(title_w);
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    if unread {
                                        // Bold isn't available in this font, so unread gets a dot.
                                        let (dot, _) = ui.allocate_exact_size(
                                            egui::vec2(8.0, row_height),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().circle_filled(
                                            dot.center(),
                                            3.5,
                                            ui.visuals().strong_text_color(),
                                        );
                                    }
                                    ui.add(
                                        egui::Label::new(title_text)
                                            .truncate()
                                            .sense(egui::Sense::click()),
                                    )
                                })
                                .inner
                            })
                            .inner;
                        if title
                            .on_hover_text(format!("{}\nthread {thrid}", sender_line(m)))
                            .clicked()
                        {
                            opened = Some(thrid);
                        }

                        // The attachment column: a paperclip for a thread with an attachment.
                        let (clip_rect, clip) = ui.allocate_exact_size(
                            egui::vec2(ATTACH_WIDTH, row_height),
                            egui::Sense::hover(),
                        );
                        if entry.attachments {
                            paint_paperclip(ui, clip_rect);
                            clip.on_hover_text("Has attachments");
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

                        let date = egui::RichText::new(format_time(m.when())).weak();
                        ui.add_sized([DATE_WIDTH, row_height], egui::Label::new(date).truncate());
                    });
                    ui.painter().set(
                        highlight,
                        egui::Shape::rect_filled(row.response.rect, 4.0, row_band),
                    );
                }
            });
        self.window_rows = rows;

        // A shift-click checks or unchecks every thread from the last plain click to this row.
        for (row, state, shift) in box_clicks {
            if let Some(anchor) = self.check_anchor
                && shift
            {
                let (first, last) = (anchor.min(row), anchor.max(row));
                self.request_thread_range(first, last - first + 1, state);
            }
            self.check_anchor = Some(row);
        }

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

    /// Saves the given messages to the Downloads folder: one `.eml` per message, or one text
    /// file. The messages are written oldest first, and the status bar shows where they went.
    fn export_thread(&mut self, uids: &[u32], eml: bool) {
        let mut messages: Vec<&Message> = uids
            .iter()
            .filter_map(|u| self.thread_msgs.get(u))
            .collect();
        messages.sort_by_key(|m| (m.when(), m.uid));
        let items: Vec<export::Item> = messages
            .iter()
            .filter_map(|m| {
                Some(export::Item {
                    from: &m.from,
                    subject: &m.subject,
                    date: &m.date,
                    raw: m.body.as_deref()?,
                })
            })
            .collect();
        let Some(dir) = export::downloads_dir() else {
            self.status = Some("No Downloads folder was found to save into.".into());
            return;
        };
        let subject = items.first().map_or("", |i| i.subject);
        let result = if eml {
            export::write_eml(&dir, subject, &items)
        } else {
            export::write_text(&dir, subject, &items)
        };
        self.status = Some(match result {
            Ok(path) => format!("Saved to {}", path.display()),
            Err(e) => format!("Could not save: {e}"),
        });
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
                    .map_or((0i64, 0u32), |m| (m.when(), m.uid)),
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
        // Saving needs every body, so it waits until the background fetches finish.
        let exportable = !thread.is_empty()
            && thread
                .iter()
                .all(|uid| self.thread_msgs.get(uid).is_some_and(|m| m.body.is_some()));
        // Some(true) saves .eml files, Some(false) the text.
        let mut export: Option<bool> = None;
        ui.horizontal(|ui| {
            let button = ui.add_enabled(open_url.is_some(), egui::Button::new("Open in Gmail"));
            if button
                .on_disabled_hover_text("This message has no thread ID.")
                .clicked()
                && let Some(url) = open_url
            {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
            let eml = ui
                .add_enabled(exportable, egui::Button::new("Save .eml"))
                .on_disabled_hover_text("Waiting for the message bodies to load.");
            if eml.clicked() {
                export = Some(true);
            }
            let text = ui
                .add_enabled(exportable, egui::Button::new("Save text"))
                .on_disabled_hover_text("Waiting for the message bodies to load.");
            if text.clicked() {
                export = Some(false);
            }
            ui.weak(format!(
                "thread {thrid} (0x{thrid:x}) · {} messages in this conversation",
                thread.len()
            ));
        });
        if let Some(eml) = export {
            self.export_thread(&thread, eml);
        }

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

        // When every message has the same subject, it's shown once, and each message keeps
        // only its sender and time.
        let bases: BTreeSet<String> = thread
            .iter()
            .filter_map(|u| self.thread_msgs.get(u))
            .map(|m| base_subject(&m.subject))
            .collect();
        let shared_subject: Option<String> = if bases.len() == 1 {
            thread
                .first()
                .and_then(|u| self.thread_msgs.get(u))
                .map(|m| m.subject.clone())
        } else {
            None
        };
        if let Some(subject) = &shared_subject {
            ui.heading(subject);
        }

        let newest = thread.first().copied();
        let mut open_original: Option<u32> = None;
        let mut pending: Option<LinkInfo> = None;
        egui::ScrollArea::vertical()
            .id_salt("reader")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for &uid in &thread {
                    let Some(m) = self.thread_msgs.get(&uid) else {
                        continue;
                    };
                    let title = if shared_subject.is_some() {
                        format!("{}   {}", sender_line(m), format_time(m.when()))
                    } else {
                        format!(
                            "{} — {}   {}",
                            sender_line(m),
                            m.subject,
                            format_time(m.when())
                        )
                    };
                    let warnings = impersonation::sender_warnings(&m.from, &m.from_email);
                    let title = if warnings.is_empty() {
                        title
                    } else {
                        format!("{title}   [possible impersonation]")
                    };
                    // The accent colours each message's header line and its bar. Bodies keep the
                    // normal text colour, so header and body read as separate.
                    let accent = ui.visuals().hyperlink_color;
                    let title = if m.seen {
                        egui::RichText::new(title).color(accent)
                    } else {
                        egui::RichText::new(title).strong().color(accent)
                    };
                    let expanded = Some(uid) == newest;
                    let block =
                        ui.horizontal_top(|ui| {
                            ui.add_space(ACCENT_GAP);
                            ui.vertical(|ui| {
                                ui.push_id(uid, |ui| {
                                    egui::CollapsingHeader::new(title)
                                        .default_open(expanded)
                                        .show(ui, |ui| {
                                            ui.horizontal(|ui| {
                                                ui.monospace(format!(
                                                    "msgid {} (0x{:x})",
                                                    m.msgid, m.msgid
                                                ));
                                                if ui.small_button("View original").clicked() {
                                                    open_original = Some(uid);
                                                }
                                            });
                                            auth_line(ui, &m.auth);
                                            for warning in &warnings {
                                                ui.colored_label(
                                                    egui::Color32::LIGHT_RED,
                                                    format!("Possible impersonation: {warning}"),
                                                );
                                            }
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
                                                        sender_line(m),
                                                        m.subject,
                                                        m.date
                                                    ));
                                                }
                                            }
                                        });
                                });
                            });
                        });
                    // Two bars in the gutter: the sender's colour, then the accent.
                    let r = block.response.rect;
                    let h = r.height();
                    let sender_bar = egui::Rect::from_min_size(r.min, egui::vec2(ACCENT_WIDTH, h));
                    let accent_bar = sender_bar.translate(egui::vec2(ACCENT_WIDTH + 2.0, 0.0));
                    ui.painter()
                        .rect_filled(sender_bar, 2.0, sender_color(sender_key(m)));
                    ui.painter().rect_filled(accent_bar, 2.0, accent);
                    ui.interact(
                        sender_bar,
                        ui.scope_id().with(("sender", uid)),
                        egui::Sense::hover(),
                    )
                    .on_hover_text(sender_line(m));
                }
            });
        if pending.is_some() {
            self.pending_link = pending;
        }
        if let Some(uid) = open_original {
            self.original = Some(uid);
            self.request_body(uid);
        }
    }

    /// The original message, shown as Gmail's "Show original" shows it: a summary, the
    /// authentication checks, then the raw source. The source is read in full from the cache
    /// or the server, and shown up to a limit, since a very large message is slow to draw.
    fn original_window(&mut self, ctx: &egui::Context) {
        const SHOWN_CHARS: usize = 200_000;
        let Some(uid) = self.original else {
            return;
        };
        let mut open = true;
        let mut copy: Option<String> = None;
        egui::Window::new("Original message")
            .default_size([760.0, 600.0])
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                let Some(m) = self.thread_msgs.get(&uid) else {
                    ui.label("This message is no longer in the list.");
                    return;
                };
                egui::Grid::new("original_summary")
                    .num_columns(2)
                    .striped(true)
                    .show(ui, |ui| {
                        if let Some(ts) = m.received {
                            ui.strong("Received");
                            ui.label(format_time(ts));
                            ui.end_row();
                        }
                        for (name, value) in gmail_imap_proto::original::summary(&m.headers) {
                            ui.strong(name);
                            ui.label(value);
                            ui.end_row();
                        }
                        for check in &m.auth {
                            ui.strong(check.method.to_uppercase());
                            ui.label(format!("{} — {}", check.result, check.detail));
                            ui.end_row();
                        }
                    });
                ui.separator();
                match &m.body {
                    Some(raw) => {
                        let text = String::from_utf8_lossy(raw).into_owned();
                        if ui.button("Copy to clipboard").clicked() {
                            copy = Some(text.clone());
                        }
                        let total = text.chars().count();
                        let shown: String = text.chars().take(SHOWN_CHARS).collect();
                        if total > SHOWN_CHARS {
                            ui.weak(format!(
                                "Showing the first {SHOWN_CHARS} of {total} characters. Copy has all of it."
                            ));
                        }
                        egui::ScrollArea::both()
                            .id_salt("original_source")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(egui::RichText::new(shown).monospace())
                                        .wrap_mode(egui::TextWrapMode::Extend),
                                );
                            });
                    }
                    None => {
                        ui.spinner();
                        ui.label("Loading the message…");
                    }
                }
            });
        if let Some(text) = copy {
            ctx.copy_text(text);
        }
        if !open {
            self.original = None;
        }
    }

    /// The sign-in form, in its own window. It closes with the app if the user closes it, since
    /// nothing else is shown until a sign-in succeeds.
    fn login_window(&mut self, ctx: &egui::Context) {
        let builder = egui::ViewportBuilder::default()
            .with_title("Sign in")
            .with_inner_size([440.0, 330.0]);
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("sign-in"),
            builder,
            |ctx, _class| {
                egui::CentralPanel::default().show(ctx, |ui| self.login_dialog(ui));
                if ctx.input(|i| i.viewport().close_requested()) {
                    ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
                }
            },
        );
    }

    /// Records a change to the window's layout, and saves it soon after it stops changing.
    fn remember(&mut self, change: impl FnOnce(&mut settings::WindowLayout)) {
        let before = self.settings.window;
        change(&mut self.settings.window);
        if self.settings.window != before {
            self.layout_dirty = true;
        }
        if self.layout_dirty && self.layout_saved.elapsed() >= Duration::from_secs(1) {
            self.save_layout();
        }
    }

    fn save_layout(&mut self) {
        self.save_settings();
        self.layout_dirty = false;
        self.layout_saved = Instant::now();
    }

    /// Notes the window's size and position, which the system knows and the app doesn't.
    fn remember_window_position(&mut self, ctx: &egui::Context) {
        let (inner, outer) = ctx.input(|i| {
            let v = i.viewport();
            (v.inner_rect, v.outer_rect)
        });
        if let Some(rect) = inner {
            let size = [rect.width().round(), rect.height().round()];
            self.remember(|w| w.size = size);
        }
        if let Some(rect) = outer {
            let position = [rect.min.x.round(), rect.min.y.round()];
            self.remember(|w| w.position = Some(position));
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

    /// Help, from `help.txt`: a title, then `== Headings ==`, `- ` bullets and plain paragraphs.
    fn help_window(&mut self, ctx: &egui::Context) {
        if !self.help_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Help")
            .default_size([520.0, 440.0])
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("help")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for line in HELP_TEXT.lines() {
                            if let Some(heading) = line
                                .strip_prefix("== ")
                                .and_then(|rest| rest.strip_suffix(" =="))
                            {
                                ui.add_space(8.0);
                                ui.strong(heading);
                            } else if let Some(item) = line.strip_prefix("- ") {
                                ui.label(format!("\u{2022} {item}"));
                            } else if line.is_empty() {
                                ui.add_space(4.0);
                            } else {
                                ui.label(line);
                            }
                        }
                    });
            });
        if !open {
            self.help_open = false;
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

/// Saves the window layout if it changed since it was last saved. A change made just before
/// closing may not be followed by another frame, so this catches it.
impl Drop for App {
    fn drop(&mut self) {
        if self.layout_dirty {
            self.save_layout();
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.open_clicked_notifications(ui.ctx());
        self.remember_window_position(ui.ctx());
        self.refresh_storage(ui.ctx());
        self.refresh_search();

        if !self.connected {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.weak(if self.connecting {
                    "Signing in…"
                } else {
                    "Sign in to load your mail."
                });
            });
            let ctx = ui.ctx().clone();
            self.login_window(&ctx);
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

        let sidebar = egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(self.settings.window.sidebar_width)
            .min_size(120.0)
            .show(ui, |ui| self.sidebar(ui));
        let width = sidebar.response.rect.width().round();
        self.remember(|w| w.sidebar_width = width);

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));

        egui::CentralPanel::default().show(ui, |ui| {
            let list = egui::Panel::top("list")
                .resizable(true)
                .default_size(self.settings.window.list_height)
                .show(ui, |ui| {
                    if !self.search.trim().is_empty() {
                        self.search_list(ui);
                    } else if self.storage_open {
                        self.storage_list(ui);
                    } else {
                        self.message_list(ui);
                    }
                });
            let height = list.response.rect.height().round();
            self.remember(|w| w.list_height = height);
            egui::CentralPanel::default().show(ui, |ui| self.reader(ui));
        });
        let ctx = ui.ctx().clone();
        self.link_confirmation(&ctx);
        self.settings_window(&ctx);
        self.original_window(&ctx);
        self.log_window(&ctx);
        self.help_window(&ctx);
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
    fn a_sender_always_gets_the_same_colour() {
        assert_eq!(sender_slot("Alice Smith"), sender_slot("  alice smith "));
        assert_eq!(sender_color("Bob"), sender_color("bob"));
        assert!(sender_slot("Alice Smith") < SENDER_COLORS.len());
        // The hash is fixed, so a sender's colour doesn't change between runs.
        assert_eq!(sender_slot(""), (0xcbf2_9ce4_8422_2325u64 % 8) as usize);
    }

    #[test]
    fn the_sender_key_is_the_address_when_there_is_one() {
        let mut m = message_with(&[]);
        m.from = "Alice Smith".into();
        m.from_email = "alice@example.com".into();
        assert_eq!(sender_key(&m), "alice@example.com");
        // A spoofed name with a different address gets a different key.
        let mut spoof = message_with(&[]);
        spoof.from = "Alice Smith".into();
        spoof.from_email = "alice@evil.example".into();
        assert_ne!(sender_key(&m), sender_key(&spoof));
        assert_eq!(sender_line(&m), "Alice Smith <alice@example.com>");
        m.from_email = String::new();
        assert_eq!(sender_key(&m), "Alice Smith");
        assert_eq!(sender_line(&m), "Alice Smith");
    }

    #[test]
    fn senders_are_spread_over_the_palette() {
        let names = [
            "Alice", "Bob", "Carol", "Dan", "Erin", "Frank", "Grace", "Heidi", "Ivan", "Judy",
        ];
        let used: std::collections::BTreeSet<usize> =
            names.iter().map(|n| sender_slot(n)).collect();
        assert!(used.len() >= 4, "{used:?}");
    }

    #[test]
    fn reply_and_forward_prefixes_do_not_change_a_subject() {
        assert_eq!(base_subject("Re: Re: Fwd: Hello"), "hello");
        assert_eq!(base_subject("  RE:hello "), "hello");
        assert_eq!(base_subject("Hello"), base_subject("re: hello"));
        assert_ne!(base_subject("Hello"), base_subject("Hello again"));
    }

    #[test]
    fn labels_nest_under_their_parents() {
        let names = [
            "2011/Moving".to_string(),
            "2011".to_string(),
            "Work".to_string(),
        ];
        let tree = label_tree(names.iter());
        assert_eq!(tree.children.len(), 2);
        let parent = &tree.children["2011"];
        assert_eq!(
            parent.label.as_deref(),
            Some("2011"),
            "a label can be its own parent"
        );
        assert_eq!(
            parent.children["Moving"].label.as_deref(),
            Some("2011/Moving")
        );
        assert!(tree.children["Work"].children.is_empty());
    }

    #[test]
    fn times_show_local_time_without_a_zone() {
        assert_eq!(format_time(0), "");
        let shown = format_time(1_794_303_000);
        assert!(shown.contains("2026"), "{shown}");
        assert!(!shown.contains("+") && !shown.contains("UTC"), "{shown}");
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
