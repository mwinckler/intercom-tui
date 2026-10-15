use crate::model::{
    Conversation, Counts, Draft, Filter, Identity, ReadState, Region, Scheduled, SendState, safe_id,
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub refresh_seconds: u64,
    pub region: Region,
    pub workspace: String,
    pub identity: Identity,
    pub filter: Filter,
    pub selected: String,
    pub zoom: bool,
    pub notifications: bool,
    pub token_file: Option<PathBuf>,
    /// None uses the system locale. true is month/day, false is day/month.
    pub month_first: Option<bool>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            refresh_seconds: 60,
            region: Region::Us,
            workspace: String::new(),
            identity: Identity::default(),
            filter: Filter::Open,
            selected: String::new(),
            zoom: false,
            notifications: true,
            token_file: None,
            month_first: None,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Cache {
    pub version: u32,
    pub conversations: BTreeMap<String, Conversation>,
    pub inboxes: BTreeMap<String, Vec<String>>,
    pub reads: ReadState,
    pub counts: Counts,
    pub fetched_at: i64,
}

#[derive(Clone)]
pub struct Store {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    // Keep the advisory lock alive across asynchronous tasks and clones.
    _lock: Arc<File>,
}

impl Store {
    pub fn open(root: Option<&Path>) -> Result<Self> {
        let (config_dir, data_dir) = if let Some(root) = root {
            (root.join("config"), root.join("data"))
        } else {
            let dirs = directories::ProjectDirs::from("", "", "intercom-tui")
                .context("Cannot locate your application directories; use --data-dir")?;
            (
                dirs.config_dir().to_owned(),
                dirs.data_local_dir().to_owned(),
            )
        };
        private_dir(&config_dir)?;
        private_dir(&data_dir)?;
        let lock = private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(data_dir.join("session.lock"))?;
        lock.try_lock_exclusive().context("Another intercom-tui is using this data directory. Only one instance may poll or send scheduled messages.")?;
        Ok(Self {
            config_dir,
            data_dir,
            _lock: Arc::new(lock),
        })
    }
    pub fn config(&self) -> Result<Config> {
        load(&self.config_dir.join("config.json"))
    }
    pub fn save_config(&self, config: &Config) -> Result<()> {
        atomic_json(&self.config_dir.join("config.json"), config)
    }
    pub fn credentials(&self) -> Result<Credentials> {
        load(&self.config_dir.join("oauth.json"))
    }
    pub fn save_credentials(&self, credentials: &Credentials) -> Result<()> {
        atomic_json(&self.config_dir.join("oauth.json"), credentials)
    }
    pub fn token_path(&self) -> PathBuf {
        self.config_dir.join("token")
    }
    pub fn save_token(&self, token: &str) -> Result<()> {
        if token.trim().is_empty() {
            bail!("The access token is empty");
        }
        atomic_write(&self.token_path(), token.trim().as_bytes())
    }
    pub fn remove_token(&self) -> Result<()> {
        match fs::remove_file(self.token_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn read_token(&self, external: Option<&Path>) -> Result<Option<String>> {
        if let Ok(token) = std::env::var("INTERCOM_ACCESS_TOKEN")
            && !token.trim().is_empty()
        {
            return Ok(Some(token.trim().into()));
        }
        if let Some(path) = external {
            let token = fs::read_to_string(expand_home(path))
                .context("Cannot read the configured access-token file")?;
            if token.trim().is_empty() {
                bail!("The configured access-token file is empty");
            }
            return Ok(Some(token.trim().into()));
        }
        match fs::read_to_string(self.token_path()) {
            Ok(token) if !token.trim().is_empty() => return Ok(Some(token.trim().into())),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e).context("Cannot read saved token"),
        }
        // Explicitly portable deployments may place a token next to the executable.
        // Never look in the old Omarchy plugin's configuration directory.
        if let Ok(executable) = std::env::current_exe()
            && let Some(parent) = executable.parent()
            && let Ok(token) = fs::read_to_string(parent.join("token"))
            && !token.trim().is_empty()
        {
            return Ok(Some(token.trim().into()));
        }
        Ok(None)
    }
    pub fn portable_token_active(&self) -> bool {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|p| p.join("token")))
            .and_then(|path| fs::read_to_string(path).ok())
            .is_some_and(|token| !token.trim().is_empty())
    }
    /// Keep different teammates/workspaces isolated without deleting their data.
    pub fn account_dir(&self, identity: &Identity) -> Result<PathBuf> {
        let id = format!(
            "{:?}-{}-{}",
            identity.region, identity.workspace, identity.id
        );
        if !safe_id(&id) {
            bail!("Intercom returned an invalid account identifier");
        }
        let path = self.data_dir.join("accounts").join(id);
        private_dir(&path)?;
        Ok(path)
    }
    pub fn cache(&self, identity: &Identity) -> Result<Cache> {
        load(&self.account_dir(identity)?.join("cache.json"))
    }
    pub fn save_cache(&self, identity: &Identity, cache: &Cache) -> Result<()> {
        atomic_json(&self.account_dir(identity)?.join("cache.json"), cache)
    }
    pub fn drafts(&self, identity: &Identity) -> Result<BTreeMap<String, Draft>> {
        load(&self.account_dir(identity)?.join("drafts.json"))
    }
    pub fn save_drafts(&self, identity: &Identity, drafts: &BTreeMap<String, Draft>) -> Result<()> {
        atomic_json(&self.account_dir(identity)?.join("drafts.json"), drafts)
    }
    pub fn scheduled(&self, identity: &Identity) -> Result<Vec<Scheduled>> {
        let mut rows: Vec<Scheduled> = load(&self.account_dir(identity)?.join("scheduled.json"))?;
        for row in &mut rows {
            if matches!(row.status, SendState::Sending) {
                row.status = SendState::NeedsReview("The app exited during a send. Check Intercom before retrying to avoid a duplicate.".into());
            }
        }
        rows.sort_by_key(|row| row.send_at);
        Ok(rows)
    }
    pub fn save_scheduled(&self, identity: &Identity, rows: &[Scheduled]) -> Result<()> {
        atomic_json(&self.account_dir(identity)?.join("scheduled.json"), rows)
    }
    pub fn archive_dir(&self, identity: &Identity) -> Result<PathBuf> {
        let path = self.account_dir(identity)?.join("archive");
        private_dir(&path)?;
        Ok(path)
    }
    pub fn archive_path(&self, identity: &Identity, id: &str) -> Result<PathBuf> {
        if !safe_id(id) {
            bail!("Invalid conversation ID");
        }
        Ok(self.archive_dir(identity)?.join(format!("{id}.json")))
    }
    pub fn archive(&self, identity: &Identity, conversation: &Conversation) -> Result<()> {
        atomic_json(
            &self.archive_path(identity, &conversation.id)?,
            conversation,
        )
    }
    pub fn archived(&self, identity: &Identity, id: &str) -> Result<Option<Conversation>> {
        let path = self.archive_path(identity, id)?;
        if !path.exists() {
            return Ok(None);
        }
        load(&path).map(Some)
    }
    pub fn search_archive(&self, identity: &Identity, query: &str) -> Result<Vec<Conversation>> {
        let tokens: Vec<_> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if tokens.is_empty() {
            return Ok(vec![]);
        }
        let mut matches = Vec::new();
        for entry in fs::read_dir(self.archive_dir(identity)?)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let conversation: Conversation = load(&path)?;
                let hay = conversation.haystack();
                if tokens.iter().all(|token| hay.contains(token)) {
                    matches.push(conversation);
                }
            }
        }
        matches.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        matches.truncate(50);
        Ok(matches)
    }
}

pub fn expand_home(path: &Path) -> PathBuf {
    if let Ok(tail) = path.strip_prefix("~")
        && let Some(base) = directories::BaseDirs::new()
    {
        return base.home_dir().join(tail);
    }
    path.to_owned()
}

pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("Cannot create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Invalid storage path")?;
    private_dir(parent)?;
    let temp = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = private_options().write(true).create_new(true).open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("Cannot save {}", path.display()))
}

pub fn atomic_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    atomic_write(path, &serde_json::to_vec(value)?)
}

pub fn load<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid JSON in {} (not overwritten)", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("Cannot read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_atomic_storage_and_locking() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(Some(root.path())).unwrap();
        assert!(Store::open(Some(root.path())).is_err());
        store.save_token("secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(store.token_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&store.config_dir)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        store.save_config(&Config::default()).unwrap();
        assert_eq!(store.config().unwrap().refresh_seconds, 60);
    }
    #[test]
    fn corrupt_files_are_not_silently_discarded() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("drafts.json"), "broken").unwrap();
        assert!(load::<BTreeMap<String, Draft>>(&root.path().join("drafts.json")).is_err());
    }

    #[test]
    fn text_only_storage_remains_readable() {
        let draft: Draft = serde_json::from_str(r#"{"body":"old draft", "note":true}"#).unwrap();
        assert!(draft.attachments.is_empty());
        let scheduled: Scheduled = serde_json::from_str(r#"{"id":"s", "conversation_id":"1", "body":"old schedule", "note":false, "send_at":10, "seen":[]}"#).unwrap();
        assert!(scheduled.attachments.is_empty());
        let conversation: Conversation = serde_json::from_str(
            r#"{"id":"1", "complete":true, "messages":[{"id":"m", "body":"old message"}]}"#,
        )
        .unwrap();
        assert_eq!(conversation.content_version, 0);
        assert!(conversation.messages[0].blocks.is_empty());
        assert_eq!(conversation.messages[0].plain(), "old message");
    }
    #[test]
    fn interrupted_sends_need_review_and_archive_searches_message_bodies() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(Some(root.path())).unwrap();
        let identity = Identity {
            id: "123".into(),
            workspace: "test".into(),
            ..Default::default()
        };
        store
            .save_scheduled(
                &identity,
                &[Scheduled {
                    id: "s".into(),
                    conversation_id: "1".into(),
                    body: "hi".into(),
                    note: false,
                    attachments: vec![],
                    send_at: 1,
                    seen: vec![],
                    status: SendState::Sending,
                }],
            )
            .unwrap();
        assert!(matches!(
            store.scheduled(&identity).unwrap()[0].status,
            SendState::NeedsReview(_)
        ));
        let c = Conversation {
            id: "1".into(),
            messages: vec![crate::model::Message {
                body: "<b>unique</b> words".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        store.archive(&identity, &c).unwrap();
        assert_eq!(
            store
                .search_archive(&identity, "unique words")
                .unwrap()
                .len(),
            1
        );
        assert!(store.archive_path(&identity, "../../token").is_err());
    }
}
