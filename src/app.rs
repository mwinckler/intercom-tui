use crate::{
    api::{Api, ApiError, ErrorKind},
    auth, format, media,
    model::*,
    platform, schedule,
    storage::{Cache, Config, Store},
    ui::{Composer, Confirm, Mode, Settings, View},
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use futures::{StreamExt, stream};
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};

pub enum Event {
    Connected {
        generation: u64,
        result: std::result::Result<(Api, Identity), String>,
        token: Option<String>,
    },
    Poll {
        generation: u64,
        result: std::result::Result<(Vec<Conversation>, Vec<String>), ApiError>,
        counts: std::result::Result<Counts, ApiError>,
    },
    Inbox {
        generation: u64,
        filter: Filter,
        result: std::result::Result<Vec<Conversation>, ApiError>,
    },
    Thread {
        generation: u64,
        id: String,
        result: std::result::Result<Conversation, ApiError>,
    },
    Reply {
        generation: u64,
        id: String,
        scheduled: Option<String>,
        result: std::result::Result<Conversation, ApiError>,
    },
    Changed {
        generation: u64,
        id: String,
        reopen: bool,
        result: std::result::Result<(), ApiError>,
    },
    Search {
        generation: u64,
        request: u64,
        result: std::result::Result<Vec<Conversation>, String>,
    },
    Sync {
        generation: u64,
        scanned: usize,
        total: usize,
        stored: usize,
        skipped: usize,
        failed: usize,
    },
    Synced {
        generation: u64,
        result: std::result::Result<String, String>,
    },
    AuthUrl {
        generation: u64,
        url: String,
    },
    Image {
        generation: u64,
        url: String,
        result: std::result::Result<std::sync::Arc<image::RgbaImage>, String>,
    },
    Download {
        generation: u64,
        result: std::result::Result<String, String>,
    },
}

pub struct App {
    pub store: Store,
    pub config: Config,
    pub cache: Cache,
    pub identity: Identity,
    pub drafts: BTreeMap<String, Draft>,
    pub scheduled: Vec<Scheduled>,
    pub api: Option<Api>,
    pub view: View,
    pub message: String,
    pub auth_url: String,
    pub connecting: bool,
    pub polling: bool,
    pub inbox_loading: bool,
    pub thread_loading: HashSet<String>,
    pub sending: bool,
    pub syncing: bool,
    pub searching: bool,
    pub downloading: bool,
    pub images: media::Images,
    pub focused: bool,
    pub quit: bool,
    pub search_hits: Vec<String>,
    pub platform_tx: mpsc::UnboundedSender<platform::PlatformEvent>,
    tx: mpsc::UnboundedSender<Event>,
    generation: u64,
    tasks: Vec<JoinHandle<()>>,
    token_file: Option<PathBuf>,
    next_poll: Instant,
    last_flush: Instant,
    dirty: bool,
    notification_seeded: bool,
    notification_threads: HashSet<String>,
    notification_history: BTreeMap<String, Conversation>,
    pending_states: BTreeMap<String, (State, i64)>,
    search_request: u64,
    search_changed: Option<Instant>,
    pub demo: bool,
}

impl App {
    pub fn new(
        store: Store,
        token_file: Option<PathBuf>,
        conversation: Option<String>,
        tx: mpsc::UnboundedSender<Event>,
        platform_tx: mpsc::UnboundedSender<platform::PlatformEvent>,
        demo: bool,
    ) -> Result<Self> {
        let mut config = store.config()?;
        config.refresh_seconds = config.refresh_seconds.clamp(15, 3600);
        let identity = config.identity.clone();
        let cache = store.cache(&identity)?;
        let drafts = store.drafts(&identity)?;
        let scheduled = store.scheduled(&identity)?;
        let settings = Settings::new(&store.credentials()?, &config);
        let mut view = View::new(settings);
        view.selected = conversation.unwrap_or_else(|| config.selected.clone());
        view.pinned = !view.selected.is_empty();
        view.zoom = config.zoom;
        let mut app = Self {
            store,
            config,
            cache,
            identity,
            drafts,
            scheduled,
            api: None,
            view,
            message: "Connecting to Intercom…".into(),
            auth_url: String::new(),
            connecting: false,
            polling: false,
            inbox_loading: false,
            thread_loading: HashSet::new(),
            sending: false,
            syncing: false,
            searching: false,
            downloading: false,
            images: media::Images::default(),
            focused: true,
            quit: false,
            search_hits: vec![],
            platform_tx,
            tx,
            generation: 0,
            tasks: vec![],
            token_file,
            next_poll: Instant::now(),
            last_flush: Instant::now(),
            dirty: false,
            notification_seeded: false,
            notification_threads: HashSet::new(),
            notification_history: BTreeMap::new(),
            pending_states: BTreeMap::new(),
            search_request: 0,
            search_changed: None,
            demo,
        };
        if demo {
            app.populate_demo();
        } else {
            app.connect(None)?;
        }
        Ok(app)
    }

    fn task(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.tasks.retain(|task| !task.is_finished());
        self.tasks.push(tokio::spawn(future));
    }

    pub fn connect(&mut self, saved_token: Option<String>) -> Result<()> {
        if self.demo {
            return Ok(());
        }
        if self.connecting {
            bail!("Sign-in is already in progress");
        }
        let token = if let Some(token) = saved_token.clone() {
            Some(token)
        } else {
            self.store.read_token(
                self.token_file
                    .as_deref()
                    .or(self.config.token_file.as_deref()),
            )?
        };
        let Some(token) = token else {
            self.view.mode = Mode::Settings;
            self.message =
                "Need app credentials? F1 shows setup steps. Enter them, then F3 to sign in."
                    .into();
            return Ok(());
        };
        self.connecting = true;
        let tx = self.tx.clone();
        let generation = self.generation;
        let region = self.config.region;
        let workspace = self.config.workspace.clone();
        self.task(async move {
            let result = Api::connect(token, region, workspace)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Event::Connected {
                generation,
                result,
                token: saved_token,
            });
        });
        Ok(())
    }

    pub fn login(&mut self) -> Result<()> {
        if self.demo {
            bail!("Demo mode never connects to Intercom");
        }
        self.require_no_external_token()?;
        if self.connecting {
            bail!("Sign-in is already in progress");
        }
        if self.sending {
            bail!("Wait for the current send to finish before signing in");
        }
        self.persist_settings(false)?;
        let credentials = self.store.credentials()?;
        if credentials.client_id.is_empty() || credentials.client_secret.is_empty() {
            bail!("Paste the shared app Client ID and secret first");
        }
        self.connecting = true;
        self.message = "Authorize in your browser. Esc returns to the inbox; sign-in expires after 150 seconds.".into();
        let tx = self.tx.clone();
        let generation = self.generation;
        let region = self.config.region;
        let workspace = self.config.workspace.clone();
        self.task(async move {
            let (urls, mut receiver) = mpsc::unbounded_channel();
            let forward = tokio::spawn({
                let tx = tx.clone();
                async move {
                    while let Some(url) = receiver.recv().await {
                        let _ = tx.send(Event::AuthUrl { generation, url });
                    }
                }
            });
            let (result, token) = match auth::login(credentials, region, urls).await {
                Ok(token) => (
                    Api::connect(token.clone(), region, workspace)
                        .await
                        .map_err(|e| e.to_string()),
                    Some(token),
                ),
                Err(error) => (Err(error.to_string()), None),
            };
            forward.abort();
            let _ = tx.send(Event::Connected {
                generation,
                result,
                token,
            });
        });
        Ok(())
    }

    fn require_no_external_token(&self) -> Result<()> {
        if std::env::var("INTERCOM_ACCESS_TOKEN").is_ok_and(|s| !s.trim().is_empty()) {
            bail!(
                "INTERCOM_ACCESS_TOKEN overrides saved credentials. Unset it before signing in or out."
            );
        }
        if self.token_file.is_some() || self.config.token_file.is_some() {
            bail!(
                "An external token file is active. Clear Access token file (and --token-file) before signing in or out. External files are never deleted."
            );
        }
        if self.store.portable_token_active() {
            bail!(
                "A portable token beside the executable is active. Move or remove it yourself before signing in or out; the app does not delete external credentials."
            );
        }
        Ok(())
    }

    pub fn sign_out(&mut self) -> Result<()> {
        self.require_no_external_token()?;
        self.flush()?;
        self.store.remove_token()?;
        self.cancel_tasks();
        self.identity = Identity::default();
        self.config.identity = Identity::default();
        self.config.selected.clear();
        self.api = None;
        self.cache = Cache::default();
        self.drafts.clear();
        self.scheduled.clear();
        self.search_hits.clear();
        self.view.selected.clear();
        self.view.composer = None;
        self.view.search_active = false;
        self.view.mode = Mode::Settings;
        self.config.workspace.clear();
        self.auth_url.clear();
        self.store.save_config(&self.config)?;
        self.message =
            "Signed out. App credentials are retained; account data is stored separately.".into();
        Ok(())
    }

    fn cancel_tasks(&mut self) {
        self.generation += 1;
        for task in self.tasks.drain(..) {
            task.abort();
        }
        self.connecting = false;
        self.polling = false;
        self.inbox_loading = false;
        self.sending = false;
        self.syncing = false;
        self.searching = false;
        self.downloading = false;
        self.images = media::Images::default();
        self.thread_loading.clear();
        self.notification_seeded = false;
        self.notification_threads.clear();
        self.notification_history.clear();
        self.pending_states.clear();
    }

    pub fn save_settings(&mut self) -> Result<()> {
        self.persist_settings(true)
    }

    fn persist_settings(&mut self, apply_auth: bool) -> Result<()> {
        if self.demo {
            bail!("Settings are not saved in demo mode");
        }
        if apply_auth && (self.connecting || self.sending) {
            bail!("Wait for the current authentication/send operation to finish");
        }
        let previous_token_file = self.config.token_file.clone();
        let settings = &self.view.settings;
        let workspace = settings.value(2);
        let workspace = if workspace.trim().is_empty() {
            String::new()
        } else {
            workspace_id(workspace.trim())
                .context("Enter a valid workspace ID or Intercom inbox URL")?
        };
        let refresh: u64 = settings
            .value(5)
            .trim()
            .parse()
            .context("Refresh interval must be a number from 15 to 3600 seconds")?;
        if !(15..=3600).contains(&refresh) {
            bail!("Refresh interval must be between 15 and 3600 seconds");
        }
        let token_file = settings.value(4);
        self.config.token_file =
            (!token_file.trim().is_empty()).then(|| PathBuf::from(token_file.trim()));
        self.config.workspace = workspace;
        self.config.refresh_seconds = refresh;
        self.config.notifications = settings.notifications;
        self.config.month_first = settings.month_first;
        let mut credentials = self.store.credentials()?;
        credentials.client_id = settings.value(0).trim().into();
        // A blank secret keeps the existing shared app secret.
        if !settings.value(1).trim().is_empty() {
            credentials.client_secret = settings.value(1).trim().into();
        }
        self.store.save_credentials(&credentials)?;
        self.store.save_config(&self.config)?;
        if let Some(api) = &mut self.api {
            api.workspace = self.config.workspace.clone();
        }
        self.message = "Settings saved.".into();
        let token = self.view.settings.value(3);
        if apply_auth && !token.trim().is_empty() {
            self.require_no_external_token()?;
            self.cancel_tasks();
            self.api = None;
            self.connect(Some(token.trim().into()))?;
            self.view.settings.clear_token();
        } else if apply_auth
            && self.token_file.is_none()
            && previous_token_file != self.config.token_file
        {
            self.cancel_tasks();
            self.api = None;
            self.connect(None)?;
        }
        Ok(())
    }

    pub fn refresh(&mut self) {
        if self.demo {
            self.message = "Demo mode: no network requests are made.".into();
            return;
        }
        if self.api.is_none() {
            if let Err(e) = self.connect(None) {
                self.message = e.to_string();
            }
            return;
        }
        self.poll();
        self.load_inbox(self.config.filter);
        let id = self.view.selected.clone();
        if !id.is_empty() {
            self.load_thread(&id, true);
        }
    }

    fn poll(&mut self) {
        if self.polling {
            return;
        }
        let Some(api) = self.api.clone() else {
            return;
        };
        self.polling = true;
        let tx = self.tx.clone();
        let generation = self.generation;
        self.task(async move {
            let (result, counts) = tokio::join!(api.poll(), api.counts());
            let _ = tx.send(Event::Poll {
                generation,
                result,
                counts,
            });
        });
    }

    pub fn load_inbox(&mut self, filter: Filter) {
        if self.demo || self.inbox_loading {
            return;
        }
        // The regular poll is already a complete Open inbox fetch.
        if matches!(filter, Filter::Open | Filter::Waiting) {
            self.poll();
            return;
        }
        let Some(api) = self.api.clone() else {
            return;
        };
        self.inbox_loading = true;
        let tx = self.tx.clone();
        let generation = self.generation;
        self.task(async move {
            let result = api.list(filter).await;
            let _ = tx.send(Event::Inbox {
                generation,
                filter,
                result,
            });
        });
    }

    pub fn set_filter(&mut self, filter: Filter) {
        self.cancel_search();
        self.config.filter = filter;
        self.view.pinned = false;
        self.view.search_active = false;
        self.view.mode = Mode::Inbox;
        self.view.list_state.select(Some(0));
        self.view.selected = self
            .rows()
            .first()
            .map(|c| c.id.clone())
            .unwrap_or_default();
        self.dirty = true;
        self.load_inbox(filter);
        self.selection_changed();
    }

    pub fn rows(&self) -> Vec<&Conversation> {
        if self.view.search_active {
            return self
                .search_hits
                .iter()
                .filter_map(|id| self.cache.conversations.get(id))
                .collect();
        }
        let filter = self.config.filter;
        let key = if filter == Filter::Waiting {
            Filter::Open
        } else {
            filter
        }
        .label();
        let mut ids: HashSet<String> = self
            .cache
            .inboxes
            .get(key)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        for c in self.cache.conversations.values() {
            if self.cache.reads.unread(&c.id) && filter.matches(c) {
                ids.insert(c.id.clone());
            }
        }
        let mut rows: Vec<_> = ids
            .iter()
            .filter_map(|id| self.cache.conversations.get(id))
            .filter(|c| filter.matches(c))
            .collect();
        rows.sort_by_key(|c| {
            (
                std::cmp::Reverse(self.cache.reads.unread(&c.id)),
                std::cmp::Reverse(if filter == Filter::Waiting {
                    c.waiting_since
                } else {
                    c.updated_at
                }),
                c.id.clone(),
            )
        });
        if self.view.pinned
            && !self.view.selected.is_empty()
            && !rows.iter().any(|c| c.id == self.view.selected)
            && let Some(c) = self.cache.conversations.get(&self.view.selected)
        {
            rows.insert(0, c);
        }
        rows
    }

    pub fn move_selection(&mut self, delta: isize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let current = rows
            .iter()
            .position(|c| c.id == self.view.selected)
            .unwrap_or(0);
        let next = current.saturating_add_signed(delta).min(rows.len() - 1);
        let id = rows[next].id.clone();
        self.view.pinned = false;
        self.view.selected = id;
        self.view.list_state.select(Some(next));
        self.selection_changed();
    }

    pub fn select_id(&mut self, id: String, pin: bool) {
        if !safe_id(&id) {
            return;
        }
        self.view.selected = id;
        self.view.pinned = pin;
        self.selection_changed();
    }

    fn selection_changed(&mut self) {
        self.config.selected = self.view.selected.clone();
        self.view.thread_scroll = 0;
        self.view.thread_item = None;
        self.view.follow_end = true;
        self.dirty = true;
        let id = self.view.selected.clone();
        self.mark_viewed();
        if !id.is_empty() {
            self.load_thread(&id, false);
        }
    }

    pub fn selected(&self) -> Option<&Conversation> {
        self.cache.conversations.get(&self.view.selected)
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn mark_viewed(&mut self) {
        if self.focused && !matches!(self.view.mode, Mode::Settings | Mode::Help) {
            self.cache.reads.mark_read(&self.view.selected);
            self.dirty = true;
        }
    }

    pub fn load_thread(&mut self, id: &str, force: bool) {
        if self.demo || self.thread_loading.contains(id) {
            return;
        }
        let now = Utc::now().timestamp();
        if !force
            && self.cache.conversations.get(id).is_some_and(|c| {
                c.complete
                    && c.content_version == CONTENT_VERSION
                    && (c.state == State::Closed || now - c.fetched_at < 120)
            })
        {
            return;
        }
        if !force {
            match self.store.archived(&self.identity, id) {
                Ok(Some(conversation)) if conversation.complete => {
                    self.merge(conversation);
                }
                Ok(_) => (),
                Err(e) => self.message = e.to_string(),
            }
        }
        let Some(api) = self.api.clone() else {
            return;
        };
        self.thread_loading.insert(id.into());
        let tx = self.tx.clone();
        let generation = self.generation;
        let id = id.to_owned();
        self.task(async move {
            let result = api.thread(&id).await;
            let _ = tx.send(Event::Thread {
                generation,
                id,
                result,
            });
        });
    }

    fn merge(&mut self, mut row: Conversation) {
        if let Some(old) = self.cache.conversations.get(&row.id)
            && old.updated_at > row.updated_at
        {
            // Archive hits and slow in-flight requests cannot roll live state back.
            row.state = old.state;
            row.waiting_since = old.waiting_since;
            row.updated_at = old.updated_at;
            row.title = old.title.clone();
            row.from = old.from.clone();
            row.email = old.email.clone();
            row.snippet = old.snippet.clone();
            for message in &old.messages {
                if !row.messages.iter().any(|m| m.id == message.id) {
                    row.messages.push(message.clone());
                }
            }
            row.messages.sort_by_key(|m| (m.created_at, m.id.clone()));
            row.complete = old.complete;
            row.fetched_at = old.fetched_at;
            row.content_version = old.content_version;
        }
        if let Some((state, until)) = self.pending_states.get(&row.id).copied() {
            if Utc::now().timestamp() >= until {
                self.pending_states.remove(&row.id);
            } else {
                row.state = state;
                if state != State::Open {
                    row.waiting_since = 0;
                }
            }
        }
        if let Some(old) = self.cache.conversations.get(&row.id)
            && !row.complete
        {
            // Summaries and write responses must not replace full history.
            let mut messages = old.messages.clone();
            for message in &row.messages {
                if let Some(existing) = messages.iter_mut().find(|m| m.id == message.id) {
                    let has_media = |m: &Message| {
                        m.blocks.iter().any(|block| {
                            matches!(block, ContentBlock::Image(_) | ContentBlock::Attachment(_))
                        })
                    };
                    if row.updated_at >= old.updated_at
                        && row.content_version >= old.content_version
                        // Search summaries can omit attachments. Only a full
                        // thread is authoritative for existing media metadata.
                        && !has_media(existing)
                    {
                        *existing = message.clone();
                    }
                } else {
                    messages.push(message.clone());
                }
            }
            messages.sort_by_key(|m| (m.created_at, m.id.clone()));
            row.messages = messages;
            if row.updated_at <= old.updated_at {
                row.complete = old.complete;
                row.fetched_at = old.fetched_at;
                row.content_version = old.content_version;
            }
            if row.title.is_empty() {
                row.title = old.title.clone();
            }
        }
        let read = self.focused
            && row.id == self.view.selected
            && !matches!(self.view.mode, Mode::Settings | Mode::Help);
        self.cache.reads.observe(&row, read, &self.identity);
        self.cache.conversations.insert(row.id.clone(), row);
        self.dirty = true;
    }

    pub fn open_composer(&mut self) -> Result<()> {
        if self.sending {
            bail!("A send is still in progress");
        }
        let conversation = self.selected().context("Select a conversation first")?;
        if !conversation.complete {
            bail!("Wait for the full thread to load before composing (press r to retry)");
        }
        let draft = self
            .drafts
            .get(&conversation.id)
            .cloned()
            .unwrap_or_default();
        self.view.composer = Some(
            Composer::new(
                conversation.id.clone(),
                draft.body,
                draft.note,
                seen(&conversation.messages),
            )
            .with_attachments(draft.attachments),
        );
        self.view.mode = Mode::Compose;
        self.view.follow_end = true;
        Ok(())
    }

    pub fn save_composer(&mut self) -> Result<()> {
        if let Some(composer) = &self.view.composer {
            let body = composer.body();
            if body.trim().is_empty() && composer.attachments.is_empty() {
                self.drafts.remove(&composer.id);
            } else {
                self.drafts.insert(
                    composer.id.clone(),
                    Draft {
                        body,
                        note: composer.note,
                        attachments: composer.attachments.clone(),
                    },
                );
            }
            if !self.demo {
                self.store.save_drafts(&self.identity, &self.drafts)?;
            }
        }
        Ok(())
    }

    pub fn close_composer(&mut self, discard: bool) -> Result<()> {
        if self.sending {
            bail!("Wait for the send result before closing the composer");
        }
        if discard {
            let id = self
                .view
                .composer
                .as_ref()
                .map(|c| c.id.clone())
                .unwrap_or_else(|| self.view.selected.clone());
            let mut next = self.drafts.clone();
            next.remove(&id);
            if !self.demo {
                self.store.save_drafts(&self.identity, &next)?;
            }
            self.drafts = next;
        } else {
            self.save_composer()?;
        }
        self.view.composer = None;
        self.view.mode = if self.view.search_active {
            Mode::SearchResults
        } else {
            Mode::Inbox
        };
        Ok(())
    }

    pub fn send_composer(&mut self) -> Result<()> {
        if self.demo {
            bail!("Demo mode never sends messages");
        }
        if self.sending {
            bail!("A send is already in progress");
        }
        let composer = self.view.composer.as_ref().context("Write a reply first")?;
        if composer.body().trim().is_empty() && composer.attachments.is_empty() {
            bail!("Write a message or attach a file before sending");
        }
        let (id, draft, seen) = (
            composer.id.clone(),
            Draft {
                body: composer.body(),
                note: composer.note,
                attachments: composer.attachments.clone(),
            },
            composer.seen.clone(),
        );
        self.save_composer()?;
        self.send(id, draft, seen, None)
    }

    fn send(
        &mut self,
        id: String,
        draft: Draft,
        seen: Vec<SeenMessage>,
        scheduled: Option<String>,
    ) -> Result<()> {
        let api = self.api.clone().context("Sign in before sending")?;
        self.sending = true;
        self.message = "Checking for new comments before sending…".into();
        let tx = self.tx.clone();
        let generation = self.generation;
        self.task(async move {
            let result = api
                .reply_with_attachments(&id, &draft.body, draft.note, &seen, &draft.attachments)
                .await;
            let _ = tx.send(Event::Reply {
                generation,
                id,
                scheduled,
                result,
            });
        });
        Ok(())
    }

    pub fn schedule_composer(&mut self, at: i64) -> Result<()> {
        if at <= Utc::now().timestamp() {
            bail!("Choose a time in the future");
        }
        let composer = self.view.composer.as_ref().context("Write a reply first")?;
        if composer.body().trim().is_empty() && composer.attachments.is_empty() {
            bail!("Write a message or attach a file before scheduling");
        }
        let mut rows = self.scheduled.clone();
        rows.push(Scheduled {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: composer.id.clone(),
            body: composer.body(),
            note: composer.note,
            attachments: composer.attachments.clone(),
            send_at: at,
            seen: composer.seen.clone(),
            status: SendState::Pending,
        });
        rows.sort_by_key(|s| s.send_at);
        if !self.demo {
            self.store.save_scheduled(&self.identity, &rows)?;
        }
        self.scheduled = rows;
        self.close_composer(true)?;
        self.message = format!(
            "Scheduled for {}. Keep the TUI open to send on time.",
            schedule::label(at)
        );
        Ok(())
    }

    pub fn edit_scheduled(&mut self, id: &str) -> Result<()> {
        let row = self
            .scheduled
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .context("Scheduled message not found")?;
        if self
            .drafts
            .get(&row.conversation_id)
            .is_some_and(|d| !d.is_empty())
        {
            bail!(
                "This thread already has a draft. Send or discard it before editing the scheduled message."
            );
        }
        if matches!(row.status, SendState::Sending) {
            bail!("This scheduled message is currently sending");
        }
        self.view.composer = Some(
            Composer::new(
                row.conversation_id.clone(),
                row.body.clone(),
                row.note,
                row.seen.clone(),
            )
            .with_attachments(row.attachments.clone()),
        );
        self.save_composer()?; // Persist the replacement before removing the queue entry.
        let mut next = self.scheduled.clone();
        next.retain(|s| s.id != id);
        if !self.demo {
            self.store.save_scheduled(&self.identity, &next)?;
        }
        self.scheduled = next;
        self.view.selected = row.conversation_id;
        self.view.mode = Mode::Compose;
        self.view.follow_end = true;
        Ok(())
    }

    pub fn cancel_scheduled(&mut self, id: &str) -> Result<()> {
        if self
            .scheduled
            .iter()
            .any(|s| s.id == id && matches!(s.status, SendState::Sending))
        {
            bail!("This message is already sending");
        }
        let mut next = self.scheduled.clone();
        next.retain(|s| s.id != id);
        if !self.demo {
            self.store.save_scheduled(&self.identity, &next)?;
        }
        self.scheduled = next;
        self.message = "Scheduled message cancelled.".into();
        Ok(())
    }

    fn send_due(&mut self) -> Result<()> {
        if self.demo
            || self.api.is_none()
            || self.sending
            || self.connecting
            || self.view.composer.is_some()
            || matches!(self.view.mode, Mode::Confirm | Mode::Download | Mode::Media)
        {
            return Ok(());
        }
        if let Some(index) = self
            .scheduled
            .iter()
            .position(|s| s.due(Utc::now().timestamp()))
        {
            let row = self.scheduled[index].clone();
            self.scheduled[index].status = SendState::Sending;
            // Journal before the network side effect. Crash recovery never blindly retries.
            if let Err(error) = self.store.save_scheduled(&self.identity, &self.scheduled) {
                self.scheduled[index].status = SendState::Pending;
                return Err(error);
            }
            self.send(
                row.conversation_id,
                Draft {
                    body: row.body,
                    note: row.note,
                    attachments: row.attachments,
                },
                row.seen,
                Some(row.id),
            )?;
        }
        Ok(())
    }

    pub fn add_attachment(&mut self, value: &str) -> Result<()> {
        if self.sending {
            bail!("Wait for the send to finish");
        }
        let path = media::local_path(value)?.to_string_lossy().into_owned();
        let composer = self
            .view
            .composer
            .as_mut()
            .context("Open a composer first")?;
        let mut paths = composer.attachments.clone();
        if !paths.contains(&path) {
            paths.push(path);
        }
        media::validate_files(&paths)?;
        composer.attachments = paths;
        self.save_composer()?;
        self.message = "Attachment added. Files are read and uploaded only when sending.".into();
        Ok(())
    }

    pub fn request_image(&mut self, image: &Media, explicit: bool) {
        if self.demo
            || !format::safe_url(&image.url)
            || !explicit && !media::automatic(image)
            || !self.images.begin(&image.url, explicit)
        {
            return;
        }
        let url = image.url.clone();
        let tx = self.tx.clone();
        let generation = self.generation;
        self.task(async move {
            let result = media::load_image(&url, !explicit)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Event::Image {
                generation,
                url,
                result,
            });
        });
    }

    pub fn download_attachment(&mut self, media: &Media, destination: &str) -> Result<()> {
        if self.demo {
            bail!("Demo mode never downloads files");
        }
        if self.downloading {
            bail!("Wait for the current download to finish");
        }
        let destination = media::local_path(destination)?;
        if destination.symlink_metadata().is_ok() {
            bail!("Destination already exists. Choose a new filename.");
        }
        let url = media.url.clone();
        if !format::safe_url(&url) {
            bail!("Invalid HTTP(S) attachment URL");
        }
        self.downloading = true;
        self.message = "Downloading attachment…".into();
        let tx = self.tx.clone();
        let generation = self.generation;
        self.task(async move {
            let result = media::download(&url, &destination)
                .await
                .map(|_| {
                    format!(
                        "Saved attachment to {}",
                        format::safe_text(&destination.display().to_string())
                    )
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Event::Download { generation, result });
        });
        Ok(())
    }

    pub fn change_state(&mut self, id: String, reopen: bool) -> Result<()> {
        if self.demo {
            bail!("Demo mode does not change Intercom conversations");
        }
        if self.sending {
            bail!("Wait for the current operation to finish");
        }
        let api = self.api.clone().context("Sign in first")?;
        self.sending = true;
        let tx = self.tx.clone();
        let generation = self.generation;
        self.task(async move {
            let result = api.change_state(&id, reopen).await;
            let _ = tx.send(Event::Changed {
                generation,
                id,
                reopen,
                result,
            });
        });
        Ok(())
    }

    pub fn search_changed(&mut self) {
        self.search_request += 1; // Invalidate in-flight results immediately, not at the next debounce.
        self.search_changed = Some(Instant::now());
        self.view.search_active = true;
        self.search_hits.clear();
        if !self.view.search_remote {
            let query = self.view.search_text();
            let tokens: Vec<_> = query
                .to_lowercase()
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            if !tokens.is_empty() {
                let mut rows: Vec<_> = self
                    .cache
                    .conversations
                    .values()
                    .filter(|c| {
                        let hay = c.haystack();
                        tokens.iter().all(|t| hay.contains(t))
                    })
                    .collect();
                rows.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
                self.search_hits = rows.into_iter().take(50).map(|c| c.id.clone()).collect();
            }
        }
    }

    pub fn cancel_search(&mut self) {
        self.search_request += 1;
        self.search_changed = None;
        self.searching = false;
    }

    fn run_search(&mut self) {
        self.search_changed = None;
        let query = self.view.search_text();
        if query.trim().is_empty() {
            self.searching = false;
            return;
        }
        let remote = self.view.search_remote;
        if remote && self.api.is_none() {
            self.message = "Sign in for Intercom API search".into();
            return;
        }
        let api = self.api.clone();
        let store = self.store.clone();
        let identity = self.identity.clone();
        let tx = self.tx.clone();
        let generation = self.generation;
        let request = self.search_request;
        self.searching = true;
        self.task(async move {
            let result = if remote {
                api.unwrap().search(&query).await.map_err(|e| e.to_string())
            } else {
                tokio::task::spawn_blocking(move || store.search_archive(&identity, &query))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()))
            };
            let _ = tx.send(Event::Search {
                generation,
                request,
                result,
            });
        });
    }

    pub fn sync_archive(&mut self) -> Result<()> {
        if self.demo {
            bail!("Demo mode never downloads your archive");
        }
        if self.syncing {
            bail!("Archive sync is already running");
        }
        let api = self.api.clone().context("Sign in first")?;
        self.syncing = true;
        self.message = "Listing all conversations for the archive…".into();
        let store = self.store.clone();
        let identity = self.identity.clone();
        let tx = self.tx.clone();
        let generation = self.generation;
        self.task(async move {
            let result = sync_archive(api, store, identity, tx.clone(), generation)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Event::Synced { generation, result });
        });
        Ok(())
    }

    pub fn open_browser(&mut self, inbox: bool) -> Result<()> {
        let url = inbox_url(
            self.config.region,
            &self.config.workspace,
            if inbox {
                None
            } else {
                Some(&self.view.selected)
            },
        );
        if self.config.workspace.is_empty() && !inbox {
            bail!(
                "Set the workspace ID (or paste an inbox URL) in settings to open conversation links"
            );
        }
        platform::open_url(&url)
    }

    pub fn tick(&mut self) -> Result<()> {
        if self.api.is_some() && !self.polling && Instant::now() >= self.next_poll {
            self.poll();
            self.load_inbox(self.config.filter);
        }
        if self.search_changed.is_some_and(|changed| {
            changed.elapsed()
                >= Duration::from_millis(if self.view.search_remote { 400 } else { 180 })
        }) {
            self.run_search();
        }
        self.send_due()?;
        if self.last_flush.elapsed() >= Duration::from_secs(1) {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        if self.demo {
            self.dirty = false;
            return Ok(());
        }
        self.save_composer()?;
        if self.config.zoom != self.view.zoom || self.config.selected != self.view.selected {
            self.dirty = true;
        }
        if self.dirty {
            self.config.selected = self.view.selected.clone();
            self.config.zoom = self.view.zoom;
            self.store.save_config(&self.config)?;
            self.store.save_cache(&self.identity, &self.cache)?;
            self.dirty = false;
        }
        self.last_flush = Instant::now();
        Ok(())
    }

    pub fn handle(&mut self, event: Event) -> Result<()> {
        let generation = match &event {
            Event::Connected { generation, .. }
            | Event::Poll { generation, .. }
            | Event::Inbox { generation, .. }
            | Event::Thread { generation, .. }
            | Event::Reply { generation, .. }
            | Event::Changed { generation, .. }
            | Event::Search { generation, .. }
            | Event::Sync { generation, .. }
            | Event::Synced { generation, .. }
            | Event::Image { generation, .. }
            | Event::Download { generation, .. }
            | Event::AuthUrl { generation, .. } => *generation,
        };
        if generation != self.generation {
            return Ok(());
        }
        match event {
            Event::Connected { result, token, .. } => {
                self.connecting = false;
                match result {
                    Ok((api, identity)) => {
                        if let Some(token) = token {
                            self.store.save_token(&token)?;
                        }
                        // Discard all results from the previous session, including
                        // polls that completed during an account-switching OAuth flow.
                        self.cancel_tasks();
                        let changed = self.identity.id != identity.id
                            || self.identity.workspace != identity.workspace
                            || self.identity.region != identity.region;
                        if changed {
                            self.flush()?;
                            self.cache = self.store.cache(&identity)?;
                            self.drafts = self.store.drafts(&identity)?;
                            self.scheduled = self.store.scheduled(&identity)?;
                            self.view.composer = None;
                            self.notification_seeded = false;
                            self.notification_threads.clear();
                        }
                        self.identity = identity.clone();
                        self.config.region = api.region;
                        self.config.workspace = api.workspace.clone();
                        self.config.identity = identity;
                        self.api = Some(api);
                        self.view.mode = Mode::Inbox;
                        self.auth_url.clear();
                        self.message = if self.identity.id.is_empty() {
                            "No teammate profile: grant Read admins and sign in again to send replies.".into()
                        } else {
                            "Connected. Fetching your inbox…".into()
                        };
                        self.dirty = true;
                        self.poll();
                        self.load_inbox(self.config.filter);
                        let id = self.view.selected.clone();
                        if !id.is_empty() {
                            self.load_thread(&id, false);
                        }
                    }
                    Err(error) => {
                        self.message = error;
                        self.view.mode = Mode::Settings;
                    }
                }
            }
            Event::Poll { result, counts, .. } => {
                self.polling = false;
                self.next_poll = Instant::now() + Duration::from_secs(self.config.refresh_seconds);
                match result {
                    Ok((mut rows, failed)) => {
                        let mut notifications = vec![];
                        let now = Utc::now().timestamp();
                        for (id, (state, until)) in &self.pending_states {
                            if *state == State::Open
                                && now < *until
                                && !rows.iter().any(|row| row.id == *id)
                                && let Some(row) = self.cache.conversations.get(id)
                            {
                                rows.push(row.clone());
                            }
                        }
                        // Overlay successful local state changes over eventually-consistent API results.
                        for row in &mut rows {
                            if let Some((state, until)) = self.pending_states.get(&row.id)
                                && now < *until
                            {
                                row.state = *state;
                                if *state != State::Open {
                                    row.waiting_since = 0;
                                }
                            }
                        }
                        self.cache.reads.reconcile_open(&rows);
                        for row in &rows {
                            if row.complete {
                                if self.notification_seeded {
                                    if let Some(previous) = self.notification_history.get(&row.id) {
                                        for message in
                                            platform::new_customer_messages(previous, row)
                                        {
                                            notifications.push((row.clone(), message));
                                        }
                                    } else if !self.notification_threads.contains(&row.id)
                                        && let Some(message) = row.messages.iter().rev().find(|m| {
                                            m.kind == Kind::Customer
                                                && m.created_at >= self.cache.fetched_at
                                        })
                                    {
                                        notifications.push((row.clone(), message.clone()));
                                    }
                                }
                                self.notification_history
                                    .insert(row.id.clone(), row.clone());
                            }
                            self.notification_threads.insert(row.id.clone());
                        }
                        self.notification_seeded = true;
                        if notifications
                            .iter()
                            .any(|(conversation, _)| conversation.id == self.view.selected)
                        {
                            self.view.follow_end = true;
                        }
                        notifications.sort_by_key(|(_, message)| message.created_at);
                        if self.config.notifications {
                            for (conversation, message) in notifications.into_iter().rev().take(5) {
                                platform::notify(
                                    &conversation,
                                    &message,
                                    self.generation,
                                    self.platform_tx.clone(),
                                );
                            }
                        }
                        self.cache.inboxes.insert(
                            "Open".into(),
                            rows.iter()
                                .filter(|c| c.state == State::Open)
                                .map(|c| c.id.clone())
                                .collect(),
                        );
                        for row in rows {
                            self.merge(row);
                        }
                        if let Ok(counts) = counts {
                            self.cache.counts = counts;
                        }
                        // These are complete inbox results, not a page-size cap.
                        // Prefer their effective state over a racing counts request.
                        let open_ids = &self.cache.inboxes["Open"];
                        self.cache.counts.open = open_ids.len();
                        self.cache.counts.waiting = open_ids
                            .iter()
                            .filter(|id| {
                                self.cache
                                    .conversations
                                    .get(*id)
                                    .is_some_and(|c| c.waiting_since > 0)
                            })
                            .count();
                        self.cache.fetched_at = now;
                        self.cache.version = 1;
                        if self.view.selected.is_empty() {
                            self.view.selected = self
                                .rows()
                                .first()
                                .map(|c| c.id.clone())
                                .unwrap_or_default();
                            self.selection_changed();
                        }
                        self.message = if failed.is_empty() {
                            format!(
                                "Updated {} · {} waiting / {} open",
                                schedule::label(now),
                                self.cache.counts.waiting,
                                self.cache.counts.open
                            )
                        } else {
                            format!(
                                "Inbox updated; {} thread(s) failed to refresh. Cached messages retained.",
                                failed.len()
                            )
                        };
                        self.dirty = true;
                    }
                    Err(error) => self.api_error(error),
                }
            }
            Event::Inbox { filter, result, .. } => {
                self.inbox_loading = false;
                match result {
                    Ok(rows) => {
                        self.cache.inboxes.insert(
                            filter.label().into(),
                            rows.iter().map(|c| c.id.clone()).collect(),
                        );
                        for row in rows {
                            self.merge(row);
                        }
                        if self.config.filter == filter && self.view.selected.is_empty() {
                            self.view.selected = self
                                .rows()
                                .first()
                                .map(|c| c.id.clone())
                                .unwrap_or_default();
                            self.selection_changed();
                        }
                    }
                    Err(error) => self.api_error(error),
                }
                // A filter may have changed while this fetch was running.
                if self.config.filter != filter {
                    self.load_inbox(self.config.filter);
                }
            }
            Event::Thread { id, result, .. } => {
                self.thread_loading.remove(&id);
                match result {
                    Ok(row) => self.merge(row),
                    Err(error) => self.api_error(error),
                }
            }
            Event::Reply {
                id,
                scheduled,
                result,
                ..
            } => {
                self.sending = false;
                match result {
                    Ok(row) => {
                        self.merge(row);
                        if let Some(scheduled) = scheduled {
                            self.scheduled.retain(|s| s.id != scheduled);
                            self.store.save_scheduled(&self.identity, &self.scheduled)?;
                        } else {
                            if let Some(composer) = &mut self.view.composer {
                                composer.ambiguous = true;
                            }
                            self.close_composer(true).context("Message was sent, but the local draft could not be removed. Do not resend")?;
                        }
                        self.message = "Message sent.".into();
                        self.load_thread(&id, true);
                        self.poll();
                    }
                    Err(error) => {
                        if let Some(scheduled) = scheduled {
                            if let Some(row) = self.scheduled.iter_mut().find(|s| s.id == scheduled)
                            {
                                row.status = SendState::NeedsReview(error.to_string());
                            }
                            self.store.save_scheduled(&self.identity, &self.scheduled)?;
                        }
                        if let Some(current) = &error.current {
                            self.merge(*current.clone());
                            if let Some(composer) = &mut self.view.composer {
                                composer.seen = seen(&current.messages);
                            }
                        }
                        if error.ambiguous
                            && let Some(composer) = &mut self.view.composer
                        {
                            composer.ambiguous = true;
                        }
                        self.api_error(error);
                    }
                }
            }
            Event::Changed {
                id, reopen, result, ..
            } => {
                self.sending = false;
                let old_index = self.rows().iter().position(|row| row.id == id).unwrap_or(0);
                match result {
                    Ok(()) => {
                        let state = if reopen { State::Open } else { State::Closed };
                        self.pending_states
                            .insert(id.clone(), (state, Utc::now().timestamp() + 120));
                        if let Some(mut row) = self.cache.conversations.get(&id).cloned() {
                            let previous = row.state;
                            let was_waiting = row.waiting_since > 0;
                            row.state = state;
                            if !reopen {
                                row.waiting_since = 0;
                            }
                            row.updated_at = Utc::now().timestamp();
                            self.merge(row);
                            if previous != state {
                                match previous {
                                    State::Open => {
                                        self.cache.counts.open =
                                            self.cache.counts.open.saturating_sub(1)
                                    }
                                    State::Closed => {
                                        self.cache.counts.closed =
                                            self.cache.counts.closed.saturating_sub(1)
                                    }
                                    State::Snoozed => {
                                        self.cache.counts.snoozed =
                                            self.cache.counts.snoozed.saturating_sub(1)
                                    }
                                }
                                match state {
                                    State::Open => self.cache.counts.open += 1,
                                    State::Closed => self.cache.counts.closed += 1,
                                    State::Snoozed => self.cache.counts.snoozed += 1,
                                }
                                if previous == State::Open && was_waiting {
                                    self.cache.counts.waiting =
                                        self.cache.counts.waiting.saturating_sub(1);
                                }
                            }
                        }
                        for (filter, ids) in &mut self.cache.inboxes {
                            let mode = Filter::ALL.into_iter().find(|f| f.label() == filter);
                            if mode.is_some_and(|f| {
                                f == Filter::All
                                    || (reopen && f == Filter::Open)
                                    || (!reopen && f == Filter::Closed)
                            }) {
                                if !ids.contains(&id) {
                                    ids.insert(0, id.clone());
                                }
                            } else {
                                ids.retain(|value| value != &id);
                            }
                        }
                        self.view.pinned = false;
                        let rows = self.rows();
                        self.view.selected = rows
                            .iter()
                            .find(|c| c.id == id)
                            .or_else(|| rows.get(old_index.min(rows.len().saturating_sub(1))))
                            .map(|c| c.id.clone())
                            .unwrap_or_default();
                        self.selection_changed();
                        self.message = if reopen {
                            "Conversation reopened."
                        } else {
                            "Conversation closed."
                        }
                        .into();
                        self.poll();
                    }
                    Err(error) => self.api_error(error),
                }
            }
            Event::Search {
                request, result, ..
            } => {
                if request != self.search_request {
                    return Ok(());
                }
                self.searching = false;
                match result {
                    Ok(rows) => {
                        let remote = self.view.search_remote;
                        if remote {
                            self.search_hits.clear();
                        }
                        for row in rows {
                            if self.search_hits.len() < 50 && !self.search_hits.contains(&row.id) {
                                self.search_hits.push(row.id.clone());
                            }
                            self.merge(row);
                        }
                        if !self.search_hits.contains(&self.view.selected) {
                            self.view.selected =
                                self.search_hits.first().cloned().unwrap_or_default();
                            self.selection_changed();
                        }
                        self.message = format!(
                            "{} search result(s){}",
                            self.search_hits.len(),
                            if self.search_hits.len() == 50 {
                                " (limit 50)"
                            } else {
                                ""
                            }
                        );
                    }
                    Err(error) => self.message = error,
                }
            }
            Event::Sync {
                scanned,
                total,
                stored,
                skipped,
                failed,
                ..
            } => {
                self.message = format!(
                    "Archive: {scanned}/{total} scanned · {stored} stored · {skipped} closed already stored · {failed} failed"
                );
            }
            Event::Synced { result, .. } => {
                self.syncing = false;
                self.message = result.unwrap_or_else(|e| e);
            }
            Event::AuthUrl { url, .. } => {
                self.auth_url = url;
            }
            Event::Image { url, result, .. } => {
                self.images.entries.insert(
                    url,
                    match result {
                        Ok(image) => media::Preview::Ready(image),
                        Err(error) => media::Preview::Failed(error),
                    },
                );
            }
            Event::Download { result, .. } => {
                self.downloading = false;
                self.message = result.unwrap_or_else(|e| e);
            }
        }
        Ok(())
    }

    fn api_error(&mut self, error: ApiError) {
        self.message = error.to_string();
        if error.kind == ErrorKind::Unauthorized {
            self.api = None;
            self.view.mode = Mode::Settings;
        }
    }

    pub fn confirm(&mut self, action: Confirm) -> Result<()> {
        match action {
            Confirm::Close(id, reopen) => self.change_state(id, reopen)?,
            Confirm::Cancel(id) => self.cancel_scheduled(&id)?,
            Confirm::Discard => self.close_composer(true)?,
            Confirm::SignOut => self.sign_out()?,
            Confirm::Quit => {
                self.flush()?;
                self.quit = true;
            }
            Confirm::RetryAmbiguous => {
                if let Some(composer) = &mut self.view.composer {
                    composer.ambiguous = false;
                }
                self.view.mode = Mode::Compose;
            }
        }
        Ok(())
    }

    fn populate_demo(&mut self) {
        self.identity = Identity {
            id: "demo".into(),
            name: "Demo teammate".into(),
            email: "demo@example.com".into(),
            workspace: "demo".into(),
            region: Region::Us,
        };
        self.cache = Cache::default();
        self.drafts.clear();
        self.scheduled.clear();
        let now = Utc::now().timestamp();
        for (index, title, from, body, state) in [
            (
                1,
                "Help with billing",
                "Ada Lovelace",
                "<p>Hi! Could you help me update the billing address?</p><p>Our new office is in <b>London</b>.</p>",
                State::Open,
            ),
            (
                2,
                "API integration",
                "Grace Hopper",
                "<p>Thanks for your help with the integration.</p><p>Docs: <a href='https://developers.intercom.com'>Intercom API</a></p>",
                State::Open,
            ),
            (
                3,
                "Account access",
                "Alan Turing",
                "<p>That worked. Thank you!</p>",
                State::Closed,
            ),
            (
                4,
                "Follow up next week",
                "Katherine Johnson",
                "<p>Please check back on Monday.</p>",
                State::Snoozed,
            ),
        ] {
            let c = Conversation {
                id: index.to_string(),
                title: title.into(),
                from: from.into(),
                email: "customer@example.com".into(),
                snippet: format::plain(body),
                state,
                waiting_since: if index == 1 { now - 1800 } else { 0 },
                updated_at: now - index * 600,
                complete: true,
                fetched_at: now,
                content_version: CONTENT_VERSION,
                messages: vec![Message {
                    id: format!("m{index}"),
                    from: from.into(),
                    email: "customer@example.com".into(),
                    created_at: now - index * 600,
                    body: body.into(),
                    ..Default::default()
                }],
            };
            self.cache.reads.observe(&c, false, &self.identity);
            self.cache.conversations.insert(c.id.clone(), c);
        }
        self.cache
            .inboxes
            .insert("Open".into(), vec!["1".into(), "2".into()]);
        self.cache.inboxes.insert("Closed".into(), vec!["3".into()]);
        self.cache
            .inboxes
            .insert("Snoozed".into(), vec!["4".into()]);
        self.cache.inboxes.insert(
            "All".into(),
            vec!["1".into(), "2".into(), "3".into(), "4".into()],
        );
        self.cache.counts = Counts {
            open: 2,
            waiting: 1,
            closed: 1,
            snoozed: 1,
        };
        self.view.selected = "1".into();
        self.view.mode = Mode::Inbox;
        self.message =
            "DEMO · No network requests, notifications, or account changes. Press ? for help."
                .into();
        self.config.filter = Filter::Open;
        self.config.zoom = false;
        self.view.zoom = false;
    }
}

impl Drop for App {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn sync_archive(
    api: Api,
    store: Store,
    identity: Identity,
    tx: mpsc::UnboundedSender<Event>,
    generation: u64,
) -> Result<String> {
    let rows = api.list(Filter::All).await?;
    let total = rows.len();
    let mut scanned = 0;
    let mut stored = 0;
    let mut skipped = 0;
    let mut failed = 0;
    let mut jobs = stream::iter(rows.into_iter().map(|row| {
        let api = api.clone();
        let store = store.clone();
        let identity = identity.clone();
        async move {
            if row.state == State::Closed
                && store.archived(&identity, &row.id)?.is_some_and(|c| {
                    c.complete && c.state == State::Closed && c.content_version == CONTENT_VERSION
                })
            {
                return Ok::<_, anyhow::Error>(false);
            }
            let thread = api.thread(&row.id).await?;
            if !thread.complete {
                bail!("Incomplete conversation {}", row.id);
            }
            store.archive(&identity, &thread)?;
            Ok(true)
        }
    }))
    .buffer_unordered(4);
    while let Some(result) = jobs.next().await {
        scanned += 1;
        match result {
            Ok(true) => stored += 1,
            Ok(false) => skipped += 1,
            Err(_) => failed += 1,
        }
        let _ = tx.send(Event::Sync {
            generation,
            scanned,
            total,
            stored,
            skipped,
            failed,
        });
    }
    Ok(format!(
        "Archive sync complete: {stored} downloaded, {skipped} closed already stored, {failed} failed (of {total}).{}",
        if failed > 0 {
            " Run sync again to retry failures."
        } else {
            ""
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Pane;

    fn fixture(root: &std::path::Path) -> App {
        let mut app = App::new(
            Store::open(Some(root)).unwrap(),
            None,
            None,
            mpsc::unbounded_channel().0,
            mpsc::unbounded_channel().0,
            true,
        )
        .unwrap();
        app.demo = false;
        app.config.identity = app.identity.clone();
        app.config.notifications = false;
        app
    }

    #[tokio::test]
    async fn local_state_drafts_schedules_and_layout_survive_restart() {
        let root = tempfile::tempdir().unwrap();
        let mut app = fixture(root.path());
        let identity = app.identity.clone();
        app.open_composer().unwrap();
        app.view
            .composer
            .as_mut()
            .unwrap()
            .editor
            .insert_str("**Persistent** reply");
        app.view.composer.as_mut().unwrap().note = true;
        app.save_composer().unwrap();
        assert!(app.store.drafts(&identity).unwrap()["1"].note);
        app.schedule_composer(Utc::now().timestamp() + 3600)
            .unwrap();
        assert!(!app.drafts.contains_key("1"));
        assert_eq!(app.store.scheduled(&identity).unwrap().len(), 1);
        app.view.zoom = true;
        app.flush().unwrap();
        assert!(app.store.config().unwrap().zoom);
        let queued_id = app.scheduled[0].id.clone();
        app.edit_scheduled(&queued_id).unwrap();
        assert!(app.store.scheduled(&identity).unwrap().is_empty());
        assert!(app.drafts["1"].body.contains("Persistent"));
        app.close_composer(false).unwrap();
        app.flush().unwrap();
        let cache = app.store.cache(&identity).unwrap();
        assert_eq!(cache.conversations.len(), 4);
        assert_eq!(cache.reads.unread("2"), app.cache.reads.unread("2"));
    }

    #[tokio::test]
    async fn attachment_only_drafts_and_schedules_survive_restart_editing_and_failures() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("selected.txt");
        std::fs::write(&path, "attachment").unwrap();
        let mut app = fixture(root.path());
        app.open_composer().unwrap();
        app.add_attachment(path.to_str().unwrap()).unwrap();
        app.add_attachment(path.to_str().unwrap()).unwrap(); // Dedupe the selection.
        app.view.composer.as_mut().unwrap().note = true;
        app.close_composer(false).unwrap();
        let restored = app.store.drafts(&app.identity).unwrap();
        assert!(restored["1"].body.is_empty());
        assert_eq!(restored["1"].attachments, vec![path.to_str().unwrap()]);
        assert!(!restored["1"].is_empty());
        app.open_composer().unwrap();
        assert_eq!(
            app.view.composer.as_ref().unwrap().attachments,
            restored["1"].attachments
        );
        app.schedule_composer(Utc::now().timestamp() + 3600)
            .unwrap();
        let queued = app.store.scheduled(&app.identity).unwrap();
        assert_eq!(queued[0].attachments, restored["1"].attachments);
        assert!(queued[0].note);
        app.open_composer().unwrap();
        app.add_attachment(path.to_str().unwrap()).unwrap();
        app.close_composer(false).unwrap();
        assert!(app.edit_scheduled(&queued[0].id).is_err()); // Don't overwrite a file-only draft.
        app.close_composer(true).unwrap();
        app.edit_scheduled(&queued[0].id).unwrap();
        assert_eq!(
            app.view.composer.as_ref().unwrap().attachments,
            restored["1"].attachments
        );
        app.handle(Event::Reply {
            generation: 0,
            id: "1".into(),
            scheduled: None,
            result: Err(ApiError {
                kind: ErrorKind::Other,
                message: "file missing".into(),
                current: None,
                ambiguous: false,
            }),
        })
        .unwrap();
        assert_eq!(
            app.view.composer.as_ref().unwrap().attachments,
            restored["1"].attachments
        );
        app.close_composer(false).unwrap();
        assert_eq!(
            app.store.drafts(&app.identity).unwrap()["1"].attachments,
            restored["1"].attachments
        );
    }

    #[tokio::test]
    async fn archive_sync_upgrades_old_closed_records_once_and_searches_media_metadata() {
        use serde_json::json;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let root = tempfile::tempdir().unwrap();
        let app = fixture(root.path());
        app.store
            .archive(
                &app.identity,
                &Conversation {
                    id: "closed".into(),
                    state: State::Closed,
                    complete: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/conversations/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"conversations":[{"id":"closed", "state":"closed"}]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/conversations/closed")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"closed", "state":"closed", "source":{"attachments":[{"url":"https://example.org/diagram.png", "name":"Workflow.png", "alt":"Regression diagram", "content_type":"image/png"}]}}))).expect(1).mount(&server).await;
        let api = Api::mock(server.uri());
        for _ in 0..2 {
            sync_archive(
                api.clone(),
                app.store.clone(),
                app.identity.clone(),
                mpsc::unbounded_channel().0,
                0,
            )
            .await
            .unwrap();
        }
        let hits = app
            .store
            .search_archive(&app.identity, "workflow regression")
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content_version, CONTENT_VERSION);
        assert!(matches!(
            hits[0].messages[0].blocks[0],
            ContentBlock::Image(_)
        ));
    }

    #[tokio::test]
    async fn partial_rows_enrich_old_caches_without_stripping_media_or_claiming_full_upgrade() {
        let root = tempfile::tempdir().unwrap();
        let mut app = fixture(root.path());
        let mut old = app.cache.conversations["1"].clone();
        old.content_version = 0;
        app.cache.conversations.insert("1".into(), old.clone());
        let mut fresh = old;
        fresh.complete = false;
        fresh.content_version = CONTENT_VERSION;
        fresh.messages[0].blocks = vec![ContentBlock::Image(Media {
            url: "https://example.org/image?token=current".into(),
            ..Default::default()
        })];
        app.merge(fresh);
        assert!(matches!(
            app.cache.conversations["1"].messages[0].blocks[0],
            ContentBlock::Image(_)
        ));
        assert_eq!(app.cache.conversations["1"].content_version, 0);
        let mut summary = app.cache.conversations["1"].clone();
        summary.complete = false;
        summary.content_version = CONTENT_VERSION;
        summary.messages[0].blocks = vec![ContentBlock::Text {
            html: "summary without media".into(),
        }];
        app.merge(summary);
        assert!(matches!(
            app.cache.conversations["1"].messages[0].blocks[0],
            ContentBlock::Image(_)
        ));
    }

    #[tokio::test]
    async fn missing_scheduled_attachments_block_the_entire_send_and_require_review() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("scheduled.txt");
        std::fs::write(&path, "content").unwrap();
        let mut app = fixture(root.path());
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.tx = tx;
        let server = wiremock::MockServer::start().await;
        app.api = Some(Api::mock(server.uri()));
        app.open_composer().unwrap();
        app.add_attachment(path.to_str().unwrap()).unwrap();
        app.view
            .composer
            .as_mut()
            .unwrap()
            .editor
            .insert_str("Keep text with its attachment");
        app.schedule_composer(Utc::now().timestamp() + 3600)
            .unwrap();
        app.scheduled[0].send_at = 1;
        std::fs::remove_file(&path).unwrap();
        app.send_due().unwrap();
        assert!(matches!(
            app.store.scheduled(&app.identity).unwrap()[0].status,
            SendState::NeedsReview(_)
        )); // Sending is crash-recovered as review.
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.handle(event).unwrap();
        let scheduled = app.store.scheduled(&app.identity).unwrap();
        assert!(
            matches!(&scheduled[0].status, SendState::NeedsReview(error) if error.contains("Cannot read attachment"))
        );
        assert_eq!(scheduled[0].attachments, vec![path.to_str().unwrap()]);
        assert!(!app.sending);
        app.send_due().unwrap();
        assert!(!app.sending);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn poll_baseline_is_independent_of_foreground_thread_loads() {
        let root = tempfile::tempdir().unwrap();
        let mut app = fixture(root.path());
        app.focused = false;
        let original = app.cache.conversations["1"].clone();
        let mut newer = original.clone();
        newer.messages.push(Message {
            id: "follow-up".into(),
            created_at: Utc::now().timestamp(),
            body: "Another question".into(),
            ..Default::default()
        });
        let poll = |row: Conversation| Event::Poll {
            generation: 0,
            result: Ok((vec![row], vec![])),
            counts: Ok(Counts {
                open: 1,
                waiting: 1,
                ..Default::default()
            }),
        };
        app.handle(poll(original.clone())).unwrap();
        app.handle(Event::Thread {
            generation: 0,
            id: "1".into(),
            result: Ok(newer.clone()),
        })
        .unwrap();
        assert_eq!(
            app.notification_history["1"].messages.len(),
            original.messages.len()
        );
        assert_eq!(
            platform::new_customer_messages(&app.notification_history["1"], &newer).len(),
            1
        );
        app.handle(poll(newer.clone())).unwrap();
        assert_eq!(
            app.notification_history["1"].messages.len(),
            newer.messages.len()
        );
        assert!(app.cache.reads.message_unread("1", "follow-up"));
        app.focused = true;
        app.mark_viewed();
        assert!(!app.cache.reads.unread("1"));
    }

    #[tokio::test]
    async fn stale_sessions_and_eventually_consistent_state_cannot_resurrect_closed_threads() {
        let root = tempfile::tempdir().unwrap();
        let mut app = fixture(root.path());
        let old = app.cache.conversations["1"].clone();
        app.handle(Event::Changed {
            generation: 0,
            id: "1".into(),
            reopen: false,
            result: Ok(()),
        })
        .unwrap();
        app.merge(old.clone());
        assert_eq!(app.cache.conversations["1"].state, State::Closed);
        assert!(!app.cache.reads.unread("1"));
        app.generation += 1;
        app.handle(Event::Thread {
            generation: 0,
            id: "1".into(),
            result: Ok(old),
        })
        .unwrap();
        assert_eq!(app.cache.conversations["1"].state, State::Closed);
    }

    #[tokio::test]
    async fn scheduled_failure_is_journaled_and_never_automatically_retried() {
        let root = tempfile::tempdir().unwrap();
        let mut app = fixture(root.path());
        app.open_composer().unwrap();
        app.view
            .composer
            .as_mut()
            .unwrap()
            .editor
            .insert_str("Later reply");
        app.schedule_composer(Utc::now().timestamp() + 60).unwrap();
        let id = app.scheduled[0].id.clone();
        let error = ApiError {
            kind: ErrorKind::Network,
            message: "No send confirmation".into(),
            current: None,
            ambiguous: true,
        };
        app.handle(Event::Reply {
            generation: 0,
            id: "1".into(),
            scheduled: Some(id),
            result: Err(error),
        })
        .unwrap();
        let restored = app.store.scheduled(&app.identity).unwrap();
        assert!(matches!(restored[0].status, SendState::NeedsReview(_)));
        assert!(!restored[0].due(Utc::now().timestamp() + 120));
    }

    #[tokio::test]
    async fn archive_sync_skips_stored_closed_threads() {
        use serde_json::json;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        let root = tempfile::tempdir().unwrap();
        let app = fixture(root.path());
        let closed = Conversation {
            id: "closed".into(),
            state: State::Closed,
            complete: true,
            content_version: CONTENT_VERSION,
            ..Default::default()
        };
        app.store.archive(&app.identity, &closed).unwrap();
        Mock::given(method("POST")).and(path("/conversations/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"conversations":[{"id":"closed","state":"closed"},{"id":"open","state":"open"}]}))).mount(&server).await;
        Mock::given(method("GET")).and(path("/conversations/open"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"open","state":"open","source":{"body":"Full text","author":{"type":"user"}}}))).expect(1).mount(&server).await;
        Mock::given(path("/conversations/closed"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let summary = sync_archive(
            Api::mock(server.uri()),
            app.store.clone(),
            app.identity.clone(),
            mpsc::unbounded_channel().0,
            0,
        )
        .await
        .unwrap();
        assert!(summary.contains("1 downloaded, 1 closed already stored, 0 failed"));
        assert!(
            app.store
                .archived(&app.identity, "open")
                .unwrap()
                .unwrap()
                .complete
        );
    }

    #[tokio::test]
    async fn typing_during_send_cannot_change_or_erase_new_draft_text() {
        use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
        let root = tempfile::tempdir().unwrap();
        let mut app = fixture(root.path());
        app.open_composer().unwrap();
        app.view
            .composer
            .as_mut()
            .unwrap()
            .editor
            .insert_str("Submitted text");
        app.sending = true;
        crate::ui::input(&mut app, InputEvent::Paste("lost edit".into())).unwrap();
        crate::ui::input(
            &mut app,
            InputEvent::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)),
        )
        .unwrap();
        assert_eq!(app.view.composer.as_ref().unwrap().body(), "Submitted text");
        assert!(!app.view.composer.as_ref().unwrap().note);
        app.view.pane = Pane::Thread;
    }

    #[tokio::test]
    async fn leaving_search_invalidates_delayed_results() {
        let root = tempfile::tempdir().unwrap();
        let mut app = fixture(root.path());
        app.view.search_active = true;
        app.search_changed();
        let request = app.search_request;
        app.cancel_search();
        let selected = app.view.selected.clone();
        app.handle(Event::Search {
            generation: 0,
            request,
            result: Ok(vec![Conversation {
                id: "unwanted".into(),
                ..Default::default()
            }]),
        })
        .unwrap();
        assert_eq!(app.view.selected, selected);
        assert!(!app.cache.conversations.contains_key("unwanted"));
    }

    #[tokio::test]
    async fn archive_hits_cannot_roll_live_status_or_messages_back() {
        let root = tempfile::tempdir().unwrap();
        let mut app = fixture(root.path());
        let original = app.cache.conversations["1"].clone();
        let mut old = original.clone();
        old.state = State::Closed;
        old.updated_at -= 100;
        old.messages.clear();
        app.merge(old);
        assert_eq!(app.cache.conversations["1"].state, State::Open);
        assert_eq!(app.cache.conversations["1"].messages, original.messages);
    }
}
