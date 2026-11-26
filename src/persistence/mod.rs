use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{NotificationFingerprint, RepositorySnapshot};

pub const SCHEMA_VERSION: u32 = 1;
const MAX_DIAGNOSTIC_LOG_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct Settings {
    pub fetch_enabled: bool,
    pub refresh_interval_seconds: u64,
    pub refresh_on_wake: bool,
    pub notifications_enabled: bool,
    /// Also notify when local commits are waiting to be pushed. Off by
    /// default: those commits are the user's own, so the notification is
    /// usually noise.
    pub notify_on_push: bool,
    pub launch_at_login: bool,
    pub theme: ThemePreference,
    pub git_executable: PathBuf,
    pub fetch_timeout_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fetch_enabled: true,
            refresh_interval_seconds: 300,
            refresh_on_wake: true,
            notifications_enabled: false,
            notify_on_push: false,
            launch_at_login: false,
            theme: ThemePreference::System,
            git_executable: PathBuf::from("/usr/bin/git"),
            fetch_timeout_seconds: 30,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RepositoryRecord {
    pub id: Uuid,
    pub selected_path: PathBuf,
    pub canonical_path: PathBuf,
    #[serde(default)]
    pub ssh_destination: Option<String>,
    pub display_name: Option<String>,
    pub paused: bool,
    pub network_access_allowed: bool,
    #[serde(default = "system_time_now")]
    pub added_at: SystemTime,
    #[serde(default = "system_time_now")]
    pub updated_at: SystemTime,
}

impl RepositoryRecord {
    pub fn display_name(&self) -> String {
        self.display_name.clone().unwrap_or_else(|| {
            self.canonical_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.canonical_path.to_string_lossy().into_owned())
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AppConfig {
    pub schema_version: u32,
    pub settings: Settings,
    pub repositories: Vec<RepositoryRecord>,
    /// Names the user gave to computers, keyed by group key (`local` or
    /// `remote:<destination>`). A computer without an entry uses its
    /// default name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub computer_names: BTreeMap<String, String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            settings: Settings::default(),
            repositories: Vec::new(),
            computer_names: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SnapshotCache {
    pub schema_version: u32,
    pub snapshots: BTreeMap<Uuid, RepositorySnapshot>,
    #[serde(default)]
    pub notification_fingerprints: BTreeMap<Uuid, NotificationFingerprint>,
}

impl Default for SnapshotCache {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            snapshots: BTreeMap::new(),
            notification_fingerprints: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LoadResult<T> {
    pub value: T,
    pub warning: Option<String>,
}

#[derive(Debug)]
pub enum PersistenceError {
    HomeDirectoryUnavailable,
    Io(io::Error),
    Serialize(serde_json::Error),
    UnsupportedSchema { found: u32 },
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeDirectoryUnavailable => {
                formatter.write_str("the user home directory is unavailable")
            }
            Self::Io(error) => write!(formatter, "local storage failed: {error}"),
            Self::Serialize(error) => write!(formatter, "local data could not be encoded: {error}"),
            Self::UnsupportedSchema { found } => {
                write!(formatter, "unsupported data schema version {found}")
            }
        }
    }
}

impl std::error::Error for PersistenceError {}

impl From<io::Error> for PersistenceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for PersistenceError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialize(error)
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn application_support() -> Result<Self, PersistenceError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or(PersistenceError::HomeDirectoryUnavailable)?;
        Ok(Self::new(home.join("Library/Application Support/GitSync")))
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn diagnostics_path(&self) -> PathBuf {
        self.root.join("diagnostics.log")
    }

    pub fn load_config(&self) -> Result<LoadResult<AppConfig>, PersistenceError> {
        self.load_document("config.json", AppConfig::default)
    }

    pub fn load_cache(&self) -> Result<LoadResult<SnapshotCache>, PersistenceError> {
        self.load_document("cache.json", SnapshotCache::default)
    }

    pub fn save_config(&self, config: &AppConfig) -> Result<(), PersistenceError> {
        validate_schema(config.schema_version)?;
        self.save_document("config.json", config)
    }

    pub fn save_cache(&self, cache: &SnapshotCache) -> Result<(), PersistenceError> {
        validate_schema(cache.schema_version)?;
        self.save_document("cache.json", cache)
    }

    pub fn append_diagnostic(&self, event: &str, detail: &str) -> Result<(), PersistenceError> {
        fs::create_dir_all(&self.root)?;
        let path = self.diagnostics_path();
        if path
            .metadata()
            .map(|metadata| metadata.len() >= MAX_DIAGNOSTIC_LOG_BYTES)
            .unwrap_or(false)
        {
            let previous = self.root.join("diagnostics.previous.log");
            match fs::remove_file(&previous) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            fs::rename(&path, previous)?;
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        writeln!(
            file,
            "{timestamp}\t{}\t{}",
            log_field(event),
            log_field(detail)
        )?;
        Ok(())
    }

    pub fn delete_diagnostics(&self) -> Result<(), PersistenceError> {
        for path in [
            self.diagnostics_path(),
            self.root.join("diagnostics.previous.log"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn load_document<T>(
        &self,
        file_name: &str,
        default: impl FnOnce() -> T,
    ) -> Result<LoadResult<T>, PersistenceError>
    where
        T: DeserializeOwned + Versioned,
    {
        let path = self.root.join(file_name);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(LoadResult {
                    value: default(),
                    warning: None,
                });
            }
            Err(error) => return Err(error.into()),
        };

        match serde_json::from_slice::<T>(&bytes) {
            Ok(value) => {
                validate_schema(value.schema_version())?;
                Ok(LoadResult {
                    value,
                    warning: None,
                })
            }
            Err(error) => {
                fs::create_dir_all(&self.root)?;
                let moved_path = self.root.join(corrupt_name(file_name));
                fs::rename(&path, &moved_path)?;
                Ok(LoadResult {
                    value: default(),
                    warning: Some(format!(
                        "{file_name} was unreadable and moved to {}: {error}",
                        moved_path.display()
                    )),
                })
            }
        }
    }

    fn save_document(
        &self,
        file_name: &str,
        value: &impl Serialize,
    ) -> Result<(), PersistenceError> {
        fs::create_dir_all(&self.root)?;
        let bytes = serde_json::to_vec_pretty(value)?;
        let path = self.root.join(file_name);
        let temporary_path = self
            .root
            .join(format!(".{file_name}.tmp-{}", std::process::id()));
        let mut temporary = open_private(&temporary_path)?;

        let result = (|| -> Result<(), PersistenceError> {
            temporary.write_all(&bytes)?;
            temporary.write_all(b"\n")?;
            temporary.sync_all()?;
            fs::rename(&temporary_path, &path)?;
            sync_directory(&self.root)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result
    }
}

trait Versioned {
    fn schema_version(&self) -> u32;
}

impl Versioned for AppConfig {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

impl Versioned for SnapshotCache {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

fn validate_schema(found: u32) -> Result<(), PersistenceError> {
    if found == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(PersistenceError::UnsupportedSchema { found })
    }
}

fn open_private(path: &Path) -> Result<File, io::Error> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

fn sync_directory(path: &Path) -> Result<(), io::Error> {
    File::open(path)?.sync_all()
}

fn corrupt_name(file_name: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{file_name}.corrupt-{timestamp}")
}

fn system_time_now() -> SystemTime {
    SystemTime::now()
}

fn log_field(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            character => character,
        })
        .take(512)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn round_trips_configuration_and_cache() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let store = Store::new(temp.path().join("state"));
        let repository = RepositoryRecord {
            id: Uuid::new_v4(),
            selected_path: PathBuf::from("/tmp/selected"),
            canonical_path: PathBuf::from("/tmp/repository"),
            ssh_destination: Some("developer@workstation".to_owned()),
            display_name: Some("Example".to_owned()),
            paused: false,
            network_access_allowed: true,
            added_at: SystemTime::now(),
            updated_at: SystemTime::now(),
        };
        let config = AppConfig {
            settings: Settings {
                fetch_enabled: false,
                refresh_interval_seconds: 900,
                refresh_on_wake: false,
                notifications_enabled: true,
                notify_on_push: true,
                launch_at_login: true,
                theme: ThemePreference::Dark,
                git_executable: PathBuf::from("/opt/homebrew/bin/git"),
                fetch_timeout_seconds: 60,
            },
            repositories: vec![repository],
            computer_names: BTreeMap::from([("local".to_owned(), "Studio".to_owned())]),
            ..AppConfig::default()
        };
        let cache = SnapshotCache {
            notification_fingerprints: BTreeMap::from([(
                config.repositories[0].id,
                NotificationFingerprint::Ahead { count: 2 },
            )]),
            ..SnapshotCache::default()
        };

        store.save_config(&config).expect("config should save");
        store.save_cache(&cache).expect("cache should save");

        assert_eq!(
            store.load_config().expect("config should load").value,
            config
        );
        assert_eq!(store.load_cache().expect("cache should load").value, cache);
    }

    #[test]
    fn additive_settings_load_from_the_existing_schema() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let store = Store::new(temp.path().join("state"));
        fs::create_dir_all(store.root()).expect("state directory should be created");
        fs::write(
            store.root().join("config.json"),
            r#"{
  "schema_version": 1,
  "settings": {
    "fetch_enabled": false,
    "refresh_interval_seconds": 900
  },
  "repositories": [
    {
      "id": "00000000-0000-0000-0000-000000000000",
      "selected_path": "/tmp/repository",
      "canonical_path": "/tmp/repository",
      "display_name": null,
      "paused": false,
      "network_access_allowed": false
    }
  ]
}"#,
        )
        .expect("legacy fixture should be written");

        let loaded = store.load_config().expect("existing config should load");

        assert!(!loaded.value.settings.fetch_enabled);
        assert_eq!(loaded.value.settings.refresh_interval_seconds, 900);
        assert!(loaded.value.settings.refresh_on_wake);
        assert!(!loaded.value.settings.notifications_enabled);
        assert!(!loaded.value.settings.notify_on_push);
        assert!(!loaded.value.settings.launch_at_login);
        assert_eq!(loaded.value.settings.theme, ThemePreference::System);
        assert_eq!(
            loaded.value.settings.git_executable,
            PathBuf::from("/usr/bin/git")
        );
        assert_eq!(loaded.value.settings.fetch_timeout_seconds, 30);
        assert_eq!(loaded.value.repositories.len(), 1);
        assert_eq!(loaded.value.repositories[0].ssh_destination, None);
        assert!(loaded.value.repositories[0].added_at <= SystemTime::now());
        assert!(loaded.value.repositories[0].updated_at <= SystemTime::now());
    }

    #[test]
    fn diagnostic_log_is_private_sanitized_and_deletable() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TempDir::new().expect("temporary directory should be created");
        let store = Store::new(temp.path().join("state"));

        store
            .append_diagnostic("refresh\ncomplete", "outcome\tup_to_date")
            .expect("diagnostic should append");
        let contents =
            fs::read_to_string(store.diagnostics_path()).expect("diagnostic should be readable");

        assert!(contents.contains("refresh complete\toutcome up_to_date"));
        assert_eq!(
            fs::metadata(store.diagnostics_path())
                .expect("diagnostic metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::write(
            store.diagnostics_path(),
            vec![b'x'; MAX_DIAGNOSTIC_LOG_BYTES as usize],
        )
        .expect("oversized diagnostic fixture should be written");
        store
            .append_diagnostic("rotation", "complete")
            .expect("oversized diagnostic should rotate");
        assert!(store.root().join("diagnostics.previous.log").exists());
        store
            .delete_diagnostics()
            .expect("diagnostics should delete");
        assert!(!store.diagnostics_path().exists());
    }

    #[test]
    fn corrupt_document_is_preserved_and_defaults_are_recovered() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let root = temp.path().join("state");
        fs::create_dir_all(&root).expect("state directory should be created");
        fs::write(root.join("config.json"), "{broken").expect("fixture should be written");
        let store = Store::new(root.clone());

        let loaded = store
            .load_config()
            .expect("corruption should be recoverable");

        assert_eq!(loaded.value, AppConfig::default());
        assert!(loaded.warning.is_some());
        assert!(
            fs::read_dir(root)
                .expect("state directory should be readable")
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"))
        );
    }
}
