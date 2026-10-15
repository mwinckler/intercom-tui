use crate::{
    app::App,
    auth, format, media,
    model::*,
    platform, schedule,
    storage::{Config, Credentials},
};
use anyhow::{Context, Result, bail};
use chrono::{Datelike, Duration, Local, NaiveDate, Timelike};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers as Mod, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Inbox,
    Compose,
    Settings,
    SearchInput,
    SearchResults,
    Schedule,
    Confirm,
    Help,
    Links,
    Media,
    Attachments,
    Download,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    List,
    Thread,
}

#[derive(Clone, Debug)]
pub enum Confirm {
    Close(String, bool),
    Cancel(String),
    Discard,
    SignOut,
    Quit,
    RetryAmbiguous,
}

impl Confirm {
    fn text(&self) -> String {
        match self {
            Self::Close(_, false) => "Close this conversation?".into(),
            Self::Close(_, true) => "Reopen this conversation?".into(),
            Self::Cancel(_) => "Cancel this scheduled send? Its queued text will be removed. Enter on the scheduled card edits it instead.".into(),
            Self::Discard => "Permanently discard this draft?".into(),
            Self::SignOut => "Sign out? Polling and scheduled sends will stop. Local account data and shared app credentials are retained.".into(),
            Self::Quit => "Quit? Notifications and scheduled sends stop while the TUI is closed. Queued messages resume next launch. In-flight sends may still succeed.".into(),
            Self::RetryAmbiguous => "The previous send has no reliable confirmation. Check Intercom first to avoid a duplicate. Have you checked and want to unlock sending again?".into(),
        }
    }
}

pub struct Composer {
    pub id: String,
    pub editor: TextArea<'static>,
    pub note: bool,
    pub seen: Vec<SeenMessage>,
    pub ambiguous: bool,
    pub attachments: Vec<String>,
}

impl Composer {
    pub fn new(id: String, body: String, note: bool, seen: Vec<SeenMessage>) -> Self {
        let mut editor = textarea(&body, false);
        editor.set_wrap_mode(WrapMode::WordOrGlyph);
        editor.set_placeholder_text(
            "Write Markdown… Enter: newline · F2: send · Ctrl+S: send later · Esc: save draft",
        );
        editor.move_cursor(CursorMove::Bottom);
        editor.move_cursor(CursorMove::End);
        Self {
            id,
            editor,
            note,
            seen,
            ambiguous: false,
            attachments: vec![],
        }
    }
    pub fn body(&self) -> String {
        self.editor.lines().join("\n")
    }
    pub fn with_attachments(mut self, attachments: Vec<String>) -> Self {
        self.attachments = attachments;
        self
    }
}

pub struct Settings {
    pub fields: Vec<TextArea<'static>>,
    pub focus: usize,
    pub notifications: bool,
    pub month_first: Option<bool>,
    pub rects: Vec<(usize, Rect)>,
}

impl Settings {
    pub fn new(credentials: &Credentials, config: &Config) -> Self {
        Self {
            fields: vec![
                textarea(&credentials.client_id, false),
                textarea(&credentials.client_secret, true),
                textarea(&config.workspace, false),
                textarea("", true),
                textarea(
                    &config
                        .token_file
                        .as_ref()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    false,
                ),
                textarea(&config.refresh_seconds.to_string(), false),
            ],
            focus: 0,
            notifications: config.notifications,
            month_first: config.month_first,
            rects: vec![],
        }
    }
    pub fn value(&self, field: usize) -> String {
        self.fields[field].lines().join("")
    }
    pub fn clear_token(&mut self) {
        self.fields[3] = textarea("", true);
    }
}

#[derive(Clone, Debug)]
pub enum ThreadTarget {
    Message(String),
    Media(Media, bool),
    Draft,
    Scheduled(String),
}

pub struct ScheduleDialog {
    pub input: TextArea<'static>,
    pub picker: bool,
    pub date: NaiveDate,
    pub hour: u32,
    pub minute: u32,
    pub focus: usize,
    pub confirming: Option<i64>,
    pub error: String,
    pub cells: Vec<(NaiveDate, Rect)>,
}

impl ScheduleDialog {
    fn new() -> Self {
        Self {
            input: textarea("", false),
            picker: false,
            date: Local::now().date_naive() + Duration::days(1),
            hour: 9,
            minute: 0,
            focus: 0,
            confirming: None,
            error: String::new(),
            cells: vec![],
        }
    }
    fn parsed(&self, month_first: bool) -> Result<i64> {
        if self.picker {
            schedule::local_date(self.date, self.hour, self.minute, Local::now().timestamp())
        } else {
            schedule::parse(&self.input.lines().join(""), Local::now(), month_first)
        }
    }
}

pub struct View {
    pub mode: Mode,
    pub pane: Pane,
    pub selected: String,
    pub pinned: bool,
    pub zoom: bool,
    pub list_state: ListState,
    pub thread_scroll: u16,
    pub thread_item: Option<usize>,
    pub follow_end: bool,
    pub thread_ranges: Vec<(ThreadTarget, usize, usize)>,
    pub composer: Option<Composer>,
    pub settings: Settings,
    pub search: TextArea<'static>,
    pub search_remote: bool,
    pub search_active: bool,
    pub filter_chord: bool,
    pub schedule: ScheduleDialog,
    pub confirm: Option<Confirm>,
    pub confirm_yes: bool,
    pub before_overlay: Mode,
    pub links: Vec<String>,
    pub link_index: usize,
    pub media: Vec<(Media, bool)>,
    pub media_index: usize,
    pub file_path: TextArea<'static>,
    pub file_index: usize,
    pub file_list_focus: bool,
    pub download_media: Option<Media>,
    pub file_return_mode: Mode,
    pub list_rect: Rect,
    pub thread_rect: Rect,
    pub composer_rect: Rect,
    pub tabs: Vec<(Filter, Rect)>,
    pub search_rect: Rect,
    pub overlay_rect: Rect,
    pub help_scroll: u16,
    pub help_return_mode: Mode,
}

impl View {
    pub fn new(settings: Settings) -> Self {
        Self {
            mode: Mode::Inbox,
            pane: Pane::List,
            selected: String::new(),
            pinned: false,
            zoom: false,
            list_state: ListState::default(),
            thread_scroll: 0,
            thread_item: None,
            follow_end: true,
            thread_ranges: vec![],
            composer: None,
            settings,
            search: textarea("", false),
            search_remote: false,
            search_active: false,
            filter_chord: false,
            schedule: ScheduleDialog::new(),
            confirm: None,
            confirm_yes: false,
            before_overlay: Mode::Inbox,
            links: vec![],
            link_index: 0,
            media: vec![],
            media_index: 0,
            file_path: textarea("", false),
            file_index: 0,
            file_list_focus: false,
            download_media: None,
            file_return_mode: Mode::Inbox,
            list_rect: Rect::default(),
            thread_rect: Rect::default(),
            composer_rect: Rect::default(),
            tabs: vec![],
            search_rect: Rect::default(),
            overlay_rect: Rect::default(),
            help_scroll: 0,
            help_return_mode: Mode::Inbox,
        }
    }
    pub fn search_text(&self) -> String {
        self.search.lines().join("")
    }
    pub fn confirm(&mut self, action: Confirm) {
        self.before_overlay = self.mode;
        self.confirm = Some(action);
        self.confirm_yes = false;
        self.mode = Mode::Confirm;
    }
    fn base_mode(&self) -> Mode {
        if self.composer.is_some() {
            Mode::Compose
        } else if self.search_active {
            Mode::SearchResults
        } else {
            Mode::Inbox
        }
    }
}

pub fn textarea(value: &str, secret: bool) -> TextArea<'static> {
    let mut field = TextArea::new(value.split('\n').map(str::to_owned).collect());
    field.set_cursor_line_style(Style::default());
    field.set_selection_style(Style::default().bg(Color::Blue).fg(Color::White));
    field.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    if secret {
        field.set_mask_char('•');
    }
    field
}

fn block(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::bordered()
        .title(title)
        .border_style(Style::default().fg(if focused { ACCENT } else { DIM }))
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 45 || area.height < 14 {
        frame.render_widget(Paragraph::new("Intercom TUI\nResize to at least 45 columns × 14 rows.\nCtrl+C quits (drafts are saved).").wrap(Wrap { trim: false }), area);
        return;
    }
    let search_height = if app.view.search_active { 3 } else { 0 };
    let layout = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(search_height),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .split(area);
    let identity = if app.identity.id.is_empty() {
        if app.api.is_some() {
            "No teammate profile · grant Read admins".into()
        } else {
            "Not signed in".into()
        }
    } else {
        format!(
            "{}  {}",
            if app.identity.name.is_empty() {
                &app.identity.id
            } else {
                &app.identity.name
            },
            app.identity.email
        )
    };
    let review_count = app
        .scheduled
        .iter()
        .filter(|s| matches!(s.status, SendState::NeedsReview(_)))
        .count();
    let queue_status = if app.scheduled.is_empty() {
        String::new()
    } else {
        format!(
            " · {} scheduled / {review_count} need review",
            app.scheduled.len()
        )
    };
    let activity = if app.connecting {
        " signing in…"
    } else if app.polling || app.inbox_loading {
        " ↻ syncing"
    } else if app.sending {
        " sending…"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    if app.demo {
                        " INTERCOM · DEMO"
                    } else {
                        " INTERCOM"
                    },
                    Style::default().fg(ACCENT).bold(),
                ),
                Span::raw(format!(
                    "  {} waiting / {} open · {} unread{}{}",
                    app.cache.counts.waiting,
                    app.cache.counts.open,
                    app.cache.reads.count(),
                    queue_status,
                    activity
                )),
            ]),
            Line::styled(format!(" {identity}"), Style::default().fg(DIM)),
        ]),
        layout[0],
    );
    draw_tabs(frame, app, layout[1]);
    app.view.search_rect = layout[2];
    if app.view.search_active {
        app.view.search.set_block(block(
            format!(
                " Search · {} · F6 toggles source ",
                if app.view.search_remote {
                    "Intercom API"
                } else {
                    "local cache + archive"
                }
            ),
            app.view.mode == Mode::SearchInput,
        ));
        frame.render_widget(&app.view.search, layout[2]);
    }
    if app.view.mode == Mode::Settings
        || app.view.mode == Mode::Help && app.view.help_return_mode == Mode::Settings
    {
        draw_settings(frame, app, layout[3]);
    } else {
        draw_inbox(frame, app, layout[3]);
    }
    let hint = match app.view.mode {
        Mode::Compose => {
            " F2 send · Ctrl+O attach files · Ctrl+N note · Ctrl+S schedule · Esc save · F8 discard"
        }
        Mode::Settings => {
            " F1 setup help · Tab next · F2 save · F3 sign in · F4 sign out · F5 sync archive · F6 copy callback · Esc back"
        }
        Mode::SearchInput => " Type to search · ↓/Tab results · F6 source · Esc close search",
        Mode::SearchResults => {
            " j/k results · / query · Enter reply · F6 source · Esc close search"
        }
        Mode::Schedule => " Tab switch controls · Enter confirm · Esc back · F6 type/pick date",
        Mode::Confirm => " ←/→ choose · Enter confirm · y yes · n/Esc cancel",
        Mode::Media => {
            " j/k choose · Enter load preview · o follow link · O original · d download · Esc back"
        }
        Mode::Attachments => {
            " Enter add path · Tab file list/path · Delete remove selected file · Esc done"
        }
        Mode::Download => " Enter download · Esc cancel · Existing files are never overwritten",
        _ => {
            " j/k move · h/l panes · f filter · Enter reply · c close/reopen · / search · s settings · ? help · q quit"
        }
    };
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::styled(
                format::preview(&app.message, area.width as usize * 2),
                Style::default().fg(
                    if app.message.contains("not sent") || app.message.contains("failed") {
                        Color::Yellow
                    } else {
                        DIM
                    },
                ),
            ),
            Line::styled(hint, Style::default().fg(DIM)),
        ]))
        .wrap(Wrap { trim: false }),
        layout[4],
    );
    match app.view.mode {
        Mode::Help => draw_help(frame, app),
        Mode::Confirm => draw_confirm(frame, app),
        Mode::Schedule => draw_schedule(frame, app),
        Mode::Links => draw_links(frame, app),
        Mode::Media => draw_media(frame, app),
        Mode::Attachments | Mode::Download => draw_files(frame, app),
        _ => (),
    }
}

fn draw_tabs(frame: &mut Frame, app: &mut App, area: Rect) {
    app.view.tabs.clear();
    let mut x = area.x;
    for filter in Filter::ALL {
        let text = format!(" {} {} ", filter.label(), app.cache.counts.get(filter));
        let width = (text.len() as u16).min(area.right().saturating_sub(x));
        let rect = Rect::new(x, area.y, width, 1);
        let selected = app.config.filter == filter && !app.view.search_active;
        let style = if selected {
            Style::default().fg(Color::Black).bg(ACCENT).bold()
        } else {
            Style::default().fg(DIM)
        };
        frame.render_widget(Paragraph::new(text).style(style), rect);
        app.view.tabs.push((filter, rect));
        x += width;
    }
    if app.view.filter_chord {
        frame.render_widget(
            Paragraph::new(" f → o open / w waiting / c closed / s snoozed / a all ")
                .style(Style::default().bg(Color::Blue).fg(Color::White)),
            area,
        );
    }
}

fn draw_inbox(frame: &mut Frame, app: &mut App, area: Rect) {
    let only_thread = app.view.zoom
        || app.view.composer.is_some() && area.width < 90
        || (area.width < 75 && app.view.pane == Pane::Thread);
    let only_list = !only_thread && area.width < 75;
    let split =
        Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(64)]).split(area);
    let list_area = if only_list {
        area
    } else if only_thread {
        Rect::default()
    } else {
        split[0]
    };
    let thread_area = if only_thread {
        area
    } else if only_list {
        Rect::default()
    } else {
        split[1]
    };
    app.view.list_rect = list_area;
    app.view.thread_rect = thread_area;
    if list_area.width > 0 {
        let rows = app.rows();
        let selected = rows.iter().position(|c| c.id == app.view.selected);
        let items: Vec<_> = rows
            .iter()
            .map(|c| {
                let unread = app.cache.reads.unread(&c.id);
                let mut badges = String::new();
                if app.drafts.contains_key(&c.id) {
                    badges.push_str(" DRAFT");
                }
                if app.scheduled.iter().any(|s| {
                    s.conversation_id == c.id && matches!(s.status, SendState::NeedsReview(_))
                }) {
                    badges.push_str(" REVIEW");
                } else if app.scheduled.iter().any(|s| s.conversation_id == c.id) {
                    badges.push_str(" LATER");
                }
                let age = format::relative(
                    if c.waiting_since > 0 {
                        c.waiting_since
                    } else {
                        c.updated_at
                    },
                    Local::now().timestamp(),
                );
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            if unread { "● " } else { "  " },
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::styled(c.title.clone(), Style::default().bold()),
                    ]),
                    Line::styled(
                        format!("  {} · {age}{badges}", c.from),
                        Style::default().fg(if c.waiting_since > 0 {
                            Color::Yellow
                        } else {
                            DIM
                        }),
                    ),
                    Line::styled(
                        format!("  {}", format::preview(&c.snippet, 120)),
                        Style::default().fg(DIM),
                    ),
                    Line::default(),
                ])
            })
            .collect();
        app.view.list_state.select(selected);
        let title = if app.view.search_active {
            format!(" Search results ({}) ", items.len())
        } else {
            format!(" {} ({}) ", app.config.filter.label(), items.len())
        };
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(if app.polling || app.inbox_loading || app.searching {
                    "Loading…"
                } else {
                    "No conversations matched."
                })
                .block(block(title, app.view.pane == Pane::List)),
                list_area,
            );
        } else {
            frame.render_stateful_widget(
                List::new(items)
                    .block(block(title, app.view.pane == Pane::List))
                    .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White)),
                list_area,
                &mut app.view.list_state,
            );
        }
    }
    if thread_area.width > 0 {
        draw_thread(frame, app, thread_area);
    }
}

fn draw_thread(frame: &mut Frame, app: &mut App, area: Rect) {
    let compose_height = if app.view.composer.is_some() {
        (area.height / 3).clamp(3, 9)
    } else {
        0
    };
    let split =
        Layout::vertical([Constraint::Min(1), Constraint::Length(compose_height)]).split(area);
    app.view.composer_rect = split[1];
    app.view.thread_rect = split[0];
    let conversation = app.selected().cloned();
    let title = conversation
        .as_ref()
        .map(|c| format!(" {} · {:?} ", c.title, c.state))
        .unwrap_or_else(|| {
            if app.view.selected.is_empty() {
                " Reader ".into()
            } else {
                format!(" Conversation {} ", app.view.selected)
            }
        });
    let border = block(
        title,
        app.view.pane == Pane::Thread || app.view.composer.is_some(),
    );
    let inner = border.inner(split[0]);
    frame.render_widget(border, split[0]);
    let mut segments: Vec<(ThreadTarget, Vec<Line<'static>>)> = vec![];
    if let Some(conversation) = &conversation {
        for message in &conversation.messages {
            let unread = app
                .cache
                .reads
                .message_unread(&conversation.id, &message.id);
            let kind = match message.kind {
                Kind::Customer => "CUSTOMER",
                Kind::Teammate => "TEAMMATE",
                Kind::Note => "PRIVATE NOTE",
                Kind::Bot => "BOT",
            };
            let color = match message.kind {
                Kind::Note => Color::Yellow,
                Kind::Teammate => ACCENT,
                Kind::Bot => DIM,
                _ => Color::Reset,
            };
            let mut lines = vec![Line::styled(
                format!(
                    "{}{} · {kind} · {}",
                    if unread { "● " } else { "" },
                    message.from,
                    format::relative(message.created_at, Local::now().timestamp())
                ),
                Style::default().fg(color).bold(),
            )];
            if !message.email.is_empty() && !message.from.eq_ignore_ascii_case(&message.email) {
                lines.push(Line::styled(
                    message.email.clone(),
                    Style::default().fg(DIM),
                ));
            }
            if message.blocks.is_empty() {
                lines.extend(format::rich(&message.body));
            } else {
                for content in &message.blocks {
                    match content {
                        ContentBlock::Text { html } => lines.extend(format::rich(html)),
                        ContentBlock::Image(media) | ContentBlock::Attachment(media) => {
                            if !lines.is_empty() {
                                segments.push((
                                    ThreadTarget::Message(message.id.clone()),
                                    std::mem::take(&mut lines),
                                ));
                            }
                            let image = matches!(content, ContentBlock::Image(_));
                            segments.push((
                                ThreadTarget::Media(media.clone(), image),
                                media_lines(app, media, image, inner.width),
                            ));
                        }
                    }
                }
            }
            lines.push(Line::default());
            segments.push((ThreadTarget::Message(message.id.clone()), lines));
        }
        let draft = if let Some(composer) = &app.view.composer {
            Some(Draft {
                body: composer.body(),
                note: composer.note,
                attachments: composer.attachments.clone(),
            })
        } else {
            app.drafts.get(&conversation.id).cloned()
        };
        if let Some(draft) = draft.filter(|d| !d.is_empty()) {
            let mut lines = vec![Line::styled(
                format!(
                    "┄ {} · {} ┄",
                    if draft.note { "PRIVATE NOTE" } else { "REPLY" },
                    if app.view.composer.is_some() {
                        "PREVIEW"
                    } else {
                        "DRAFT · Enter to edit / F8 discard"
                    }
                ),
                Style::default().fg(Color::Magenta).bold(),
            )];
            lines.extend(format::rich(&format::markdown(&draft.body)));
            lines.extend(attachment_lines(&draft.attachments));
            lines.push(Line::default());
            segments.push((ThreadTarget::Draft, lines));
        }
        for scheduled in app
            .scheduled
            .iter()
            .filter(|s| s.conversation_id == conversation.id)
        {
            let mut status = match &scheduled.status {
                SendState::Pending => format!("LATER · {}", schedule::label(scheduled.send_at)),
                SendState::Sending => "SENDING…".into(),
                SendState::NeedsReview(error) => format!("NEEDS REVIEW · {error}"),
            };
            status.push_str(if scheduled.note {
                " · PRIVATE NOTE"
            } else {
                " · PUBLIC REPLY"
            });
            let mut lines = vec![Line::styled(
                status,
                Style::default().fg(Color::Yellow).bold(),
            )];
            lines.extend(format::rich(&format::markdown(&scheduled.body)));
            lines.extend(attachment_lines(&scheduled.attachments));
            lines.push(Line::styled(
                "Enter edit/reschedule · x cancel",
                Style::default().fg(DIM),
            ));
            lines.push(Line::default());
            segments.push((ThreadTarget::Scheduled(scheduled.id.clone()), lines));
        }
    }
    app.view.thread_ranges.clear();
    let mut lines = vec![];
    let mut height = 0;
    for (index, (target, mut segment)) in segments.into_iter().enumerate() {
        let selected = app.view.thread_item == Some(index)
            && app.view.pane == Pane::Thread
            && app.view.composer.is_none();
        if selected {
            for line in &mut segment {
                line.style = line.style.bg(Color::DarkGray);
            }
        }
        let count = Paragraph::new(segment.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner.width.max(1));
        app.view
            .thread_ranges
            .push((target, height, height + count));
        height += count;
        lines.extend(segment);
    }
    if app.thread_loading.contains(&app.view.selected) {
        lines.push(Line::styled("Refreshing thread…", Style::default().fg(DIM)));
    }
    if conversation.as_ref().is_some_and(|c| !c.complete) {
        lines.push(Line::styled(
            "Thread may be incomplete; refresh before replying.",
            Style::default().fg(Color::Yellow),
        ));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "Select a conversation. Enter replies; h/l switches panes.",
            Style::default().fg(DIM),
        ));
    }
    let total = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner.width.max(1));
    if app.view.follow_end {
        app.view.thread_scroll = total
            .saturating_sub(inner.height as usize)
            .min(u16::MAX as usize) as u16;
    } else if let Some((_, start, end)) = app
        .view
        .thread_item
        .and_then(|i| app.view.thread_ranges.get(i))
    {
        if *start < app.view.thread_scroll as usize {
            app.view.thread_scroll = (*start).min(u16::MAX as usize) as u16;
        } else if *end > app.view.thread_scroll as usize + inner.height as usize {
            app.view.thread_scroll = end
                .saturating_sub(inner.height as usize)
                .min(*start)
                .min(u16::MAX as usize) as u16;
        }
    }
    app.view.thread_scroll = app.view.thread_scroll.min(
        total
            .saturating_sub(inner.height as usize)
            .min(u16::MAX as usize) as u16,
    );
    // Fetch only images near the visible reader, never an entire email history.
    let scroll = app.view.thread_scroll as usize;
    if !matches!(app.view.mode, Mode::Settings | Mode::Help | Mode::Confirm) {
        let nearby: Vec<_> = app
            .view
            .thread_ranges
            .iter()
            .filter_map(|(target, start, end)| {
                if let ThreadTarget::Media(media, true) = target
                    && *end >= scroll.saturating_sub(8)
                    && *start <= scroll + inner.height as usize + 8
                {
                    Some(media.clone())
                } else {
                    None
                }
            })
            .collect();
        for media in nearby {
            app.request_image(&media, false);
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.view.thread_scroll, 0)),
        inner,
    );
    if let Some(composer) = &mut app.view.composer {
        let title = format!(
            " {} as {} · {} files · Ctrl+O attach · F2 send ",
            if composer.note {
                "Private note"
            } else {
                "Public reply"
            },
            app.identity.name,
            composer.attachments.len()
        );
        composer
            .editor
            .set_block(block(title, app.view.mode == Mode::Compose));
        frame.render_widget(&composer.editor, split[1]);
    }
}

fn attachment_lines(paths: &[String]) -> Vec<Line<'static>> {
    paths
        .iter()
        .map(|path| {
            Line::styled(
                format!(
                    "Attachment: {}",
                    format::safe_text(path).replace(['\n', '\t'], " ")
                ),
                Style::default().fg(Color::Magenta),
            )
        })
        .collect()
}

fn media_lines(app: &App, media: &Media, image: bool, width: u16) -> Vec<Line<'static>> {
    let details = [
        format::safe_text(&media.content_type),
        media::size(media.size),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    let mut lines = vec![Line::styled(
        format!(
            "{}: {}{}",
            if image { "Image" } else { "Attachment" },
            media.label(),
            if details.is_empty() {
                String::new()
            } else {
                format!(" · {details}")
            }
        ),
        Style::default().fg(ACCENT),
    )];
    if image {
        match app.images.entries.get(&media.url) {
            Some(media::Preview::Ready(pixels)) => {
                lines.extend(media::image_lines(pixels, width.min(80), 8))
            }
            Some(media::Preview::Loading) => {
                lines.push(Line::styled("Loading image…", Style::default().fg(DIM)))
            }
            Some(media::Preview::Failed(error)) => lines.push(Line::styled(
                error.clone(),
                Style::default().fg(Color::Yellow),
            )),
            None => lines.push(Line::styled(
                if media::automatic(media) {
                    "Preview loads near the viewport"
                } else {
                    "External / tracking image · Enter to load (makes a network request)"
                },
                Style::default().fg(DIM),
            )),
        }
    }
    lines.push(Line::styled(
        if image {
            "Enter load/retry · o follow link · O original · d save · a all media"
        } else {
            "d save · o open in browser · a all media"
        },
        Style::default().fg(DIM),
    ));
    lines.push(Line::default());
    lines
}

fn draw_settings(frame: &mut Frame, app: &mut App, area: Rect) {
    app.view.list_rect = Rect::default();
    app.view.thread_rect = Rect::default();
    app.view.composer_rect = Rect::default();
    let outer = block(" Settings · F1 setup help ", true);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    let fields_height = if inner.height >= 25 { 3 } else { 2 };
    let mut y = inner.y;
    app.view.settings.rects.clear();
    let labels = [
        "Shared app Client ID",
        "Shared app Client secret (masked)",
        "Workspace ID or inbox URL",
        "Advanced: paste access token (masked)",
        "Access token file (optional)",
        "Poll interval in seconds (15–3600)",
    ];
    // Scroll the settings fields on very short terminals, keeping the focused input reachable.
    let available = inner.height.saturating_sub(5) as usize / fields_height as usize;
    let first = if app.view.settings.focus < 6 {
        app.view
            .settings
            .focus
            .saturating_sub(available.saturating_sub(1))
    } else {
        6usize.saturating_sub(available)
    };
    for (index, label) in labels.iter().enumerate().skip(first) {
        if y + fields_height > inner.bottom().saturating_sub(4) {
            break;
        }
        let rect = Rect::new(inner.x, y, inner.width, fields_height);
        let focused = app.view.settings.focus == index;
        let border = if fields_height == 2 {
            Block::default()
                .borders(Borders::TOP)
                .title(*label)
                .border_style(Style::default().fg(if focused { ACCENT } else { DIM }))
        } else {
            block(*label, focused)
        };
        app.view.settings.fields[index].set_block(border);
        frame.render_widget(&app.view.settings.fields[index], rect);
        app.view.settings.rects.push((index, rect));
        y += fields_height;
    }
    let options = format!(
        " [{}] Notifications    Date order: {} ",
        if app.view.settings.notifications {
            "x"
        } else {
            " "
        },
        match app.view.settings.month_first {
            None => "system locale",
            Some(true) => "month/day",
            Some(false) => "day/month",
        }
    );
    if y < inner.bottom() {
        frame.render_widget(
            Paragraph::new(options).style(Style::default().fg(
                if [6, 7].contains(&app.view.settings.focus) {
                    ACCENT
                } else {
                    DIM
                },
            )),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }
    let button_labels = [
        (8, "Save F2"),
        (9, "Sign in F3"),
        (10, "Sign out F4"),
        (11, "Sync all F5"),
        (12, "Back"),
    ];
    let mut x = inner.x;
    for (index, label) in button_labels {
        let width = (label.len() as u16 + 4).min(inner.right().saturating_sub(x));
        let rect = Rect::new(x, y, width, 1);
        frame.render_widget(
            Paragraph::new(format!(" [ {label} ] ")).style(
                Style::default()
                    .fg(if app.view.settings.focus == index {
                        Color::Black
                    } else {
                        ACCENT
                    })
                    .bg(if app.view.settings.focus == index {
                        ACCENT
                    } else {
                        Color::Reset
                    }),
            ),
            rect,
        );
        app.view.settings.rects.push((index, rect));
        x += width;
    }
    y += 1;
    if y < inner.bottom() {
        let text = if app.auth_url.is_empty() {
            format!(
                "OAuth redirect: {} · F6 copies it\nConfig: {}\nData: {}",
                auth::REDIRECT,
                app.store.config_dir.display(),
                app.store.data_dir.display()
            )
        } else {
            format!("Authorize in browser (F7 copies URL): {}", app.auth_url)
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(DIM))
                .wrap(Wrap { trim: false }),
            Rect::new(inner.x, y, inner.width, inner.bottom() - y),
        );
    }
}

fn popup(frame: &mut Frame, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    rect
}

fn draw_confirm(frame: &mut Frame, app: &mut App) {
    let area = popup(frame, 70, 9);
    app.view.overlay_rect = area;
    let text = app
        .view
        .confirm
        .as_ref()
        .map(Confirm::text)
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(text),
            Line::default(),
            Line::styled(
                if app.view.confirm_yes {
                    "  No     [ Yes ]"
                } else {
                    "[ No ]     Yes"
                },
                Style::default().fg(ACCENT).bold(),
            ),
            Line::raw("y / n · arrows + Enter · Esc cancels"),
        ])
        .wrap(Wrap { trim: false })
        .block(block(" Confirm ", true)),
        area,
    );
}

fn draw_schedule(frame: &mut Frame, app: &mut App) {
    let height = if app.view.schedule.picker { 19 } else { 11 };
    let area = popup(frame, 62, height);
    app.view.overlay_rect = area;
    let border = block(" Send later · only while the TUI is running ", true);
    let inner = border.inner(area);
    frame.render_widget(border, area);
    let month_first = app
        .config
        .month_first
        .unwrap_or_else(schedule::system_month_first);
    let parsed = app.view.schedule.parsed(month_first);
    let dialog = &mut app.view.schedule;
    if let Some(at) = dialog.confirming {
        frame.render_widget(
            Paragraph::new(format!(
                "Schedule this {} for {}?\n\nEnter confirms · Esc returns to date selection",
                if app.view.composer.as_ref().is_some_and(|c| c.note) {
                    "private note"
                } else {
                    "reply"
                },
                schedule::label(at)
            ))
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    frame.render_widget(
        Paragraph::new(if dialog.picker {
            " Type a time   [ Pick a date ] · F6 switches"
        } else {
            "[ Type a time ]   Pick a date · F6 switches"
        })
        .style(Style::default().fg(ACCENT)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    if dialog.picker {
        let month = dialog.date.with_day(1).unwrap();
        let start = month - Duration::days(month.weekday().num_days_from_monday() as i64);
        frame.render_widget(
            Paragraph::new(format!("‹ {} ›   PgUp/PgDn month", month.format("%B %Y"))).centered(),
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(" Mon    Tue    Wed    Thu    Fri    Sat    Sun")
                .style(Style::default().fg(DIM)),
            Rect::new(inner.x + 1, inner.y + 3, inner.width.saturating_sub(2), 1),
        );
        dialog.cells.clear();
        let cell_width = inner.width / 7;
        for day in 0..42 {
            let date = start + Duration::days(day);
            let rect = Rect::new(
                inner.x + (day % 7) as u16 * cell_width,
                inner.y + 4 + (day / 7) as u16,
                cell_width,
                1,
            );
            let style = if date == dialog.date {
                Style::default().fg(Color::Black).bg(ACCENT).bold()
            } else {
                Style::default().fg(if date.month() == month.month() {
                    Color::Reset
                } else {
                    DIM
                })
            };
            if rect.y < inner.bottom() {
                frame.render_widget(
                    Paragraph::new(date.day().to_string())
                        .centered()
                        .style(style),
                    rect,
                );
                dialog.cells.push((date, rect));
            }
        }
        if inner.height > 12 {
            frame.render_widget(
                Paragraph::new(format!(
                    " Time: {}{:02}{} : {}{:02}{}   Tab selects hour/minute",
                    if dialog.focus == 1 { "[" } else { " " },
                    dialog.hour,
                    if dialog.focus == 1 { "]" } else { " " },
                    if dialog.focus == 2 { "[" } else { " " },
                    dialog.minute,
                    if dialog.focus == 2 { "]" } else { " " }
                ))
                .style(Style::default().fg(ACCENT)),
                Rect::new(inner.x, inner.y + 11, inner.width, 1),
            );
        }
    } else {
        dialog.input.set_block(block(
            " e.g. tomorrow at 8 · Monday · in 2h · 2027-04-07 09:30 ",
            true,
        ));
        frame.render_widget(
            &dialog.input,
            Rect::new(inner.x, inner.y + 2, inner.width, 3),
        );
    }
    let status = if !dialog.error.is_empty() {
        dialog.error.clone()
    } else {
        match parsed {
            Ok(at) => format!("Will send {}", schedule::label(at)),
            Err(e) => e.to_string(),
        }
    };
    frame.render_widget(
        Paragraph::new(format!("{status}\nEnter reviews time · Esc cancels"))
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: false }),
        Rect::new(
            inner.x,
            inner.bottom().saturating_sub(3),
            inner.width,
            3.min(inner.height),
        ),
    );
}

const HELP: &str = "INBOX & READER\nj/k or arrows   Move conversations / select message cards\nh/l or Left/Right   Switch list and thread panes\nTab/Shift+Tab   Cycle inbox filters\nf then o/w/c/s/a   Open / Waiting / Closed / Snoozed / All\ne   Toggle split and zoomed reader\nCtrl+U / Ctrl+D or PgUp/PgDn   Scroll half/page\ng / G   First / last conversation or thread start/end\nEnter   Reply, reopen draft, or edit selected scheduled send\nc   Close or reopen selected conversation (confirmation)\no   Open selected conversation in browser\nO   Open inbox home in browser\nL   Select and open a link from the thread\nr   Refresh now\ns or Ctrl+,   Settings\ny   Download full searchable archive\nq   Quit (no background worker)\n\nCOMPOSER\nEnter   Newline\nF2 / Ctrl+Enter / Alt+Enter   Send as signed-in teammate\nCtrl+N   Toggle public reply / internal note\nCtrl+S   Schedule send (typed date or calendar)\nEsc   Save draft and return\nF8 / Ctrl+Shift+D   Confirm discarding draft\nCtrl+W   Delete previous word\nCtrl+U   Delete to start of current line\nShift+arrows   Select text (requires terminal support)\nCtrl+A / Ctrl+C / Ctrl+X / Ctrl+V   Select all / copy / cut / paste\nCtrl+Z / Ctrl+Y   Undo / redo\nMouse drag   Select composer text\nShift+mouse   Terminal's own selection when supported\n\nSCHEDULED MESSAGES\nFocus thread with l, j/k selects cards\nEnter on scheduled card   Edit or reschedule (returns to draft)\nx on scheduled card   Confirm cancelling\nNeeds-review sends never retry automatically\nKeep the TUI open; due sends resume next launch\nNew comments/notes block a scheduled send until reviewed\n\nSEARCH\n/   Search local cached conversations and archived message bodies\nF6 / Ctrl+I   Toggle Intercom API / local search\nDown / Tab   Leave search input for result navigation\nUp on first result or /   Return to search input\ni   Toggle source while browsing results\nEsc   Close search and return to current inbox\nRemote search uses Intercom's source-message search semantics\nLocal search matches every word anywhere in the thread\n\nSETTINGS\nTab / Shift+Tab   Move fields and buttons\nF2   Save (advanced token field also signs in)\nF3 / Ctrl+L   Sign in with shared OAuth app credentials\nF4   Sign out (only app-owned tokens are deleted)\nF5 / Ctrl+Y   Sync every conversation to local archive\nF6   Copy OAuth callback URL\nF7   Copy authorization URL if browser did not open\nSpace on options   Toggle notifications / date order\n\nNOTIFICATIONS & READ STATUS\nNative toasts show new customer messages, including follow-ups\nInitial loading and archive sync do not flood notifications\nSelecting a thread in a focused TUI marks messages read locally\nOnly Open conversations can remain locally unread\nYour own replies and notes never create unread alerts\nClosed/snoozed conversations are always locally read\nmacOS: bundled build enables modern click-through notifications\n\nPress Esc or ? to close help. j/k or PgUp/PgDn scroll.";

const MEDIA_HELP: &str = "IMAGES & ATTACHMENTS
a   List images and attached files in the selected thread
Focus reader with l, then j/k to select an image or file card
Enter   Load/retry the selected image, or save the selected file
o / O   Follow the selected media's link / open its original in a browser
d   Save selected image/file (or open media list if none is selected)
Downloads never use your API token, open saved files, or overwrite files
Ctrl+O in composer   Add/remove attachments with a file-path dialog
Enter adds a path; Tab selects attached files; Delete removes; Esc returns
Replies/notes can be file-only: up to 10 files, 20 MiB combined
Drafts and schedules keep file paths; files are read only at send time
Keep selected files in place and unchanged until sending
Intercom HTTPS images preview near the viewport; external/tracking images need Enter
Previews are static Unicode color blocks; original files remain available
Refresh (r) if a media URL expires; archive sync upgrades old text-only records";

fn setup_help() -> String {
    format!(
        "SETUP (recommended: personal OAuth)
1. Developer Hub: https://app.intercom.com/a/developers
   Create/select your team's app; ask a workspace admin if needed.
2. Configure > Authentication: enable Use OAuth and allow:
   Read conversations, Write conversations, Read admins.
   Add this exact redirect URL (F6 copies it in Settings):
   {}
3. Basic Information: copy Client ID and Client secret into Settings.
   If a teammate already set up the app, ask them for these values.
4. Press F3 in Settings and authorize as yourself in the browser.
   The TUI obtains/saves your token; leave the advanced token field empty.
   Browser didn't open? F7 in Settings copies the authorization URL.

EXISTING TOKEN (advanced)
Paste an existing personal OAuth token in the advanced token field; F2 saves.
External token files / INTERCOM_ACCESS_TOKEN override the saved token;
clear them before OAuth sign-in. Avoid shared workspace tokens.
Keep secrets private; saved locally in plaintext, not a keychain.
Workspace is normally automatic; paste an inbox URL only if links fail.",
        auth::REDIRECT
    )
}

fn open_help(app: &mut App) {
    app.view.help_return_mode = app.view.mode;
    app.view.help_scroll = 0;
    app.view.mode = Mode::Help;
}

fn draw_help(frame: &mut Frame, app: &mut App) {
    let area = popup(frame, 94, frame.area().height.saturating_sub(2));
    let settings = app.view.help_return_mode == Mode::Settings;
    let text = if settings {
        setup_help()
    } else {
        format!("{}\n\n{HELP}\n\n{MEDIA_HELP}", setup_help())
    };
    let outer = block(
        if settings {
            " Setup help · j/k/PgUp/PgDn scroll · Esc back "
        } else {
            " Help · j/k/PgUp/PgDn scroll · Esc back "
        },
        true,
    );
    let inner = outer.inner(area);
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(inner.width.max(1))
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    app.view.help_scroll = app.view.help_scroll.min(max_scroll);
    frame.render_widget(
        paragraph.block(outer).scroll((app.view.help_scroll, 0)),
        area,
    );
}

fn open_media(app: &mut App) -> Result<()> {
    let conversation = app.selected().context("Select a conversation first")?;
    app.view.media = conversation
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            ContentBlock::Image(media) => Some((media.clone(), true)),
            ContentBlock::Attachment(media) => Some((media.clone(), false)),
            _ => None,
        })
        .collect();
    if app.view.media.is_empty() {
        bail!("No images or attachments. Refresh to upgrade an older cached thread.");
    }
    app.view.media_index = 0;
    app.view.mode = Mode::Media;
    Ok(())
}

fn begin_download(app: &mut App, media: Media) {
    let base = directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(std::path::Path::to_owned))
        .filter(|path| path.is_dir())
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_owned()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    app.view.file_path = textarea(
        &base.join(media::download_name(&media)).to_string_lossy(),
        false,
    );
    app.view.file_path.move_cursor(CursorMove::End);
    app.view.download_media = Some(media);
    app.view.file_return_mode = app.view.mode;
    app.view.mode = Mode::Download;
}

fn draw_media(frame: &mut Frame, app: &mut App) {
    let area = popup(frame, 90, 18);
    let items = app
        .view
        .media
        .iter()
        .map(|(media, image)| {
            ListItem::new(format!(
                "{}: {} · {} {}",
                if *image { "Image" } else { "File" },
                media.label(),
                format::safe_text(&media.content_type),
                media::size(media.size)
            ))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(app.view.media_index));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(Style::default().fg(Color::Black).bg(ACCENT))
            .block(block(
                " Media · Enter preview · o link · O original · d save · Esc back ",
                true,
            )),
        area,
        &mut state,
    );
}

fn draw_files(frame: &mut Frame, app: &mut App) {
    let area = popup(frame, 90, 18);
    let downloading = app.view.mode == Mode::Download;
    let border = block(
        if downloading {
            " Save attachment · no automatic opening or overwriting "
        } else {
            " Attach local files · up to 10 files / 20 MiB total "
        },
        true,
    );
    let inner = border.inner(area);
    frame.render_widget(border, area);
    let split = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(inner);
    frame.render_widget(Paragraph::new(if downloading { "Choose a destination. Enter saves; Esc cancels." } else { "Enter a file path (~/, relative paths, and file:// supported).\nEnter adds · Tab selects file list · Delete removes · Esc done" }).wrap(Wrap { trim: false }), split[0]);
    if !downloading {
        let items = app
            .view
            .composer
            .as_ref()
            .map(|composer| {
                composer
                    .attachments
                    .iter()
                    .map(|path| ListItem::new(format::safe_text(path).replace(['\n', '\t'], " ")))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut state = ListState::default().with_selected(if app.view.file_list_focus {
            Some(app.view.file_index)
        } else {
            None
        });
        frame.render_stateful_widget(
            List::new(items).highlight_style(Style::default().fg(Color::Black).bg(ACCENT)),
            split[1],
            &mut state,
        );
    } else if let Some(media) = &app.view.download_media {
        frame.render_widget(Paragraph::new(media.label()), split[1]);
    }
    app.view.file_path.set_block(block(
        if downloading {
            " Destination path "
        } else {
            " File path "
        },
        downloading || !app.view.file_list_focus,
    ));
    frame.render_widget(&app.view.file_path, split[2]);
}

fn file_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let downloading = app.view.mode == Mode::Download;
    if key.code == KeyCode::Esc {
        app.view.mode = if downloading {
            app.view.file_return_mode
        } else {
            app.view.base_mode()
        };
        app.view.download_media = None;
        return Ok(());
    }
    if !downloading && key.code == KeyCode::Tab {
        app.view.file_list_focus = !app.view.file_list_focus;
        return Ok(());
    }
    if !downloading && app.view.file_list_focus {
        if let Some(composer) = &mut app.view.composer {
            let len = composer.attachments.len();
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    app.view.file_index = (app.view.file_index + 1).min(len.saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.view.file_index = app.view.file_index.saturating_sub(1)
                }
                KeyCode::Delete | KeyCode::Backspace if app.view.file_index < len => {
                    composer.attachments.remove(app.view.file_index);
                    app.view.file_index = app.view.file_index.min(len.saturating_sub(2));
                    app.save_composer()?;
                }
                _ => (),
            }
        }
        return Ok(());
    }
    if key.code == KeyCode::Enter {
        let path = app.view.file_path.lines().join("");
        if downloading {
            let media = app
                .view
                .download_media
                .clone()
                .context("Select an attachment first")?;
            app.download_attachment(&media, &path)?;
            app.view.mode = app.view.file_return_mode;
            app.view.download_media = None;
        } else {
            app.add_attachment(&path)?;
            app.view.file_path = textarea("", false);
            app.view.follow_end = true;
        }
    } else {
        editor_input(&mut app.view.file_path, key, true)?;
    }
    Ok(())
}

fn draw_links(frame: &mut Frame, app: &mut App) {
    let area = popup(frame, 90, 16);
    let items: Vec<_> = app
        .view
        .links
        .iter()
        .enumerate()
        .map(|(i, url)| {
            ListItem::new(url.clone()).style(if i == app.view.link_index {
                Style::default().fg(Color::Black).bg(ACCENT)
            } else {
                Style::default()
            })
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.view.link_index));
    frame.render_stateful_widget(
        List::new(items).block(block(
            " Open link · j/k choose · Enter open · Esc back ",
            true,
        )),
        area,
        &mut state,
    );
}

pub fn input(app: &mut App, event: Event) -> Result<()> {
    match event {
        Event::FocusGained => {
            app.focused = true;
            app.mark_viewed();
        }
        Event::FocusLost => app.focused = false,
        Event::Paste(text) => {
            let text = format::safe_text(&text);
            match app.view.mode {
                Mode::Compose if !app.sending => {
                    if let Some(c) = &mut app.view.composer {
                        c.editor.insert_str(text);
                    }
                }
                Mode::SearchInput => {
                    app.view.search.insert_str(text.replace(['\n', '\r'], " "));
                    app.search_changed();
                }
                Mode::Settings if app.view.settings.focus < 6 => {
                    app.view.settings.fields[app.view.settings.focus]
                        .insert_str(text.replace(['\n', '\r'], ""));
                }
                Mode::Schedule if !app.view.schedule.picker => {
                    app.view
                        .schedule
                        .input
                        .insert_str(text.replace(['\n', '\r'], " "));
                }
                Mode::Attachments | Mode::Download => {
                    app.view
                        .file_path
                        .insert_str(text.replace(['\n', '\r'], ""));
                }
                _ => (),
            }
        }
        Event::Mouse(mouse) => mouse_input(app, mouse)?,
        Event::Key(key) if key.kind != KeyEventKind::Release => key_input(app, key)?,
        _ => (),
    }
    Ok(())
}

fn key_input(app: &mut App, key: KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(Mod::CONTROL);
    let alt = key.modifiers.contains(Mod::ALT);
    if ctrl
        && key.code == KeyCode::Char('c')
        && app.view.mode != Mode::Compose
        && app.view.mode != Mode::Settings
    {
        return request_quit(app);
    }
    match app.view.mode {
        Mode::Attachments | Mode::Download => return file_key(app, key),
        Mode::Media => {
            match key.code {
                KeyCode::Esc => app.view.mode = app.view.base_mode(),
                KeyCode::Down | KeyCode::Char('j') => {
                    app.view.media_index =
                        (app.view.media_index + 1).min(app.view.media.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.view.media_index = app.view.media_index.saturating_sub(1)
                }
                KeyCode::Enter | KeyCode::Char('o' | 'O' | 'd') => {
                    if let Some((media, image)) = app.view.media.get(app.view.media_index).cloned()
                    {
                        match key.code {
                            KeyCode::Char('d') => begin_download(app, media),
                            KeyCode::Char('O') => platform::open_url(&media.url)?,
                            KeyCode::Char('o') => {
                                platform::open_url(if format::safe_url(&media.link_url) {
                                    &media.link_url
                                } else {
                                    &media.url
                                })?
                            }
                            _ if image => {
                                app.request_image(&media, true);
                                app.view.thread_item = app.view.thread_ranges.iter().position(|(target, _, _)| matches!(target, ThreadTarget::Media(m, _) if m.url == media.url));
                                app.view.follow_end = false;
                                app.view.pane = Pane::Thread;
                                app.view.mode = app.view.base_mode();
                            }
                            _ => begin_download(app, media),
                        }
                    }
                }
                _ => (),
            }
            return Ok(());
        }
        Mode::Help => {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::F(1) => {
                    app.view.mode = app.view.help_return_mode
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    app.view.help_scroll = app.view.help_scroll.saturating_add(1)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.view.help_scroll = app.view.help_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => app.view.help_scroll = app.view.help_scroll.saturating_add(15),
                KeyCode::PageUp => app.view.help_scroll = app.view.help_scroll.saturating_sub(15),
                _ => (),
            }
            return Ok(());
        }
        Mode::Confirm => {
            match key.code {
                KeyCode::Esc | KeyCode::Char('n') => {
                    app.view.confirm = None;
                    app.view.mode = app.view.before_overlay;
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    app.view.confirm_yes = !app.view.confirm_yes
                }
                KeyCode::Enter | KeyCode::Char('y') => {
                    if app.view.confirm_yes || key.code == KeyCode::Char('y') {
                        if let Some(action) = app.view.confirm.take() {
                            app.view.mode = app.view.before_overlay;
                            app.confirm(action)?;
                        }
                    } else {
                        app.view.confirm = None;
                        app.view.mode = app.view.before_overlay;
                    }
                }
                _ => (),
            }
            return Ok(());
        }
        Mode::Schedule => return schedule_key(app, key),
        Mode::Settings => return settings_key(app, key),
        Mode::Links => {
            match key.code {
                KeyCode::Esc => app.view.mode = app.view.base_mode(),
                KeyCode::Char('j') | KeyCode::Down => {
                    app.view.link_index =
                        (app.view.link_index + 1).min(app.view.links.len().saturating_sub(1))
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.view.link_index = app.view.link_index.saturating_sub(1)
                }
                KeyCode::Enter => {
                    if let Some(url) = app.view.links.get(app.view.link_index) {
                        platform::open_url(url)?;
                    }
                    app.view.mode = app.view.base_mode();
                }
                _ => (),
            }
            return Ok(());
        }
        Mode::Compose => {
            if app.sending {
                return Ok(());
            }
            if key.code == KeyCode::Esc {
                return app.close_composer(false);
            }
            if ctrl && key.code == KeyCode::Char('o') {
                app.view.file_path = textarea("", false);
                app.view.file_index = 0;
                app.view.file_list_focus = false;
                app.view.mode = Mode::Attachments;
                return Ok(());
            }
            if key.code == KeyCode::F(8)
                || ctrl
                    && key.modifiers.contains(Mod::SHIFT)
                    && matches!(key.code, KeyCode::Char('d' | 'D'))
            {
                app.view.confirm(Confirm::Discard);
                return Ok(());
            }
            if key.code == KeyCode::F(2) || key.code == KeyCode::Enter && (ctrl || alt) {
                if app.view.composer.as_ref().is_some_and(|c| c.ambiguous) {
                    app.view.confirm(Confirm::RetryAmbiguous);
                    return Ok(());
                }
                return app.send_composer();
            }
            if ctrl && key.code == KeyCode::Char('s') {
                if app.sending {
                    bail!("Wait for the send to finish");
                }
                app.view.schedule = ScheduleDialog::new();
                app.view.mode = Mode::Schedule;
                return Ok(());
            }
            if ctrl && key.code == KeyCode::Char('n') {
                if let Some(c) = &mut app.view.composer {
                    c.note = !c.note;
                }
                return Ok(());
            }
            if app.sending {
                return Ok(());
            }
            if let Some(c) = &mut app.view.composer {
                editor_input(&mut c.editor, key, false)?;
            }
            app.view.follow_end = true;
            return Ok(());
        }
        Mode::SearchInput => {
            match key.code {
                KeyCode::Esc => close_search(app),
                KeyCode::Down | KeyCode::Tab if !ctrl => {
                    app.view.mode = Mode::SearchResults;
                    if !app.search_hits.contains(&app.view.selected)
                        && let Some(id) = app.search_hits.first().cloned()
                    {
                        app.select_id(id, false);
                    }
                    app.mark_viewed();
                }
                KeyCode::Enter => {
                    app.view.mode = Mode::SearchResults;
                    app.view.pane = Pane::Thread;
                }
                KeyCode::F(6) => {
                    app.view.search_remote = !app.view.search_remote;
                    app.search_changed();
                }
                KeyCode::Char('i') if ctrl => {
                    app.view.search_remote = !app.view.search_remote;
                    app.search_changed();
                }
                _ => {
                    if editor_input(&mut app.view.search, key, true)? {
                        app.search_changed();
                    }
                }
            }
            return Ok(());
        }
        _ => (),
    }
    if app.view.filter_chord {
        app.view.filter_chord = false;
        if let KeyCode::Char(c) = key.code
            && let Some(filter) = Filter::chord(c)
        {
            app.set_filter(filter);
        }
        return Ok(());
    }
    match key.code {
        KeyCode::Char('q') => request_quit(app)?,
        KeyCode::Esc if app.view.search_active => close_search(app),
        KeyCode::Char('/') => {
            app.view.mode = Mode::SearchInput;
            app.view.search_active = true;
            app.view.pane = Pane::List;
            app.search_changed();
        }
        KeyCode::F(6) | KeyCode::Char('i') if app.view.search_active => {
            app.view.search_remote = !app.view.search_remote;
            app.search_changed();
        }
        KeyCode::Char('f') => app.view.filter_chord = true,
        KeyCode::Char('s') | KeyCode::Char(',') if key.code == KeyCode::Char('s') || ctrl => {
            app.view.mode = Mode::Settings;
            // Refresh inferred workspace and active polling preferences, not typed credentials.
            app.view.settings.fields[2] = textarea(&app.config.workspace, false);
        }
        KeyCode::Char('r') => app.refresh(),
        KeyCode::Char('y') => app.sync_archive()?,
        KeyCode::Char('e') => app.view.zoom = !app.view.zoom,
        KeyCode::Char('?') | KeyCode::F(1) => open_help(app),
        KeyCode::Char('a') => open_media(app)?,
        KeyCode::Char('d') if !ctrl => {
            if let Some(ThreadTarget::Media(media, _)) =
                selected_target(app).filter(|_| app.view.pane == Pane::Thread)
            {
                begin_download(app, media);
            } else {
                open_media(app)?;
            }
        }
        KeyCode::Char('o' | 'O') => {
            if let Some(ThreadTarget::Media(media, _)) =
                selected_target(app).filter(|_| app.view.pane == Pane::Thread)
            {
                platform::open_url(
                    if key.code == KeyCode::Char('o') && format::safe_url(&media.link_url) {
                        &media.link_url
                    } else {
                        &media.url
                    },
                )?;
            } else {
                app.open_browser(key.code == KeyCode::Char('O'))?;
            }
        }
        KeyCode::Char('L') => {
            let conversation = app.selected().context("Select a conversation first")?;
            app.view.links = conversation
                .messages
                .iter()
                .flat_map(|m| {
                    let mut links = format::links(&m.body);
                    for block in &m.blocks {
                        if let ContentBlock::Image(media) | ContentBlock::Attachment(media) = block
                        {
                            links.push(media.url.clone());
                            if format::safe_url(&media.link_url) {
                                links.push(media.link_url.clone());
                            }
                        }
                    }
                    links
                })
                .collect();
            app.view.links.sort();
            app.view.links.dedup();
            if app.view.links.is_empty() {
                bail!("No http/https links in this conversation");
            }
            app.view.link_index = 0;
            app.view.mode = Mode::Links;
        }
        KeyCode::Char('c') => {
            let c = app.selected().context("Select a conversation first")?;
            app.view
                .confirm(Confirm::Close(c.id.clone(), c.state == State::Closed));
        }
        KeyCode::Char('x') => {
            if let Some(ThreadTarget::Scheduled(id)) = selected_target(app) {
                app.view.confirm(Confirm::Cancel(id));
            } else {
                bail!("Select a scheduled message card in the thread first (l, then j/k)");
            }
        }
        KeyCode::F(8) => app.view.confirm(Confirm::Discard),
        KeyCode::Char('d' | 'D') if ctrl && key.modifiers.contains(Mod::SHIFT) => {
            app.view.confirm(Confirm::Discard)
        }
        KeyCode::Tab | KeyCode::BackTab if !app.view.search_active => {
            let index = Filter::ALL
                .iter()
                .position(|f| *f == app.config.filter)
                .unwrap_or(0);
            let next = if key.code == KeyCode::BackTab {
                (index + 4) % 5
            } else {
                (index + 1) % 5
            };
            app.set_filter(Filter::ALL[next]);
        }
        KeyCode::Char('h') | KeyCode::Left => {
            app.view.pane = Pane::List;
            if app.view.zoom {
                app.view.zoom = false;
            }
        }
        KeyCode::Char('l') | KeyCode::Right => app.view.pane = Pane::Thread,
        KeyCode::Char('j') | KeyCode::Down => navigate(app, 1),
        KeyCode::Char('k') | KeyCode::Up => {
            if app.view.search_active
                && app.view.pane == Pane::List
                && app.search_hits.first() == Some(&app.view.selected)
            {
                app.view.mode = Mode::SearchInput;
            } else {
                navigate(app, -1);
            }
        }
        KeyCode::Char('g') | KeyCode::Home => {
            if app.view.pane == Pane::List {
                app.move_selection(isize::MIN);
            } else {
                app.view.thread_scroll = 0;
                app.view.thread_item = None;
                app.view.follow_end = false;
            }
        }
        KeyCode::Char('G') | KeyCode::End => {
            if app.view.pane == Pane::List {
                app.move_selection(isize::MAX);
            } else {
                app.view.follow_end = true;
                app.view.thread_item = None;
            }
        }
        KeyCode::PageDown | KeyCode::Char('d') if key.code == KeyCode::PageDown || ctrl => {
            page(app, true)
        }
        KeyCode::PageUp | KeyCode::Char('u') if key.code == KeyCode::PageUp || ctrl => {
            page(app, false)
        }
        KeyCode::Enter => match selected_target(app) {
            Some(ThreadTarget::Media(media, image)) if app.view.pane == Pane::Thread => {
                if image {
                    app.request_image(&media, true);
                } else {
                    begin_download(app, media);
                }
            }
            Some(ThreadTarget::Scheduled(id)) if app.view.pane == Pane::Thread => {
                app.edit_scheduled(&id)?
            }
            _ => {
                app.open_composer()?;
                app.view.pane = Pane::Thread;
            }
        },
        _ => (),
    }
    Ok(())
}

fn selected_target(app: &App) -> Option<ThreadTarget> {
    app.view
        .thread_item
        .and_then(|i| app.view.thread_ranges.get(i))
        .map(|(target, _, _)| target.clone())
}

fn navigate(app: &mut App, delta: isize) {
    if app.view.pane == Pane::List {
        app.move_selection(delta);
    } else {
        let len = app.view.thread_ranges.len();
        if len > 0 {
            let index = app
                .view
                .thread_item
                .map(|i| i.saturating_add_signed(delta).min(len - 1))
                .unwrap_or(if delta > 0 { 0 } else { len - 1 });
            app.view.thread_item = Some(index);
            app.view.follow_end = false;
        }
    }
}

fn page(app: &mut App, down: bool) {
    if app.view.pane == Pane::List {
        app.move_selection(if down { 5 } else { -5 });
    } else {
        let amount = app.view.thread_rect.height.max(4) / 2;
        app.view.thread_scroll = if down {
            app.view.thread_scroll.saturating_add(amount)
        } else {
            app.view.thread_scroll.saturating_sub(amount)
        };
        app.view.thread_item = None;
        app.view.follow_end = false;
    }
}

fn close_search(app: &mut App) {
    app.cancel_search();
    app.view.search_active = false;
    app.view.mode = Mode::Inbox;
    app.view.pinned = true;
}

fn request_quit(app: &mut App) -> Result<()> {
    if app.sending || !app.scheduled.is_empty() {
        app.view.confirm(Confirm::Quit);
    } else {
        app.flush()?;
        app.quit = true;
    }
    Ok(())
}

fn editor_input(editor: &mut TextArea<'static>, key: KeyEvent, single_line: bool) -> Result<bool> {
    let ctrl = key.modifiers.contains(Mod::CONTROL);
    if single_line && matches!(key.code, KeyCode::Enter | KeyCode::Tab) {
        return Ok(false);
    }
    if ctrl {
        match key.code {
            KeyCode::Char('a') => {
                editor.select_all();
                return Ok(false);
            }
            KeyCode::Char('c') => {
                editor.copy();
                platform::copy(&editor.yank_text())?;
                return Ok(false);
            }
            KeyCode::Char('x') => {
                editor.copy();
                platform::copy(&editor.yank_text())?;
                return Ok(editor.cut());
            }
            KeyCode::Char('v') => {
                let text = platform::paste()?;
                return Ok(editor.insert_str(if single_line {
                    text.replace(['\n', '\r'], " ")
                } else {
                    text
                }));
            }
            KeyCode::Char('w') => return Ok(editor.delete_word()),
            KeyCode::Char('u') => return Ok(editor.delete_line_by_head()),
            KeyCode::Char('z') if key.modifiers.contains(Mod::SHIFT) => return Ok(editor.redo()),
            KeyCode::Char('z') => return Ok(editor.undo()),
            KeyCode::Char('y') => return Ok(editor.redo()),
            _ => (),
        }
    }
    Ok(editor.input(key))
}

fn settings_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(Mod::CONTROL);
    match key.code {
        KeyCode::Esc => app.view.mode = app.view.base_mode(),
        KeyCode::F(1) => open_help(app),
        KeyCode::Tab | KeyCode::BackTab => {
            app.view.settings.focus = if key.code == KeyCode::BackTab {
                (app.view.settings.focus + 12) % 13
            } else {
                (app.view.settings.focus + 1) % 13
            }
        }
        KeyCode::F(2) => app.save_settings()?,
        KeyCode::F(3) => app.login()?,
        KeyCode::F(4) => app.view.confirm(Confirm::SignOut),
        KeyCode::F(5) => app.sync_archive()?,
        KeyCode::F(6) => {
            platform::copy(auth::REDIRECT)?;
            app.message = "Callback URL copied.".into();
        }
        KeyCode::F(7) => {
            if !app.auth_url.is_empty() {
                platform::copy(&app.auth_url)?;
                app.message = "Authorization URL copied.".into();
            }
        }
        KeyCode::Char('l') if ctrl => app.login()?,
        KeyCode::Char('y') if ctrl => app.sync_archive()?,
        KeyCode::Enter | KeyCode::Char(' ') if app.view.settings.focus >= 6 => {
            settings_activate(app)?
        }
        KeyCode::Enter => app.view.settings.focus = (app.view.settings.focus + 1) % 13,
        KeyCode::Char('c') if ctrl && app.view.settings.focus >= 6 => request_quit(app)?,
        _ if app.view.settings.focus < 6 => {
            editor_input(
                &mut app.view.settings.fields[app.view.settings.focus],
                key,
                true,
            )?;
        }
        _ => (),
    }
    Ok(())
}

fn settings_activate(app: &mut App) -> Result<()> {
    match app.view.settings.focus {
        6 => app.view.settings.notifications = !app.view.settings.notifications,
        7 => {
            app.view.settings.month_first = match app.view.settings.month_first {
                None => Some(true),
                Some(true) => Some(false),
                Some(false) => None,
            }
        }
        8 => app.save_settings()?,
        9 => app.login()?,
        10 => app.view.confirm(Confirm::SignOut),
        11 => app.sync_archive()?,
        12 => app.view.mode = app.view.base_mode(),
        _ => (),
    }
    Ok(())
}

fn schedule_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let month_first = app
        .config
        .month_first
        .unwrap_or_else(schedule::system_month_first);
    let dialog = &mut app.view.schedule;
    if key.code == KeyCode::Esc {
        if dialog.confirming.take().is_none() {
            app.view.mode = Mode::Compose;
        }
        return Ok(());
    }
    if let Some(at) = dialog.confirming {
        if key.code == KeyCode::Enter {
            app.schedule_composer(at)?;
        }
        return Ok(());
    }
    match key.code {
        KeyCode::F(6) => {
            if !dialog.picker {
                if let Ok(at) = dialog.parsed(month_first)
                    && let Some(date) =
                        chrono::DateTime::from_timestamp(at, 0).map(|d| d.with_timezone(&Local))
                {
                    dialog.date = date.date_naive();
                    dialog.hour = date.hour();
                    dialog.minute = date.minute();
                }
            } else {
                dialog.input = textarea(
                    &format!("{} {:02}:{:02}", dialog.date, dialog.hour, dialog.minute),
                    false,
                );
            }
            dialog.picker = !dialog.picker;
            dialog.error.clear();
        }
        KeyCode::Enter => match dialog.parsed(month_first) {
            Ok(at) => dialog.confirming = Some(at),
            Err(error) => dialog.error = error.to_string(),
        },
        KeyCode::Tab | KeyCode::BackTab if dialog.picker => {
            dialog.focus = if key.code == KeyCode::BackTab {
                (dialog.focus + 2) % 3
            } else {
                (dialog.focus + 1) % 3
            }
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down if dialog.picker => {
            let delta = match key.code {
                KeyCode::Left => -1,
                KeyCode::Right => 1,
                KeyCode::Up => -7,
                _ => 7,
            };
            match dialog.focus {
                1 => {
                    dialog.hour =
                        (dialog.hour as i32 + if delta < 0 { -1 } else { 1 }).rem_euclid(24) as u32
                }
                2 => {
                    dialog.minute = (dialog.minute as i32 + if delta < 0 { -1 } else { 1 })
                        .rem_euclid(60) as u32
                }
                _ => {
                    if let Some(date) = dialog.date.checked_add_signed(Duration::days(delta)) {
                        dialog.date = date;
                    }
                }
            }
            dialog.error.clear();
        }
        KeyCode::PageUp | KeyCode::PageDown if dialog.picker => {
            let months = chrono::Months::new(1);
            if let Some(date) = if key.code == KeyCode::PageDown {
                dialog.date.checked_add_months(months)
            } else {
                dialog.date.checked_sub_months(months)
            } {
                dialog.date = date;
            }
            dialog.error.clear();
        }
        _ if !dialog.picker => {
            editor_input(&mut dialog.input, key, true)?;
            dialog.error.clear();
        }
        _ => (),
    }
    Ok(())
}

fn mouse_input(app: &mut App, mouse: MouseEvent) -> Result<()> {
    let position = Position::new(mouse.column, mouse.row);
    if app.view.mode == Mode::Settings {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some((index, _)) = app
                .view
                .settings
                .rects
                .iter()
                .find(|(_, rect)| rect.contains(position))
                .copied()
        {
            app.view.settings.focus = index;
            if index >= 6 {
                settings_activate(app)?;
            }
        }
        return Ok(());
    }
    if app.view.mode == Mode::Schedule {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && app.view.schedule.picker
            && let Some((date, _)) = app
                .view
                .schedule
                .cells
                .iter()
                .find(|(_, rect)| rect.contains(position))
                .copied()
        {
            app.view.schedule.date = date;
        }
        return Ok(());
    }
    if matches!(
        app.view.mode,
        Mode::Confirm | Mode::Help | Mode::Links | Mode::Media | Mode::Attachments | Mode::Download
    ) {
        return Ok(());
    }
    if app.view.composer_rect.contains(position) && app.view.composer.is_some() {
        if let Some(c) = &mut app.view.composer
            && matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)
            )
        {
            let inner = c
                .editor
                .block()
                .map(|b| b.inner(app.view.composer_rect))
                .unwrap_or(app.view.composer_rect);
            let (top, left) = c.editor.scroll_offset();
            let row = mouse.row.saturating_sub(inner.y) as usize + top as usize;
            let col = mouse.column.saturating_sub(inner.x) as usize + left as usize;
            let cursor = c.editor.screen_to_data(row, col);
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                c.editor.cancel_selection();
            } else if !c.editor.is_selecting() {
                c.editor.start_selection();
            }
            c.editor.move_cursor(CursorMove::Jump(
                cursor.0.min(u16::MAX as usize) as u16,
                cursor.1.min(u16::MAX as usize) as u16,
            ));
        }
        return Ok(());
    }
    if app.view.mode == Mode::Compose {
        return Ok(());
    }
    if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
        if let Some((filter, _)) = app
            .view
            .tabs
            .iter()
            .find(|(_, rect)| rect.contains(position))
            .copied()
        {
            app.set_filter(filter);
            return Ok(());
        }
        if app.view.search_active && app.view.search_rect.contains(position) {
            app.view.mode = Mode::SearchInput;
            return Ok(());
        }
    }
    if app.view.list_rect.contains(position) {
        app.view.pane = Pane::List;
        match mouse.kind {
            MouseEventKind::ScrollDown => app.move_selection(1),
            MouseEventKind::ScrollUp => app.move_selection(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                let offset = app.view.list_state.offset();
                let index =
                    offset + mouse.row.saturating_sub(app.view.list_rect.y + 1) as usize / 4;
                if let Some(id) = app.rows().get(index).map(|c| c.id.clone()) {
                    app.select_id(id, false);
                }
                if app.view.search_active {
                    app.view.mode = Mode::SearchResults;
                }
            }
            _ => (),
        }
    } else if app.view.thread_rect.contains(position) {
        app.view.pane = Pane::Thread;
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                app.view.thread_scroll = app.view.thread_scroll.saturating_add(3);
                app.view.thread_item = None;
                app.view.follow_end = false;
            }
            MouseEventKind::ScrollUp => {
                app.view.thread_scroll = app.view.thread_scroll.saturating_sub(3);
                app.view.thread_item = None;
                app.view.follow_end = false;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let row = mouse.row.saturating_sub(app.view.thread_rect.y + 1) as usize
                    + app.view.thread_scroll as usize;
                app.view.thread_item = app
                    .view
                    .thread_ranges
                    .iter()
                    .position(|(_, start, end)| row >= *start && row < *end);
                app.view.follow_end = false;
            }
            _ => (),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Store;
    use ratatui::{Terminal, backend::TestBackend};
    use tokio::sync::mpsc;

    fn demo(root: &std::path::Path) -> App {
        App::new(
            Store::open(Some(root)).unwrap(),
            None,
            None,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            true,
        )
        .unwrap()
    }

    fn press(app: &mut App, code: KeyCode) {
        input(app, Event::Key(KeyEvent::new(code, Mod::NONE))).unwrap();
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[tokio::test]
    async fn reader_media_and_file_dialogs_work_with_keyboard_and_small_terminals() {
        let root = tempfile::tempdir().unwrap();
        let mut app = demo(root.path());
        let image = Media {
            url: "https://example.org/image.png".into(),
            name: "Screenshot.png".into(),
            ..Default::default()
        };
        let attachment = Media {
            url: "https://example.org/invoice.pdf".into(),
            name: "Invoice.pdf".into(),
            content_type: "application/pdf".into(),
            size: 1234,
            ..Default::default()
        };
        app.cache.conversations.get_mut("1").unwrap().messages[0].blocks = vec![
            ContentBlock::Image(image.clone()),
            ContentBlock::Attachment(attachment),
        ];
        app.images.begin(&image.url, false);
        app.images.entries.insert(
            image.url.clone(),
            media::Preview::Ready(std::sync::Arc::new(image::RgbaImage::from_pixel(
                8,
                4,
                image::Rgba([255, 0, 0, 255]),
            ))),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Screenshot.png"));
        assert!(text.contains("Invoice.pdf"));
        assert!(text.contains('▀'));
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.view.mode, Mode::Media);
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("Screenshot.png"));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.view.mode, Mode::Download);
        assert!(app.view.file_path.lines().join("").ends_with("Invoice.pdf"));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("Destination path"));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.view.mode, Mode::Media);
        press(&mut app, KeyCode::Esc);
        app.open_composer().unwrap();
        input(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('o'), Mod::CONTROL)),
        )
        .unwrap();
        assert_eq!(app.view.mode, Mode::Attachments);
        let path = root.path().join("upload.txt");
        std::fs::write(&path, "content").unwrap();
        input(&mut app, Event::Paste(path.to_string_lossy().into_owned())).unwrap();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.view.composer.as_ref().unwrap().attachments.len(), 1);
        for (width, height) in [(120, 40), (80, 24), (45, 14)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        }
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Delete);
        assert!(app.view.composer.as_ref().unwrap().attachments.is_empty());
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.view.mode, Mode::Compose);
    }

    #[tokio::test]
    async fn rendering_does_not_fetch_external_or_offscreen_images() {
        let root = tempfile::tempdir().unwrap();
        let mut app = demo(root.path());
        app.demo = false;
        app.view.follow_end = false;
        app.view.thread_scroll = 0;
        app.cache.conversations.get_mut("1").unwrap().messages[0].blocks = vec![
            ContentBlock::Image(Media {
                url: "https://external.example.org/image.png".into(),
                ..Default::default()
            }),
            ContentBlock::Text {
                html: "line<br>".repeat(120),
            },
            ContentBlock::Image(Media {
                url: "https://downloads.intercomcdn.com/image.png".into(),
                ..Default::default()
            }),
        ];
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.images.entries.is_empty());
    }

    #[tokio::test]
    async fn settings_setup_help_preserves_unsaved_fields_and_focus() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = demo(dir.path());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        press(&mut app, KeyCode::Char('s'));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("F1 setup help"));

        app.view.settings.focus = 1;
        input(&mut app, Event::Paste("unsaved-secret".into())).unwrap();
        // Printable '?' must still be accepted in a credential field.
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.view.mode, Mode::Settings);

        for close in [KeyCode::Esc, KeyCode::Char('?'), KeyCode::F(1)] {
            app.view.help_scroll = 10;
            press(&mut app, KeyCode::F(1));
            assert_eq!(app.view.mode, Mode::Help);
            assert_eq!(app.view.help_scroll, 0);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let text = screen(&terminal);
            for expected in [
                "Setup help",
                "Developer Hub",
                "https://app.intercom.com/a/developers",
                "Configure > Authentication",
                "Read conversations, Write conversations, Read admins",
                auth::REDIRECT,
                "Basic Information",
                "Client ID and Client secret",
                "Press F3",
            ] {
                assert!(text.contains(expected), "Missing {expected}: {text}");
            }
            assert!(!text.contains("unsaved-secret"));
            // Cancelling another overlay must not lose help's return destination.
            app.view.confirm(Confirm::Quit);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.view.mode, Mode::Help);
            press(&mut app, close);
            assert_eq!(app.view.mode, Mode::Settings);
            assert_eq!(app.view.settings.focus, 1);
            assert_eq!(app.view.settings.value(1), "unsaved-secret?");
        }
        assert!(!app.store.config_dir.join("oauth.json").exists());
        assert!(!app.store.token_path().exists());
    }

    #[tokio::test]
    async fn setup_help_scrolls_to_end_on_small_terminals() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = demo(dir.path());
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::F(1));
        for (width, height) in [(80, 24), (45, 14)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            app.view.help_scroll = u16::MAX;
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let end = app.view.help_scroll;
            let text = screen(&terminal);
            assert!(text.contains("if links fail."), "{text}");
            press(&mut app, KeyCode::PageDown);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            assert_eq!(app.view.help_scroll, end);
        }
    }

    #[tokio::test]
    async fn general_help_includes_setup_and_returns_to_previous_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = demo(dir.path());
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        for (mode, key) in [
            (Mode::Inbox, KeyCode::Char('?')),
            (Mode::SearchResults, KeyCode::F(1)),
        ] {
            app.view.mode = mode;
            press(&mut app, key);
            assert_eq!(app.view.mode, Mode::Help);
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let text = screen(&terminal);
            assert!(text.contains("SETUP (recommended: personal OAuth)"));
            assert!(text.contains("INBOX & READER"));
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.view.mode, mode);
        }
    }

    #[tokio::test]
    async fn renders_reader_settings_search_schedule_and_small_terminals() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = demo(dir.path());
        for (width, height) in [(120, 40), (80, 24), (45, 14), (30, 8)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for mode in [
                Mode::Inbox,
                Mode::Settings,
                Mode::SearchInput,
                Mode::Help,
                Mode::Schedule,
            ] {
                app.view.mode = mode;
                app.view.search_active = mode == Mode::SearchInput;
                terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn composer_enter_newlines_escape_saves_note_and_filter_chord_works() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = demo(dir.path());
        app.open_composer().unwrap();
        input(&mut app, Event::Paste("Hello".into())).unwrap();
        input(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Enter, Mod::NONE)),
        )
        .unwrap();
        input(&mut app, Event::Paste("World".into())).unwrap();
        input(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('n'), Mod::CONTROL)),
        )
        .unwrap();
        input(&mut app, Event::Key(KeyEvent::new(KeyCode::Esc, Mod::NONE))).unwrap();
        assert_eq!(app.drafts["1"].body, "Hello\nWorld");
        assert!(app.drafts["1"].note);
        input(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('f'), Mod::NONE)),
        )
        .unwrap();
        input(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('s'), Mod::NONE)),
        )
        .unwrap();
        assert_eq!(app.config.filter, Filter::Snoozed);
        assert_eq!(app.rows().len(), 1);
    }

    #[test]
    fn portable_undo_redo_and_line_deletion() {
        let mut editor = textarea("", false);
        editor_input(
            &mut editor,
            KeyEvent::new(KeyCode::Char('x'), Mod::NONE),
            false,
        )
        .unwrap();
        editor_input(
            &mut editor,
            KeyEvent::new(KeyCode::Char('z'), Mod::CONTROL),
            false,
        )
        .unwrap();
        assert_eq!(editor.lines(), &[""]);
        editor_input(
            &mut editor,
            KeyEvent::new(KeyCode::Char('y'), Mod::CONTROL),
            false,
        )
        .unwrap();
        assert_eq!(editor.lines(), &["x"]);
        editor.insert_str("\nsecond line");
        editor_input(
            &mut editor,
            KeyEvent::new(KeyCode::Char('u'), Mod::CONTROL),
            false,
        )
        .unwrap();
        assert_eq!(editor.lines(), &["x", ""]);
    }
}
