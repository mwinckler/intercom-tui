use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

pub const CONTENT_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Media {
    pub url: String,
    pub name: String,
    pub alt: String,
    pub content_type: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub link_url: String,
}

impl Media {
    pub fn label(&self) -> String {
        crate::format::safe_text(if !self.name.is_empty() {
            &self.name
        } else if !self.alt.is_empty() {
            &self.alt
        } else {
            "Image"
        })
        .replace(['\n', '\t'], " ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { html: String },
    Image(Media),
    Attachment(Media),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    #[default]
    Us,
    Eu,
    Au,
}

impl Region {
    pub fn api(self) -> &'static str {
        match self {
            Self::Us => "https://api.intercom.io",
            Self::Eu => "https://api.eu.intercom.io",
            Self::Au => "https://api.au.intercom.io",
        }
    }
    pub fn web(self) -> &'static str {
        match self {
            Self::Us => "https://app.intercom.com",
            Self::Eu => "https://app.eu.intercom.com",
            Self::Au => "https://app.au.intercom.com",
        }
    }
    pub fn order(self) -> Vec<Self> {
        let mut regions = vec![self];
        for region in [Self::Us, Self::Eu, Self::Au] {
            if !regions.contains(&region) {
                regions.push(region);
            }
        }
        regions
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    #[default]
    Open,
    Closed,
    Snoozed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Filter {
    #[default]
    Open,
    Waiting,
    Closed,
    Snoozed,
    All,
}

impl Filter {
    pub const ALL: [Self; 5] = [
        Self::Open,
        Self::Waiting,
        Self::Closed,
        Self::Snoozed,
        Self::All,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Waiting => "Waiting",
            Self::Closed => "Closed",
            Self::Snoozed => "Snoozed",
            Self::All => "All",
        }
    }
    pub fn matches(self, conversation: &Conversation) -> bool {
        match self {
            Self::Open => conversation.state == State::Open,
            Self::Waiting => conversation.state == State::Open && conversation.waiting_since > 0,
            Self::Closed => conversation.state == State::Closed,
            Self::Snoozed => conversation.state == State::Snoozed,
            Self::All => true,
        }
    }
    pub fn chord(c: char) -> Option<Self> {
        match c {
            'o' => Some(Self::Open),
            'w' => Some(Self::Waiting),
            'c' => Some(Self::Closed),
            's' => Some(Self::Snoozed),
            'a' => Some(Self::All),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Customer,
    Teammate,
    Note,
    Bot,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Message {
    pub id: String,
    pub from: String,
    pub email: String,
    pub author_id: String,
    pub kind: Kind,
    pub created_at: i64,
    pub body: String,
    pub blocks: Vec<ContentBlock>,
}

impl Message {
    pub fn plain(&self) -> String {
        if self.blocks.is_empty() {
            return crate::format::plain(&self.body);
        }
        self.blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { html } => crate::format::plain(html),
                ContentBlock::Image(media) => {
                    let label = media.label();
                    if media.alt.is_empty() || media.alt == label {
                        format!("Image: {label}")
                    } else {
                        format!("Image: {label} {}", media.alt)
                    }
                }
                ContentBlock::Attachment(media) => format!("Attachment: {}", media.label()),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub from: String,
    pub email: String,
    pub snippet: String,
    pub state: State,
    pub waiting_since: i64,
    pub updated_at: i64,
    pub messages: Vec<Message>,
    pub complete: bool,
    pub fetched_at: i64,
    pub content_version: u32,
}

impl Conversation {
    pub fn haystack(&self) -> String {
        let mut text = format!(
            "{} {} {} {} {} {:?}",
            self.id, self.title, self.from, self.email, self.snippet, self.state
        );
        for message in &self.messages {
            text.push_str(&format!(
                " {} {} {}",
                message.from,
                message.email,
                message.plain()
            ));
        }
        text.to_lowercase()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Identity {
    pub id: String,
    pub name: String,
    pub email: String,
    pub workspace: String,
    pub region: Region,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Counts {
    pub open: usize,
    pub waiting: usize,
    pub closed: usize,
    pub snoozed: usize,
}

impl Counts {
    pub fn get(&self, filter: Filter) -> usize {
        match filter {
            Filter::Open => self.open,
            Filter::Waiting => self.waiting,
            Filter::Closed => self.closed,
            Filter::Snoozed => self.snoozed,
            Filter::All => self.open + self.closed + self.snoozed,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Draft {
    pub body: String,
    pub note: bool,
    pub attachments: Vec<String>,
}

impl Draft {
    pub fn is_empty(&self) -> bool {
        self.body.trim().is_empty() && self.attachments.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenMessage {
    pub id: String,
    pub created_at: i64,
}

pub fn seen(messages: &[Message]) -> Vec<SeenMessage> {
    messages
        .iter()
        .map(|m| SeenMessage {
            id: m.id.clone(),
            created_at: m.created_at,
        })
        .collect()
}

pub fn has_new_comments(messages: &[Message], seen: &[SeenMessage]) -> bool {
    // Older previously-unfetched history must not block a reply.
    let newest = seen.iter().map(|m| m.created_at).max().unwrap_or(0);
    let ids: HashSet<_> = seen.iter().map(|m| m.id.as_str()).collect();
    !seen.is_empty()
        && messages
            .iter()
            .any(|m| m.created_at >= newest && !ids.contains(m.id.as_str()))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendState {
    #[default]
    Pending,
    Sending,
    NeedsReview(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scheduled {
    pub id: String,
    pub conversation_id: String,
    pub body: String,
    pub note: bool,
    #[serde(default)]
    pub attachments: Vec<String>,
    pub send_at: i64,
    pub seen: Vec<SeenMessage>,
    #[serde(default)]
    pub status: SendState,
}

impl Scheduled {
    pub fn due(&self, now: i64) -> bool {
        self.send_at <= now && matches!(self.status, SendState::Pending)
    }
}

/// Local read flags are independent of Intercom's own read status.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReadState(pub BTreeMap<String, BTreeMap<String, bool>>);

impl ReadState {
    pub fn observe(&mut self, conversation: &Conversation, read: bool, own: &Identity) {
        let flags = self.0.entry(conversation.id.clone()).or_default();
        let read = read || conversation.state != State::Open;
        if read {
            flags.values_mut().for_each(|flag| *flag = true);
        }
        for message in &conversation.messages {
            let mine = matches!(message.kind, Kind::Teammate | Kind::Note)
                && ((!own.id.is_empty() && message.author_id == own.id)
                    || (!own.email.is_empty() && message.email.eq_ignore_ascii_case(&own.email)));
            if mine {
                continue;
            }
            let value = flags.entry(message.id.clone()).or_insert(read);
            if read {
                *value = true;
            }
        }
    }
    pub fn mark_read(&mut self, id: &str) {
        if let Some(flags) = self.0.get_mut(id) {
            flags.values_mut().for_each(|flag| *flag = true);
        }
    }
    pub fn reconcile_open(&mut self, conversations: &[Conversation]) {
        let open: HashSet<_> = conversations
            .iter()
            .filter(|c| c.state == State::Open)
            .map(|c| c.id.as_str())
            .collect();
        for (id, flags) in &mut self.0 {
            if !open.contains(id.as_str()) {
                flags.values_mut().for_each(|flag| *flag = true);
            }
        }
    }
    pub fn unread(&self, id: &str) -> bool {
        self.0
            .get(id)
            .is_some_and(|flags| flags.values().any(|read| !read))
    }
    pub fn message_unread(&self, id: &str, message: &str) -> bool {
        self.0
            .get(id)
            .and_then(|flags| flags.get(message))
            .is_some_and(|read| !read)
    }
    pub fn count(&self) -> usize {
        self.0.keys().filter(|id| self.unread(id)).count()
    }
}

pub fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

pub fn workspace_id(value: &str) -> Option<String> {
    let candidate = if value.contains('/') {
        let url = reqwest::Url::parse(value).ok()?;
        let parts: Vec<_> = url.path_segments()?.collect();
        if parts.len() < 3 || parts[0] != "a" || !["inbox", "apps"].contains(&parts[1]) {
            return None;
        }
        parts[2].to_owned()
    } else {
        value.trim().to_owned()
    };
    let reserved = [
        "inbox",
        "apps",
        "shared",
        "all",
        "conversation",
        "conversations",
        "admin",
        "home",
    ];
    (candidate.len() >= 2
        && candidate.len() <= 64
        && safe_id(&candidate)
        && !reserved.contains(&candidate.to_lowercase().as_str()))
    .then(|| candidate.to_owned())
}

pub fn inbox_url(region: Region, workspace: &str, conversation: Option<&str>) -> String {
    let Some(workspace) = workspace_id(workspace) else {
        return region.web().into();
    };
    let base = format!("{}/a/inbox/{workspace}/inbox/shared/all", region.web());
    match conversation.filter(|id| safe_id(id)) {
        Some(id) => format!("{base}/conversation/{id}"),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_is_message_based_and_open_only() {
        let mut reads = ReadState::default();
        let mut c = Conversation {
            id: "1".into(),
            messages: vec![Message {
                id: "a".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        reads.observe(&c, false, &Identity::default());
        assert!(reads.unread("1"));
        reads.mark_read("1");
        c.messages.push(Message {
            id: "b".into(),
            ..Default::default()
        });
        reads.observe(&c, false, &Identity::default());
        assert!(!reads.message_unread("1", "a"));
        assert!(reads.message_unread("1", "b"));
        c.state = State::Snoozed;
        reads.observe(&c, false, &Identity::default());
        assert_eq!(reads.count(), 0);
    }

    #[test]
    fn own_notes_are_not_unread() {
        let mut reads = ReadState::default();
        let c = Conversation {
            id: "1".into(),
            messages: vec![Message {
                id: "a".into(),
                author_id: "me".into(),
                kind: Kind::Note,
                ..Default::default()
            }],
            ..Default::default()
        };
        reads.observe(
            &c,
            false,
            &Identity {
                id: "me".into(),
                ..Default::default()
            },
        );
        assert_eq!(reads.count(), 0);
    }

    #[test]
    fn new_notes_block_but_old_history_does_not() {
        let seen = vec![SeenMessage {
            id: "a".into(),
            created_at: 10,
        }];
        assert!(!has_new_comments(
            &[Message {
                id: "old".into(),
                created_at: 5,
                ..Default::default()
            }],
            &seen
        ));
        assert!(has_new_comments(
            &[Message {
                id: "note".into(),
                created_at: 10,
                kind: Kind::Note,
                ..Default::default()
            }],
            &seen
        ));
    }

    #[test]
    fn links_validate_identifiers() {
        assert_eq!(
            workspace_id("https://app.intercom.com/a/inbox/abcd/inbox/shared/all"),
            Some("abcd".into())
        );
        assert_eq!(workspace_id("shared"), None);
        assert!(!safe_id("../../token"));
        assert!(inbox_url(Region::Eu, "abcd", Some("123")).ends_with("/conversation/123"));
    }
}
