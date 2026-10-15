use crate::{format, media, model::*};
use futures::{StreamExt, stream};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::Duration,
};

pub const API_VERSION: &str = "2.14";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Network,
    Unauthorized,
    Permission,
    Conflict,
    Other,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub kind: ErrorKind,
    pub message: String,
    pub current: Option<Box<Conversation>>,
    /// A write may have succeeded even though no response was received.
    pub ambiguous: bool,
}

impl ApiError {
    fn other(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Other,
            message: message.into(),
            current: None,
            ambiguous: false,
        }
    }
}

type Result<T> = std::result::Result<T, ApiError>;

#[derive(Clone)]
pub struct Api {
    client: Client,
    token: Arc<str>,
    pub region: Region,
    pub workspace: String,
    base_override: Option<String>,
}

impl Api {
    pub fn new(token: String, region: Region, workspace: String) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("intercom-tui/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| ApiError::other("Cannot initialize HTTPS client"))?;
        Ok(Self {
            client,
            token: token.into(),
            region,
            workspace,
            base_override: None,
        })
    }

    pub async fn connect(
        token: String,
        preferred: Region,
        workspace: String,
    ) -> Result<(Self, Identity)> {
        let mut last = ApiError::other("Could not find your Intercom region");
        for region in preferred.order() {
            let mut api = Self::new(token.clone(), region, workspace.clone())?;
            match api
                .request(
                    Method::POST,
                    "/conversations/search",
                    Some(&search_body(Filter::Open, 1, None)),
                    true,
                )
                .await
            {
                Ok(payload) => {
                    let mut identity = match api.me().await {
                        Ok(identity) => identity,
                        Err(error) if error.kind == ErrorKind::Permission => Identity {
                            region,
                            ..Default::default()
                        },
                        Err(error) => return Err(error),
                    };
                    if identity.workspace.is_empty() {
                        identity.workspace = workspace_id(&workspace)
                            .or_else(|| payload_workspace(&payload))
                            .unwrap_or_default();
                    }
                    if identity.workspace.is_empty() {
                        identity.workspace = api.discover_workspace().await.unwrap_or_default();
                    }
                    api.workspace = identity.workspace.clone();
                    api.region = identity.region;
                    return Ok((api, identity));
                }
                Err(error) if error.kind == ErrorKind::Unauthorized => last = error,
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }

    fn base(&self) -> &str {
        self.base_override.as_deref().unwrap_or(self.region.api())
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        safe_retry: bool,
    ) -> Result<Value> {
        self.request_version(method, path, body, safe_retry, API_VERSION)
            .await
    }

    async fn request_version(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        safe_retry: bool,
        version: &str,
    ) -> Result<Value> {
        let mut attempt = 0;
        loop {
            let mut request = self
                .client
                .request(method.clone(), format!("{}{path}", self.base()))
                .bearer_auth(self.token.as_ref())
                .header("Accept", "application/json")
                .header("Intercom-Version", version);
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request.send().await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    if safe_retry && attempt < 2 {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
                        continue;
                    }
                    return Err(ApiError {
                        kind: ErrorKind::Network,
                        message: if safe_retry {
                            "Cannot reach Intercom. Your cached data is still available.".into()
                        } else {
                            "No send confirmation received. Check Intercom before retrying; the operation may have succeeded.".into()
                        },
                        ambiguous: !safe_retry && !error.is_connect(),
                        current: None,
                    });
                }
            };
            let status = response.status();
            if safe_retry
                && attempt < 2
                && (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
            {
                let delay = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1 << attempt)
                    .min(30);
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(delay)).await;
                continue;
            }
            let payload: Value = response.json().await.map_err(|_| ApiError {
                kind: match status.as_u16() {
                    401 => ErrorKind::Unauthorized,
                    403 => ErrorKind::Permission,
                    _ => ErrorKind::Other,
                },
                message: match status.as_u16() {
                    401 => "Intercom rejected the token. Sign in again.",
                    403 => "Intercom token is missing a required permission.",
                    _ => "Intercom returned an incomplete or invalid response",
                }
                .into(),
                current: None,
                ambiguous: !safe_retry,
            })?;
            if !status.is_success() {
                let code = payload
                    .pointer("/errors/0/code")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let detail = payload
                    .pointer("/errors/0/message")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let kind = if code == "token_unauthorized"
                    || detail.contains("Not authorized to access resource")
                    || status == StatusCode::FORBIDDEN
                {
                    ErrorKind::Permission
                } else if status == StatusCode::UNAUTHORIZED {
                    ErrorKind::Unauthorized
                } else {
                    ErrorKind::Other
                };
                let message = match kind {
                    ErrorKind::Permission => "Missing Intercom permission. Grant Read conversations, Write conversations, and Read admins, then sign in again.".into(),
                    ErrorKind::Unauthorized => "Intercom rejected the token. Sign in again (the app may have been revoked).".into(),
                    _ => if detail.is_empty() { format!("Intercom request failed ({status})") } else { format::preview(detail, 500) },
                };
                return Err(ApiError {
                    kind,
                    message,
                    current: None,
                    ambiguous: !safe_retry && status.is_server_error(),
                });
            }
            if !payload.is_object() {
                return Err(ApiError::other("Intercom returned invalid JSON"));
            }
            return Ok(payload);
        }
    }

    pub async fn me(&self) -> Result<Identity> {
        let payload = self.request(Method::GET, "/me", None, true).await?;
        let id = text(&payload["id"]);
        if id.is_empty() {
            return Err(ApiError {
                kind: ErrorKind::Permission,
                message:
                    "No teammate profile. Grant Read admins and sign in again to reply as yourself."
                        .into(),
                current: None,
                ambiguous: false,
            });
        }
        Ok(Identity {
            id,
            name: text(&payload["name"]),
            email: text(&payload["email"]),
            workspace: workspace_id(&text(&payload["app"]["id_code"])).unwrap_or_default(),
            region: match text(&payload["app"]["region"]).to_uppercase().as_str() {
                "EU" => Region::Eu,
                "AU" => Region::Au,
                _ => self.region,
            },
        })
    }

    async fn discover_workspace(&self) -> Option<String> {
        let payload = self
            .request_version(
                Method::POST,
                "/conversations/search",
                Some(&search_body(Filter::Waiting, 8, None)),
                true,
                "Unstable",
            )
            .await
            .ok()?;
        if let Some(workspace) = payload_workspace(&payload) {
            return Some(workspace);
        }
        for row in payload["conversations"]
            .as_array()
            .into_iter()
            .flatten()
            .take(8)
        {
            let id = text(&row["id"]);
            if !safe_id(&id) {
                continue;
            }
            if let Ok(payload) = self
                .request_version(
                    Method::GET,
                    &format!("/conversations/{id}"),
                    None,
                    true,
                    "Unstable",
                )
                .await
                && let Some(workspace) = payload_workspace(&payload)
            {
                return Some(workspace);
            }
        }
        None
    }

    pub async fn list(&self, filter: Filter) -> Result<Vec<Conversation>> {
        let payloads = self.list_raw(filter).await?;
        let mut rows: Vec<_> = payloads
            .iter()
            .filter_map(normalize)
            .filter(|row| filter.matches(row))
            .collect();
        if filter == Filter::Snoozed {
            rows =
                stream::iter(rows.into_iter().map(|summary| async move {
                    self.thread(&summary.id).await.unwrap_or(summary)
                }))
                .buffer_unordered(6)
                .collect()
                .await;
        }
        rows.sort_by_key(|c| (std::cmp::Reverse(c.updated_at), c.id.clone()));
        Ok(rows)
    }

    async fn list_raw(&self, filter: Filter) -> Result<Vec<Value>> {
        let mut cursor = None;
        let mut visited = HashSet::new();
        let mut rows = BTreeMap::new();
        loop {
            let payload = self
                .request(
                    Method::POST,
                    "/conversations/search",
                    Some(&search_body(filter, 150, cursor.as_deref())),
                    true,
                )
                .await?;
            let page = payload["conversations"]
                .as_array()
                .ok_or_else(|| ApiError::other("Intercom omitted the conversation list"))?;
            for row in page {
                let id = text(&row["id"]);
                if safe_id(&id) {
                    rows.insert(id, row.clone());
                }
            }
            cursor = next_cursor(&payload);
            match &cursor {
                None => break,
                Some(cursor) if !visited.insert(cursor.clone()) => {
                    return Err(ApiError::other(
                        "Intercom repeated an inbox pagination cursor. Partial results were not applied.",
                    ));
                }
                _ => (),
            }
        }
        Ok(rows.into_values().collect())
    }

    pub async fn count(&self, filter: Filter) -> Result<usize> {
        let payload = self
            .request(
                Method::POST,
                "/conversations/search",
                Some(&search_body(filter, 1, None)),
                true,
            )
            .await?;
        Ok(payload["total_count"].as_u64().unwrap_or(0) as usize)
    }

    pub async fn counts(&self) -> Result<Counts> {
        let (open, waiting, closed, snoozed) = tokio::try_join!(
            self.count(Filter::Open),
            self.count(Filter::Waiting),
            self.count(Filter::Closed),
            self.count(Filter::Snoozed)
        )?;
        Ok(Counts {
            open,
            waiting,
            closed,
            snoozed,
        })
    }

    pub async fn thread(&self, id: &str) -> Result<Conversation> {
        if !safe_id(id) {
            return Err(ApiError::other("Invalid conversation ID"));
        }
        let path = format!("/conversations/{id}");
        let mut payload = self.request(Method::GET, &path, None, true).await?;
        let mut parts = conversation_parts(&payload);
        let mut cursor = parts_cursor(&payload);
        let total = payload
            .pointer("/conversation_parts/total_count")
            .and_then(Value::as_u64)
            .unwrap_or(parts.len() as u64) as usize;
        let mut visited = HashSet::new();
        let mut ids: HashSet<_> = parts.iter().map(|p| text(&p["id"])).collect();
        while let Some(current) = cursor {
            if !visited.insert(current.clone()) {
                return Err(ApiError::other(
                    "Intercom repeated a message pagination cursor",
                ));
            }
            let query = reqwest::Url::parse_with_params(
                "http://localhost",
                &[("starting_after", current.as_str())],
            )
            .unwrap();
            let extra = self
                .request(
                    Method::GET,
                    &format!("{path}?{}", query.query().unwrap()),
                    None,
                    true,
                )
                .await?;
            for part in conversation_parts(&extra) {
                let id = text(&part["id"]);
                if id.is_empty() || ids.insert(id) {
                    parts.push(part);
                }
            }
            cursor = parts_cursor(&extra);
        }
        let complete = parts.len() >= total;
        payload["conversation_parts"] = json!({"conversation_parts": parts});
        let mut conversation = normalize(&payload)
            .ok_or_else(|| ApiError::other("Intercom returned an invalid conversation"))?;
        if conversation.id != id {
            return Err(ApiError::other(
                "Intercom returned a different conversation ID",
            ));
        }
        conversation.complete = complete;
        conversation.fetched_at = chrono::Utc::now().timestamp();
        Ok(conversation)
    }

    /// Poll every open thread, including ones already waiting, so same-timestamp
    /// customer follow-ups and other teammates' messages remain discoverable.
    pub async fn poll(&self) -> Result<(Vec<Conversation>, Vec<String>)> {
        let summaries = self.list(Filter::Open).await?;
        let results = stream::iter(summaries.into_iter().map(|summary| async move {
            match self.thread(&summary.id).await {
                Ok(thread) => Ok(thread),
                Err(error) => Err((summary, error)),
            }
        }))
        .buffer_unordered(6)
        .collect::<Vec<_>>()
        .await;
        let mut rows = Vec::new();
        let mut failures = Vec::new();
        for result in results {
            match result {
                Ok(row) => rows.push(row),
                Err((row, error)) => {
                    if matches!(error.kind, ErrorKind::Unauthorized | ErrorKind::Permission) {
                        return Err(error);
                    }
                    failures.push(row.id.clone());
                    rows.push(row);
                }
            }
        }
        rows.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        Ok((rows, failures))
    }

    pub async fn search(&self, query: &str) -> Result<Vec<Conversation>> {
        let mut clauses = vec![
            json!({"field":"source.author.name","operator":"~","value":query}),
            json!({"field":"source.author.email","operator":"~","value":query}),
            json!({"field":"source.subject","operator":"~","value":query}),
        ];
        let words: Vec<_> = query
            .split_whitespace()
            .take(6)
            .map(|word| json!({"field":"source.body","operator":"=","value":word.to_lowercase()}))
            .collect();
        if !words.is_empty() {
            clauses.push(json!({"operator":"AND","value":words}));
        }
        let body = json!({"query":{"operator":"OR","value":clauses},"pagination":{"per_page":50},"sort":{"field":"updated_at","order":"descending"}});
        let payload = self
            .request(Method::POST, "/conversations/search", Some(&body), true)
            .await?;
        Ok(payload["conversations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(normalize)
            .take(50)
            .collect())
    }

    pub async fn reply(
        &self,
        id: &str,
        markdown: &str,
        note: bool,
        seen: &[SeenMessage],
    ) -> Result<Conversation> {
        self.reply_with_attachments(id, markdown, note, seen, &[])
            .await
    }

    pub async fn reply_with_attachments(
        &self,
        id: &str,
        markdown: &str,
        note: bool,
        seen: &[SeenMessage],
        attachments: &[String],
    ) -> Result<Conversation> {
        if !safe_id(id) || markdown.trim().is_empty() && attachments.is_empty() {
            return Err(ApiError::other(
                "Select a conversation and write a message or attach a file first",
            ));
        }
        let attachments = attachments.to_vec();
        let files = tokio::task::spawn_blocking(move || media::upload_files(&attachments))
            .await
            .map_err(|_| ApiError::other("Cannot read attachments; message was not sent"))?
            .map_err(|error| ApiError::other(error.to_string()))?;
        let identity = self.me().await?;
        let fresh = self.thread(id).await?;
        if !fresh.complete {
            return Err(ApiError::other(
                "The full thread could not be loaded. Reply was not sent.",
            ));
        }
        if has_new_comments(&fresh.messages, seen) {
            return Err(ApiError { kind: ErrorKind::Conflict, message: "New comments or notes arrived; reply was not sent. Review the updated thread, then send again.".into(), current: Some(Box::new(fresh)), ambiguous: false });
        }
        let mut body = json!({"message_type":if note {"note"} else {"comment"},"type":"admin","admin_id":identity.id,"body":format::markdown(markdown)});
        if !files.is_empty() {
            body["attachment_files"] = json!(files);
        }
        // Never retry a write, or fall through to a different region after 404.
        let payload = self
            .request(
                Method::POST,
                &format!("/conversations/{id}/reply"),
                Some(&body),
                false,
            )
            .await?;
        let mut conversation = normalize(&payload).unwrap_or(fresh);
        if conversation.id != id {
            conversation = self.thread(id).await.map_err(|_| ApiError { ambiguous: true, ..ApiError::other("Reply was accepted, but the updated thread could not be loaded. Do not resend.") })?;
        }
        conversation.complete = false; // The write response may contain only the new part.
        Ok(conversation)
    }

    pub async fn change_state(&self, id: &str, reopen: bool) -> Result<()> {
        if !safe_id(id) {
            return Err(ApiError::other("Invalid conversation ID"));
        }
        let identity = self.me().await?;
        self.request(Method::POST, &format!("/conversations/{id}/parts"), Some(&json!({"message_type":if reopen {"open"} else {"close"},"type":"admin","admin_id":identity.id})), false).await?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn mock(base: String) -> Self {
        let mut api = Self::new("test-token".into(), Region::Us, "test".into()).unwrap();
        api.base_override = Some(base);
        api
    }
}

pub fn search_body(filter: Filter, per_page: usize, cursor: Option<&str>) -> Value {
    let clause = |state| json!({"field":"state","operator":"=","value":state});
    let query = match filter {
        Filter::All => {
            json!({"operator":"OR","value":[clause("open"),clause("closed"),clause("snoozed")]})
        }
        Filter::Closed => json!({"operator":"AND","value":[clause("closed")]}),
        Filter::Snoozed => json!({"operator":"AND","value":[clause("snoozed")]}),
        Filter::Open => {
            json!({"operator":"AND","value":[{"field":"open","operator":"=","value":true},clause("open")]})
        }
        Filter::Waiting => {
            json!({"operator":"AND","value":[{"field":"open","operator":"=","value":true},clause("open"),{"field":"waiting_since","operator":">","value":0}]})
        }
    };
    let mut body = json!({"query":query,"pagination":{"per_page":per_page.clamp(1,150)},"sort":{"field":"updated_at","order":"descending"}});
    if let Some(cursor) = cursor {
        body["pagination"]["starting_after"] = json!(cursor);
    }
    body
}

pub fn text(value: &Value) -> String {
    match value {
        Value::String(text) => format::safe_text(text),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn timestamp(value: &Value) -> i64 {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
        .unwrap_or(0)
        .max(0)
}

fn next_cursor(payload: &Value) -> Option<String> {
    let next = &payload["pages"]["next"];
    let cursor = if next.is_object() {
        text(&next["starting_after"]).or_else_text(|| text(&next["starting_after_id"]))
    } else {
        text(next)
    };
    (!cursor.is_empty()).then_some(cursor)
}

fn parts_cursor(payload: &Value) -> Option<String> {
    next_cursor(&payload["conversation_parts"])
}

fn payload_workspace(payload: &Value) -> Option<String> {
    workspace_id(&text(&payload["company"]["app_id"])).or_else(|| {
        payload["conversations"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|c| workspace_id(&text(&c["company"]["app_id"])))
    })
}

fn conversation_parts(payload: &Value) -> Vec<Value> {
    let parts = &payload["conversation_parts"];
    parts
        .as_array()
        .or_else(|| parts["conversation_parts"].as_array())
        .cloned()
        .unwrap_or_default()
}

trait FallbackText {
    fn or_else_text(self, fallback: impl FnOnce() -> String) -> String;
}
impl FallbackText for String {
    fn or_else_text(self, fallback: impl FnOnce() -> String) -> String {
        if self.is_empty() { fallback() } else { self }
    }
}

fn author_kind(author: &Value) -> Kind {
    match text(&author["type"]).to_lowercase().as_str() {
        "admin" | "team" => Kind::Teammate,
        "bot" | "operator" | "ai_agent" => Kind::Bot,
        _ => Kind::Customer,
    }
}

fn author_label(author: &Value) -> String {
    text(&author["name"])
        .or_else_text(|| text(&author["email"]))
        .or_else_text(|| {
            match author_kind(author) {
                Kind::Teammate => "Teammate",
                Kind::Bot => "Bot",
                _ => "Customer",
            }
            .into()
        })
}

pub fn normalize(payload: &Value) -> Option<Conversation> {
    let id = text(&payload["id"]);
    if !safe_id(&id) {
        return None;
    }
    let now = chrono::Utc::now().timestamp();
    let state = match text(&payload["state"]).as_str() {
        "closed" => State::Closed,
        "snoozed" => State::Snoozed,
        _ if timestamp(&payload["snoozed_until"]) > now => State::Snoozed,
        _ if payload["open"] == false => State::Closed,
        _ => State::Open,
    };
    let mut messages = Vec::new();
    let mut ids = HashSet::new();
    let mut add = |part: &Value, source: bool| {
        let author = &part["author"];
        let part_type = text(&part["part_type"]);
        let is_note = matches!(part_type.as_str(), "note" | "note_and_reopen");
        if !source
            && !matches!(
                part_type.as_str(),
                "note"
                    | "note_and_reopen"
                    | "comment"
                    | "open"
                    | "close"
                    | "assignment"
                    | "assign_and_reopen"
                    | "assign_and_unsnooze"
            )
        {
            return;
        }
        if matches!(
            part_type.as_str(),
            "assignment" | "assign_and_reopen" | "assign_and_unsnooze"
        ) && author_kind(author) == Kind::Bot
        {
            return;
        }
        let mut body = format::message_body(part["body"].as_str().unwrap_or(""));
        let blocks = media::content(&body, &part["attachments"]);
        let plain = Message {
            body: body.clone(),
            blocks: blocks.clone(),
            ..Default::default()
        }
        .plain();
        if plain.trim().is_empty() {
            return;
        }
        if format::plain(&body).trim().is_empty() {
            body = format::escape(&plain).replace('\n', "<br>");
        }
        let created_at = if source {
            timestamp(&part["created_at"]).max(timestamp(&payload["created_at"]))
        } else {
            timestamp(&part["created_at"])
        };
        let id = text(&part["id"]).or_else_text(|| {
            if source {
                "source".into()
            } else {
                let keys = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Image(m) | ContentBlock::Attachment(m) => {
                            Some(media::identity(&m.url))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("|");
                if keys.is_empty() {
                    // Keep pre-media cache/read-state identities for text-only history.
                    format!("{created_at}:{}", format::preview(&body, 80))
                } else {
                    format!("{created_at}:{}:{keys}", format::preview(&plain, 80))
                }
            }
        });
        if !ids.insert(id.clone()) {
            return;
        }
        messages.push(Message {
            id,
            from: author_label(author),
            email: text(&author["email"]),
            author_id: text(&author["id"]),
            kind: if is_note {
                Kind::Note
            } else {
                author_kind(author)
            },
            created_at,
            body,
            blocks,
        });
    };
    add(&payload["source"], true);
    for part in conversation_parts(payload) {
        add(&part, false);
    }
    messages.sort_by_key(|m| (m.created_at, m.id.clone()));
    let latest = messages.iter().rev().find(|m| m.kind != Kind::Note);
    let title = text(&payload["title"])
        .or_else_text(|| text(&payload["source"]["subject"]))
        .or_else_text(|| author_label(&payload["source"]["author"]));
    Some(Conversation {
        id,
        title,
        from: latest.map(|m| m.from.clone()).unwrap_or_default(),
        email: latest.map(|m| m.email.clone()).unwrap_or_default(),
        snippet: latest
            .map(|m| format::preview(&m.plain(), 240))
            .unwrap_or_default(),
        state,
        waiting_since: if state == State::Open {
            timestamp(&payload["waiting_since"])
        } else {
            0
        },
        updated_at: timestamp(&payload["updated_at"]).max(timestamp(&payload["created_at"])),
        messages,
        complete: false,
        fetched_at: 0,
        content_version: CONTENT_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method, path, query_param},
    };

    fn conversation() -> Value {
        json!({"id":"1","state":"open","waiting_since":10,"source":{"id":"source","author":{"type":"user","name":"Ada"},"body":"Help","created_at":10},"conversation_parts":{"total_count":1,"conversation_parts":[{"id":"note","part_type":"note","created_at":20,"author":{"type":"admin","id":"other"},"body":"Internal"}]}})
    }

    #[test]
    fn image_and_attachment_only_messages_are_searchable_and_have_stable_ids() {
        let mut payload = json!({"id":"1", "source":{"id":"source", "body":"<a href='https://example.org/full'><img src='https://example.org/screenshot.png' alt='Billing problem'></a>"}, "conversation_parts":{"conversation_parts":[
            {"part_type":"comment", "created_at":20, "attachments":[{"name":"invoice.pdf", "content_type":"application/pdf", "url":"https://example.org/invoice?token=old"}]},
            {"id":"private", "part_type":"note", "created_at":30, "attachments":[{"name":"internal.png", "url":"https://example.org/internal.png"}]}
        ]}});
        let old = normalize(&payload).unwrap();
        assert_eq!(old.messages.len(), 3);
        assert_eq!(old.messages[0].plain(), "Image: Billing problem");
        assert!(matches!(old.messages[0].blocks[0], ContentBlock::Image(_)));
        assert!(old.haystack().contains("billing problem"));
        assert!(old.haystack().contains("invoice.pdf"));
        assert!(old.snippet.contains("invoice.pdf"));
        assert_eq!(old.messages[2].kind, Kind::Note);
        payload["conversation_parts"]["conversation_parts"][0]["attachments"][0]["url"] =
            json!("https://example.org/invoice?token=new");
        let new = normalize(&payload).unwrap();
        assert_eq!(old.messages[1].id, new.messages[1].id);
        assert!(!has_new_comments(&new.messages, &seen(&old.messages)));
        assert_eq!(new.content_version, CONTENT_VERSION);
        let text_only = normalize(&json!({"id":"2", "conversation_parts":[{"part_type":"comment", "body":"<p>Original message</p>", "created_at":40}]})).unwrap();
        assert_eq!(text_only.messages[0].id, "40:<p>Original message</p>");
    }

    #[tokio::test]
    async fn uploads_files_with_text_optional_for_public_replies_and_private_notes() {
        use base64::Engine;
        let root = tempfile::tempdir().unwrap();
        let attachment = root.path().join("report.txt");
        std::fs::write(&attachment, "selected file").unwrap();
        let server = MockServer::start().await;
        Mock::given(path("/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"me"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/conversations/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(conversation()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/conversations/1/reply"))
            .respond_with(ResponseTemplate::new(200).set_body_json(conversation()))
            .expect(2)
            .mount(&server)
            .await;
        let paths = vec![attachment.to_string_lossy().into_owned()];
        let api = Api::mock(server.uri());
        let snapshot = seen(&normalize(&conversation()).unwrap().messages);
        for note in [false, true] {
            api.reply_with_attachments("1", "", note, &snapshot, &paths)
                .await
                .unwrap();
        }
        let posts: Vec<_> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method == "POST")
            .collect();
        for (request, kind) in posts.iter().zip(["comment", "note"]) {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["admin_id"], "me");
            assert_eq!(body["message_type"], kind);
            assert_eq!(body["body"], "");
            assert_eq!(body["attachment_files"][0]["name"], "report.txt");
            assert_eq!(body["attachment_files"][0]["content_type"], "text/plain");
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(body["attachment_files"][0]["data"].as_str().unwrap())
                    .unwrap(),
                b"selected file"
            );
        }
        let error = api
            .reply_with_attachments("1", "", false, &snapshot[..1], &paths)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Conflict);
        std::fs::remove_file(attachment).unwrap();
        let error = api
            .reply_with_attachments("1", "keep text", false, &snapshot, &paths)
            .await
            .unwrap_err();
        assert!(!error.ambiguous);
        assert!(error.to_string().contains("Cannot read attachment"));
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.method == "POST")
                .count(),
            2
        );
    }

    #[test]
    fn keeps_notes_and_teammate_assignment_replies_not_bot_events() {
        let mut payload = conversation();
        payload["conversation_parts"]["conversation_parts"].as_array_mut().unwrap().extend([
            json!({"id":"bot","part_type":"assignment","author":{"type":"bot"},"body":"Assigned","created_at":30}),
            json!({"id":"reply","part_type":"assignment","author":{"type":"admin","name":"Bo"},"body":"On it","created_at":25}),
        ]);
        let c = normalize(&payload).unwrap();
        assert_eq!(c.messages.len(), 3);
        assert_eq!(c.messages[1].kind, Kind::Note);
        assert_eq!(c.from, "Bo");
        assert_eq!(c.snippet, "On it");
    }

    #[tokio::test]
    async fn lists_every_page_and_rejects_repeated_cursors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/conversations/search"))
            .and(body_partial_json(
                json!({"pagination":{"starting_after":"next"}}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"conversations":[{"id":"2","state":"open"}],"pages":{}})),
            )
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/conversations/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"conversations":[{"id":"1","state":"open"}],"pages":{"next":{"starting_after":"next"}}}))).with_priority(2).mount(&server).await;
        assert_eq!(
            Api::mock(server.uri())
                .list(Filter::Open)
                .await
                .unwrap()
                .len(),
            2
        );
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"conversations":[],"pages":{"next":{"starting_after":"repeat"}}}),
            ))
            .mount(&server)
            .await;
        assert!(
            Api::mock(server.uri())
                .list(Filter::Open)
                .await
                .unwrap_err()
                .message
                .contains("repeated")
        );
    }

    #[tokio::test]
    async fn partial_inbox_is_never_successful() {
        let server = MockServer::start().await;
        Mock::given(body_partial_json(
            json!({"pagination":{"starting_after":"bad"}}),
        ))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({})))
        .with_priority(1)
        .mount(&server)
        .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"conversations":[{"id":"1"}],"pages":{"next":{"starting_after":"bad"}}}),
            ))
            .with_priority(2)
            .mount(&server)
            .await;
        assert!(Api::mock(server.uri()).list(Filter::All).await.is_err());
    }

    #[tokio::test]
    async fn paginates_thread_parts() {
        let server = MockServer::start().await;
        Mock::given(query_param("starting_after", "p2")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"conversation_parts":{"conversation_parts":[{"id":"last","part_type":"comment","author":{"type":"user"},"body":"More","created_at":30}]}}))).with_priority(1).mount(&server).await;
        let mut first = conversation();
        first["conversation_parts"]["total_count"] = json!(2);
        first["conversation_parts"]["pages"] = json!({"next":{"starting_after":"p2"}});
        Mock::given(path("/conversations/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(first))
            .with_priority(2)
            .mount(&server)
            .await;
        let thread = Api::mock(server.uri()).thread("1").await.unwrap();
        assert!(thread.complete);
        assert_eq!(thread.messages.len(), 3);
    }

    #[tokio::test]
    async fn new_notes_prevent_posting_and_404_never_retries_write() {
        let server = MockServer::start().await;
        Mock::given(path("/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"me"})))
            .mount(&server)
            .await;
        Mock::given(path("/conversations/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(conversation()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/conversations/1/reply"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let api = Api::mock(server.uri());
        let error = api
            .reply(
                "1",
                "Hello",
                false,
                &[SeenMessage {
                    id: "source".into(),
                    created_at: 10,
                }],
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Conflict);
        let error = api
            .reply(
                "1",
                "Hello",
                true,
                &seen(&normalize(&conversation()).unwrap().messages),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Other);
        let requests = server.received_requests().await.unwrap();
        let post = requests
            .iter()
            .find(|r| r.url.path().ends_with("/reply"))
            .unwrap();
        let body: Value = serde_json::from_slice(&post.body).unwrap();
        assert_eq!(body["admin_id"], "me");
        assert_eq!(body["message_type"], "note");
    }

    #[tokio::test]
    async fn server_write_errors_are_ambiguous_and_not_retried() {
        let server = MockServer::start().await;
        Mock::given(path("/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"me"})))
            .mount(&server)
            .await;
        Mock::given(path("/conversations/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(conversation()))
            .mount(&server)
            .await;
        Mock::given(path("/conversations/1/reply"))
            .and(method("POST"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_json(json!({"errors":[{"message":"Server error"}]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = Api::mock(server.uri())
            .reply(
                "1",
                "Hello",
                false,
                &seen(&normalize(&conversation()).unwrap().messages),
            )
            .await
            .unwrap_err();
        assert!(error.ambiguous);
    }

    #[tokio::test]
    async fn revoked_tokens_and_missing_permissions_are_distinct_even_without_json() {
        let server = MockServer::start().await;
        Mock::given(path("/me"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;
        assert_eq!(
            Api::mock(server.uri()).me().await.unwrap_err().kind,
            ErrorKind::Unauthorized
        );
        server.reset().await;
        Mock::given(path("/me"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"errors":[{"code":"token_unauthorized"}]})),
            )
            .mount(&server)
            .await;
        assert_eq!(
            Api::mock(server.uri()).me().await.unwrap_err().kind,
            ErrorKind::Permission
        );
    }

    #[tokio::test]
    async fn snoozed_previews_use_latest_message_instead_of_initial_source() {
        let server = MockServer::start().await;
        Mock::given(path("/conversations/search")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"conversations":[{"id":"1","state":"snoozed","source":{"body":"Original question","author":{"name":"Ada","type":"user"}}}]}))).mount(&server).await;
        Mock::given(path("/conversations/1")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"1","state":"snoozed","source":{"body":"Original question","created_at":1,"author":{"name":"Ada","type":"user"}},"conversation_parts":{"conversation_parts":[{"id":"reply","part_type":"comment","body":"Latest answer","created_at":2,"author":{"name":"Bo","type":"admin"}}]}}))).mount(&server).await;
        let rows = Api::mock(server.uri()).list(Filter::Snoozed).await.unwrap();
        assert_eq!(rows[0].snippet, "Latest answer");
        assert_eq!(rows[0].from, "Bo");
    }
}
