use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, AppContext as _, Context, EventEmitter, SystemNotification, WindowAppearance};
use uuid::Uuid;

use gitsync::domain::{
    ErrorKind, Freshness, NotificationFingerprint, NotificationTransition, OfflineReason,
    RepositoryIssue, RepositorySnapshot, StaleReason, SynchronizationState,
    notification_transition,
};
use gitsync::git::{
    CancellationToken, Discovery, FetchMode, GitError, RefreshOptions, discover_local_repositories,
    discover_remote_repositories, find_github_cli, git_version, normalize_remote_root,
    normalize_ssh_destination, refresh_remote_repository, refresh_repository, remote_git_version,
};
use gitsync::persistence::{
    AppConfig, RepositoryRecord, SCHEMA_VERSION, SnapshotCache, Store, ThemePreference,
};

use crate::macos::StatusItemController;

const MAX_CONCURRENT_REFRESHES: usize = 8;
/// Checks that run at once on one computer. The limit is per computer, so a
/// slow or unreachable SSH host does not hold up the checks on this Mac.
const MAX_REFRESHES_PER_COMPUTER: usize = 4;
/// Discovery walks whole folder trees, so it gets more time than one Git
/// command.
const MIN_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
/// Info notices clear themselves after this delay; errors stay until closed.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(6);
/// Refresh results are written to the cache at most this often.
const CACHE_SAVE_DELAY: Duration = Duration::from_secs(2);
/// Time for a finished scan's window to close before fetch consent is asked.
const CONSENT_PROMPT_DELAY: Duration = Duration::from_millis(400);
/// Notifications raised within this window are combined into one.
const NOTIFICATION_COALESCE_DELAY: Duration = Duration::from_secs(2);

/// Key and display name for the collapsible repository group a destination
/// belongs to. `None` is the local Mac; every other destination is grouped by
/// its remote computer.
pub(crate) fn repository_group_identity(destination: Option<&str>) -> (String, String) {
    match destination {
        Some(destination) => (format!("remote:{destination}"), destination.to_owned()),
        None => ("local".to_owned(), "This Mac".to_owned()),
    }
}

/// Records `name` for the computer group `key`, or forgets the custom name
/// when `name` is blank or the default. Returns whether anything changed.
fn set_computer_name(
    names: &mut std::collections::BTreeMap<String, String>,
    key: &str,
    default_label: &str,
    name: &str,
) -> bool {
    let name = name.trim();
    if name.is_empty() || name == default_label {
        names.remove(key).is_some()
    } else if names.get(key).map(String::as_str) == Some(name) {
        false
    } else {
        names.insert(key.to_owned(), name.to_owned());
        true
    }
}

/// Stable key used to remember whether a group is collapsed.
pub(crate) fn repository_group_key(destination: Option<&str>) -> String {
    repository_group_identity(destination).0
}

/// One-line summary of a group for its collapsible header.
pub(crate) fn repository_group_description(repository_count: usize, attention: usize) -> String {
    let repositories = match repository_count {
        1 => "1 repository".to_owned(),
        count => format!("{count} repositories"),
    };
    match attention {
        0 => repositories,
        1 => format!("{repositories} · 1 needs attention"),
        count => format!("{repositories} · {count} need attention"),
    }
}

/// Remembers which repository groups the user expanded or collapsed. Until
/// the user chooses, a group is expanded only when it needs attention, so a
/// long list opens as a compact list of computers with problems in view. The
/// choice survives closing and reopening the panel and settings window.
#[derive(Default)]
pub(crate) struct RepositoryGroupState {
    collapsed: HashMap<String, bool>,
}

impl gpui_kit::Global for RepositoryGroupState {}

impl RepositoryGroupState {
    pub fn is_collapsed(&self, group: &RepositoryGroup) -> bool {
        self.collapsed
            .get(&group.key)
            .copied()
            .unwrap_or(group.attention == 0)
    }

    pub fn set_collapsed(&mut self, key: &str, collapsed: bool) {
        self.collapsed.insert(key.to_owned(), collapsed);
    }
}

/// A remote computer (or the local Mac) that GitSync monitors repositories
/// from, paired with the state the collapsible list needs to render.
#[derive(Clone, Debug)]
pub(crate) struct RepositoryGroup {
    pub key: String,
    /// The name the user gave the computer, or its default name.
    pub label: String,
    /// `This Mac`, or the SSH destination.
    pub default_label: String,
    pub ssh_destination: Option<String>,
    /// True when every repository in the group is paused.
    pub paused: bool,
    pub repository_count: usize,
    pub attention: usize,
}

/// How a repository is presented. Only `Problem` means something is broken;
/// `Unverified` means the remote state is unknown (fetch is off, not yet
/// allowed, offline, or not checked since launch) and is shown neutrally.
/// `Uncommitted` means the branch is level with its upstream, as far as
/// GitSync knows, but the working tree has changes that are not committed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepositoryCategory {
    Actionable,
    Problem,
    Uncommitted,
    Unverified,
    Current,
    Paused,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct StatusSummary {
    pub total: usize,
    pub actionable: usize,
    pub problems: usize,
    pub uncommitted: usize,
    pub unverified: usize,
    /// Total commits waiting to be pushed across actionable repositories.
    pub push: u32,
    /// Total commits waiting to be pulled across actionable repositories.
    pub pull: u32,
}

/// One monitored repository plus the transient state of its last refresh.
pub(crate) struct RepositoryRow {
    pub record: RepositoryRecord,
    pub snapshot: RepositorySnapshot,
    pub refreshing: bool,
    refresh_pending: bool,
    refresh_sequence: u64,
    cancellation: Option<CancellationToken>,
    refresh_started_at: Option<Instant>,
}

struct DiscoveredRepository {
    selected_path: PathBuf,
    ssh_destination: Option<String>,
    snapshot: RepositorySnapshot,
}

impl RepositoryRow {
    pub fn category(&self) -> RepositoryCategory {
        category_for(self.record.paused, &self.snapshot)
    }
}

pub(crate) fn category_for(paused: bool, snapshot: &RepositorySnapshot) -> RepositoryCategory {
    if paused {
        return RepositoryCategory::Paused;
    }
    if snapshot.issue.as_ref().is_some_and(issue_is_problem) {
        return RepositoryCategory::Problem;
    }
    // Commit counts against the last known upstream stay true when the
    // remote cannot be reached: local commits still need a push, and known
    // remote commits still need a pull.
    match snapshot.synchronization {
        Some(
            SynchronizationState::Ahead { .. }
            | SynchronizationState::Behind { .. }
            | SynchronizationState::Diverged { .. },
        ) => RepositoryCategory::Actionable,
        // Uncommitted changes are a local fact, so they count even while the
        // remote state is unknown. A repository with them is never shown as
        // up to date.
        _ if snapshot.working_tree.is_dirty() => RepositoryCategory::Uncommitted,
        Some(SynchronizationState::UpToDate)
            if snapshot.issue.is_none()
                && matches!(
                    snapshot.freshness,
                    Freshness::Fresh { .. } | Freshness::LocalUpstream { .. }
                ) =>
        {
            RepositoryCategory::Current
        }
        Some(SynchronizationState::UpToDate) | None => RepositoryCategory::Unverified,
    }
}

/// A transient fetch failure (offline, timeout, canceled) only makes the
/// remote state unknown. Every other issue needs the user to act.
pub(crate) fn issue_is_problem(issue: &RepositoryIssue) -> bool {
    match issue {
        RepositoryIssue::FetchFailed { kind, .. } => !matches!(
            kind,
            ErrorKind::NetworkUnavailable | ErrorKind::Timeout | ErrorKind::Cancelled
        ),
        RepositoryIssue::Unavailable { kind, .. } => *kind != ErrorKind::Cancelled,
        RepositoryIssue::NoUpstream | RepositoryIssue::DetachedHead => true,
    }
}

fn is_cancelled(snapshot: &RepositorySnapshot) -> bool {
    matches!(
        snapshot.issue,
        Some(
            RepositoryIssue::FetchFailed {
                kind: ErrorKind::Cancelled,
                ..
            } | RepositoryIssue::Unavailable {
                kind: ErrorKind::Cancelled,
                ..
            }
        )
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NoticeTone {
    Info,
    Error,
}

/// A short message shown at the top of the panel and Settings.
#[derive(Clone, Debug)]
pub(crate) struct Notice {
    pub text: String,
    pub tone: NoticeTone,
    sequence: u64,
}

/// Progress of a running folder scan, shared with the background task.
#[derive(Debug, Default)]
pub(crate) struct ScanProgress {
    pub found: AtomicUsize,
    pub checked: AtomicUsize,
}

/// Emitted when a remote scan ends, so the remote window can close itself on
/// success or show the error in place.
#[derive(Clone, Debug)]
pub(crate) enum ScanEvent {
    RemoteFinished { result: Result<(), String> },
}

pub(crate) struct AppModel {
    store: Store,
    config: AppConfig,
    cache: SnapshotCache,
    pub repositories: Vec<RepositoryRow>,
    pub message: Option<Notice>,
    pub adding_repository: bool,
    pub scan_progress: Option<Arc<ScanProgress>>,
    pub testing_git: bool,
    /// True when saved data could not be loaded. Nothing is written in this
    /// state, so the files on disk are never replaced with defaults.
    read_only: bool,
    active_refreshes: usize,
    queued_refreshes: VecDeque<Uuid>,
    add_cancellation: Option<CancellationToken>,
    add_generation: u64,
    git_test_cancellation: Option<CancellationToken>,
    last_refresh_outcome: Option<String>,
    tested_git_version: Option<String>,
    notice_sequence: u64,
    cache_save_scheduled: bool,
    pending_notifications: Vec<(Uuid, String, NotificationFingerprint)>,
    notification_flush_scheduled: bool,
    /// When the last full refresh started, manual or scheduled.
    last_refresh_all_at: Option<Instant>,
}

impl EventEmitter<ScanEvent> for AppModel {}

impl AppModel {
    pub fn load() -> Result<Self, String> {
        let store = Store::application_support().map_err(|error| error.to_string())?;
        let loaded_config = store.load_config().map_err(|error| error.to_string())?;
        let mut warnings = Vec::new();
        warnings.extend(loaded_config.warning);
        // The cache only speeds up the first paint, so an unreadable or newer
        // cache is replaced instead of blocking startup.
        let mut cache = match store.load_cache() {
            Ok(loaded_cache) => {
                warnings.extend(loaded_cache.warning);
                loaded_cache.value
            }
            Err(error) => {
                warnings.push(format!("Cached statuses were ignored: {error}"));
                SnapshotCache::default()
            }
        };
        let config = loaded_config.value;
        cache
            .snapshots
            .retain(|id, _| config.repositories.iter().any(|record| record.id == *id));

        for snapshot in cache.snapshots.values_mut() {
            snapshot.freshness = Freshness::Stale {
                last_successful_fetch_at: snapshot.freshness.last_successful_fetch_at(),
                reason: StaleReason::CachedAtStartup,
            };
        }

        let repositories = config
            .repositories
            .iter()
            .cloned()
            .map(|record| RepositoryRow {
                snapshot: cache.snapshots.get(&record.id).cloned().unwrap_or_default(),
                record,
                refreshing: false,
                refresh_pending: false,
                refresh_sequence: 0,
                cancellation: None,
                refresh_started_at: None,
            })
            .collect();

        if let Err(error) = store.append_diagnostic("startup", "configuration loaded") {
            warnings.push(format!("Diagnostics logging is unavailable: {error}"));
        }

        let mut model = Self::new(store, config, cache, repositories, false);
        if !warnings.is_empty() {
            model.message = Some(model.notice(warnings.join("\n"), NoticeTone::Error));
        }
        Ok(model)
    }

    /// A model used when saved data could not be loaded. It works for this
    /// session but never writes, so the saved files stay as they were.
    pub fn empty_for_error(message: String) -> Self {
        let store = Store::application_support()
            .unwrap_or_else(|_| Store::new(PathBuf::from("/tmp/GitSync")));
        let mut model = Self::new(
            store,
            AppConfig::default(),
            SnapshotCache::default(),
            Vec::new(),
            true,
        );
        model.message = Some(model.notice(
            format!(
                "GitSync could not load its saved data: {message}. Changes in this session are not saved, so the saved files stay unchanged."
            ),
            NoticeTone::Error,
        ));
        model
    }

    fn new(
        store: Store,
        config: AppConfig,
        cache: SnapshotCache,
        repositories: Vec<RepositoryRow>,
        read_only: bool,
    ) -> Self {
        Self {
            store,
            config,
            cache,
            repositories,
            message: None,
            adding_repository: false,
            scan_progress: None,
            testing_git: false,
            read_only,
            active_refreshes: 0,
            queued_refreshes: VecDeque::new(),
            add_cancellation: None,
            add_generation: 0,
            git_test_cancellation: None,
            last_refresh_outcome: None,
            tested_git_version: None,
            notice_sequence: 0,
            cache_save_scheduled: false,
            pending_notifications: Vec::new(),
            notification_flush_scheduled: false,
            last_refresh_all_at: None,
        }
    }

    fn notice(&mut self, text: String, tone: NoticeTone) -> Notice {
        self.notice_sequence += 1;
        Notice {
            text,
            tone,
            sequence: self.notice_sequence,
        }
    }

    /// Shows an info notice that clears itself after a short delay.
    fn show_info(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        let notice = self.notice(text.into(), NoticeTone::Info);
        let sequence = notice.sequence;
        self.message = Some(notice);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_TIMEOUT).await;
            this.update(cx, |model, cx| {
                if model
                    .message
                    .as_ref()
                    .is_some_and(|notice| notice.sequence == sequence)
                {
                    model.message = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Shows an error notice that stays until the user closes it.
    fn show_error(&mut self, text: impl Into<String>) {
        self.message = Some(self.notice(text.into(), NoticeTone::Error));
    }

    pub fn add_repository(&mut self, selected_path: PathBuf, cx: &mut Context<Self>) {
        if self.adding_repository {
            return;
        }
        let label = selected_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| selected_path.display().to_string());
        let root = selected_path.clone();
        self.start_scan(None, root, cx, move |options, progress, cancellation| {
            let discovery = discover_local_repositories(
                &selected_path,
                options.git_executable.clone(),
                options.timeout.max(MIN_DISCOVERY_TIMEOUT),
                cancellation,
            )?;
            Ok(check_discovered(
                discovery,
                None,
                options,
                progress,
                cancellation,
            ))
        });
        self.show_info(format!("Scanning {label} for Git projects…"), cx);
        self.publish(cx);
    }

    pub fn add_remote_folder(
        &mut self,
        destination: &str,
        selected_root: &str,
        cx: &mut Context<Self>,
    ) {
        if self.adding_repository {
            return;
        }
        let destination = match normalize_ssh_destination(destination) {
            Ok(destination) => destination,
            Err(error) => {
                cx.emit(ScanEvent::RemoteFinished {
                    result: Err(error.diagnostic),
                });
                return;
            }
        };
        let selected_root = match normalize_remote_root(selected_root) {
            Ok(selected_root) => selected_root,
            Err(error) => {
                cx.emit(ScanEvent::RemoteFinished {
                    result: Err(error.diagnostic),
                });
                return;
            }
        };

        let scan_destination = destination.clone();
        let root = selected_root.clone();
        self.start_scan(
            Some(destination),
            root,
            cx,
            move |options, progress, cancellation| {
                let discovery = discover_remote_repositories(
                    &scan_destination,
                    &selected_root,
                    options.timeout.max(MIN_DISCOVERY_TIMEOUT),
                    cancellation,
                )?;
                Ok(check_discovered(
                    discovery,
                    Some(&scan_destination),
                    options,
                    progress,
                    cancellation,
                ))
            },
        );
        self.publish(cx);
    }

    /// Runs `scan` in the background and adds what it finds. A generation
    /// number makes a canceled or superseded scan's late result harmless.
    fn start_scan(
        &mut self,
        destination: Option<String>,
        root: PathBuf,
        cx: &mut Context<Self>,
        scan: impl FnOnce(
            &RefreshOptions,
            &ScanProgress,
            &CancellationToken,
        ) -> Result<ScanResult, GitError>
        + Send
        + 'static,
    ) {
        self.adding_repository = true;
        self.add_generation += 1;
        let generation = self.add_generation;
        let options = self.refresh_options(FetchMode::Offline(OfflineReason::ConsentNotGranted));
        let cancellation = CancellationToken::default();
        self.add_cancellation = Some(cancellation.clone());
        let progress = Arc::new(ScanProgress::default());
        self.scan_progress = Some(progress.clone());
        let background_progress = progress.clone();
        cx.spawn(async move |this, cx| {
            let task = cx.background_spawn(async move {
                scan(&options, &background_progress, &cancellation)
                    .map_err(|error| error.diagnostic)
            });
            // Repaint while the scan runs so its progress stays current.
            let ticker = {
                let this = this.clone();
                cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(300))
                            .await;
                        let running = this
                            .update(cx, |model, cx| {
                                cx.notify();
                                model.add_generation == generation && model.adding_repository
                            })
                            .unwrap_or(false);
                        if !running {
                            break;
                        }
                    }
                })
            };
            let result = task.await;
            drop(ticker);
            let added = this
                .update(cx, |model, cx| {
                    model.finish_add(generation, destination.clone(), &root, result, cx)
                })
                .ok()
                .flatten();
            if let Some(added) = added
                && !added.is_empty()
            {
                // Let the remote window close before the modal question
                // opens. Closing a window during an `NSAlert` modal session
                // ends the session as if its default (Allow) button had been
                // pressed, which would grant consent nobody gave.
                cx.background_executor().timer(CONSENT_PROMPT_DELAY).await;
                // The alert blocks, so it runs here, between updates, and not
                // inside `cx.update`, where GPUI would still hold the app.
                let allow = crate::macos::confirm_fetch_consent(added.len());
                if allow {
                    this.update(cx, |model, cx| model.allow_network_access_for(&added, cx))
                        .ok();
                }
            }
        })
        .detach();
    }

    pub fn cancel_scan(&mut self, cx: &mut Context<Self>) {
        if !self.adding_repository {
            return;
        }
        if let Some(cancellation) = self.add_cancellation.take() {
            cancellation.cancel();
        }
        self.add_generation += 1;
        self.adding_repository = false;
        self.scan_progress = None;
        self.show_info("Scan canceled. Nothing was added.", cx);
        self.publish(cx);
    }

    /// Checks that `destination` accepts a batch-mode SSH login and has Git,
    /// then previews how many repositories a scan of `selected_root` would
    /// find, without checking or adding them.
    pub fn test_remote(
        &self,
        destination: &str,
        selected_root: &str,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Task<Result<String, String>> {
        let destination = match normalize_ssh_destination(destination) {
            Ok(destination) => destination,
            Err(error) => return gpui_kit::Task::ready(Err(error.diagnostic)),
        };
        let root_label = selected_root.trim().to_owned();
        let selected_root = normalize_remote_root(selected_root);
        let timeout = Duration::from_secs(self.config.settings.fetch_timeout_seconds);
        cx.background_spawn(async move {
            let cancellation = CancellationToken::default();
            let version = remote_git_version(&destination, timeout, &cancellation)
                .map_err(|error| error.diagnostic)?;
            let connected = format!("Connected to {destination} · {version}");
            let selected_root = match selected_root {
                Ok(selected_root) => selected_root,
                Err(error) => return Ok(format!("{connected}. Folder: {}", error.diagnostic)),
            };
            match discover_remote_repositories(
                &destination,
                &selected_root,
                timeout.max(MIN_DISCOVERY_TIMEOUT),
                &cancellation,
            ) {
                Ok(discovery) => Ok(format!(
                    "{connected}. {} in {root_label}.",
                    repository_count_label(discovery.repositories.len(), discovery.truncated)
                )),
                Err(error) => Ok(format!(
                    "{connected}. Could not list {root_label}: {}",
                    error.diagnostic
                )),
            }
        })
    }

    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.last_refresh_all_at = Some(Instant::now());
        let mut repository_ids: Vec<(u8, Uuid)> = self
            .repositories
            .iter()
            .filter(|row| !row.record.paused)
            .map(|row| (refresh_priority(row.category()), row.record.id))
            .collect();
        repository_ids.sort_by_key(|(priority, _)| *priority);
        for (_, repository_id) in repository_ids {
            self.refresh(repository_id, cx);
        }
    }

    /// Refreshes every active repository on one computer (or the local Mac).
    pub fn refresh_destination(&mut self, destination: Option<&str>, cx: &mut Context<Self>) {
        let repository_ids: Vec<Uuid> = self
            .repositories
            .iter()
            .filter(|row| {
                !row.record.paused && row.record.ssh_destination.as_deref() == destination
            })
            .map(|row| row.record.id)
            .collect();
        for repository_id in repository_ids {
            self.refresh(repository_id, cx);
        }
    }

    /// Time until the scheduler starts the next full refresh, or `None` in
    /// Manual mode.
    pub fn next_refresh_in(&self) -> Option<Duration> {
        let interval = Duration::from_secs(self.config.settings.refresh_interval_seconds);
        if interval.is_zero() {
            return None;
        }
        let elapsed = self
            .last_refresh_all_at
            .map(|started| started.elapsed())
            .unwrap_or(interval);
        Some(interval.saturating_sub(elapsed))
    }

    /// Whether the scheduler should start a full refresh now.
    pub fn refresh_due(&self) -> bool {
        self.next_refresh_in()
            .is_some_and(|remaining| remaining.is_zero())
    }

    pub fn refresh_interval_seconds(&self) -> u64 {
        self.config.settings.refresh_interval_seconds
    }

    pub fn refresh_queued(&self, repository_id: Uuid) -> bool {
        self.queued_refreshes.contains(&repository_id)
    }

    /// Checks running or waiting to run, for progress text.
    pub fn refreshes_remaining(&self) -> usize {
        self.active_refreshes + self.queued_refreshes.len()
    }

    /// Whether another check may start now on the computer at `destination`.
    fn has_refresh_capacity(&self, destination: Option<&str>) -> bool {
        self.active_refreshes < MAX_CONCURRENT_REFRESHES
            && self
                .repositories
                .iter()
                .filter(|row| {
                    row.refreshing && row.record.ssh_destination.as_deref() == destination
                })
                .count()
                < MAX_REFRESHES_PER_COMPUTER
    }

    pub fn fetch_enabled(&self) -> bool {
        self.config.settings.fetch_enabled
    }

    pub fn refresh_on_wake(&self) -> bool {
        self.config.settings.refresh_on_wake
    }

    pub fn notifications_enabled(&self) -> bool {
        self.config.settings.notifications_enabled
    }

    pub fn theme_preference(&self) -> ThemePreference {
        self.config.settings.theme
    }

    pub fn git_executable(&self) -> &Path {
        &self.config.settings.git_executable
    }

    pub fn fetch_timeout_seconds(&self) -> u64 {
        self.config.settings.fetch_timeout_seconds
    }

    pub fn apply_theme(preference: ThemePreference, cx: &mut App) {
        apply_theme_preference(preference, cx);
    }

    pub fn set_fetch_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.config.settings.fetch_enabled == enabled {
            return;
        }
        self.config.settings.fetch_enabled = enabled;
        if !enabled {
            for row in &mut self.repositories {
                if row.refreshing {
                    row.refresh_pending = true;
                    if let Some(cancellation) = &row.cancellation {
                        cancellation.cancel();
                    }
                }
            }
        }
        self.persist();
        self.publish(cx);
        if enabled {
            self.refresh_all(cx);
        }
    }

    pub fn set_refresh_interval_seconds(&mut self, seconds: u64, cx: &mut Context<Self>) {
        if ![0, 60, 300, 900, 1_800].contains(&seconds)
            || self.config.settings.refresh_interval_seconds == seconds
        {
            return;
        }
        self.config.settings.refresh_interval_seconds = seconds;
        self.persist();
        self.publish(cx);
    }

    pub fn set_theme_preference(&mut self, preference: ThemePreference, cx: &mut Context<Self>) {
        if self.config.settings.theme == preference {
            return;
        }
        self.config.settings.theme = preference;
        apply_theme_preference(preference, cx);
        self.persist();
        self.publish(cx);
        cx.refresh_windows();
    }

    pub fn set_git_executable(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match validate_git_executable(&path) {
            Ok(()) => {
                self.config.settings.git_executable = path;
                self.tested_git_version = None;
                self.persist();
                self.show_info("Git executable updated. Use Test Git to verify it.", cx);
            }
            Err(error) => self.show_error(error),
        }
        self.publish(cx);
    }

    pub fn set_fetch_timeout_seconds(&mut self, seconds: u64, cx: &mut Context<Self>) {
        if ![10, 30, 60].contains(&seconds) || self.config.settings.fetch_timeout_seconds == seconds
        {
            return;
        }
        self.config.settings.fetch_timeout_seconds = seconds;
        self.persist();
        self.publish(cx);
    }

    pub fn test_git(&mut self, cx: &mut Context<Self>) {
        if self.testing_git {
            return;
        }
        self.testing_git = true;
        self.publish(cx);
        let executable = self.config.settings.git_executable.clone();
        let timeout = Duration::from_secs(self.config.settings.fetch_timeout_seconds);
        let cancellation = CancellationToken::default();
        self.git_test_cancellation = Some(cancellation.clone());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { git_version(executable, timeout, &cancellation) })
                .await;
            this.update(cx, |model, cx| {
                model.testing_git = false;
                model.git_test_cancellation = None;
                match result {
                    Ok(version) => {
                        model.tested_git_version = Some(version.clone());
                        model.show_info(format!("Git is ready: {version}"), cx);
                    }
                    Err(error) => {
                        model.show_error(format!("Git test failed: {}", error.diagnostic));
                    }
                }
                model.publish(cx);
            })
            .ok();
        })
        .detach();
    }

    pub fn set_refresh_on_wake(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.config.settings.refresh_on_wake == enabled {
            return;
        }
        self.config.settings.refresh_on_wake = enabled;
        self.persist();
        self.publish(cx);
    }

    pub fn set_notifications_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.config.settings.notifications_enabled == enabled {
            return;
        }
        self.config.settings.notifications_enabled = enabled;
        if enabled {
            for row in &self.repositories {
                if let Some(fingerprint) = NotificationFingerprint::from_snapshot(&row.snapshot) {
                    self.cache
                        .notification_fingerprints
                        .insert(row.record.id, fingerprint);
                }
            }
            self.show_info(
                "Notifications enabled. macOS controls banner delivery in System Settings.",
                cx,
            );
            cx.show_system_notification(SystemNotification {
                tag: "notifications-enabled".into(),
                title: "GitSync notifications enabled".into(),
                body: "GitSync will notify you when a remote has new commits to pull.".into(),
                actions: Vec::new(),
            });
        } else {
            cx.dismiss_system_notification("notifications-enabled")
                .detach();
        }
        self.persist();
        self.publish(cx);
    }

    pub fn notify_on_push(&self) -> bool {
        self.config.settings.notify_on_push
    }

    pub fn set_notify_on_push(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.config.settings.notify_on_push == enabled {
            return;
        }
        self.config.settings.notify_on_push = enabled;
        self.persist();
        self.publish(cx);
    }

    pub fn set_launch_at_login_preference(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.config.settings.launch_at_login == enabled {
            return;
        }
        self.config.settings.launch_at_login = enabled;
        self.persist();
        self.publish(cx);
    }

    pub fn diagnostic_summary(&self) -> String {
        let summary = self.status_summary();
        let launch_at_login = crate::macos::launch_at_login_status()
            .map(|status| status.description().to_owned())
            .unwrap_or_else(|error| format!("Unavailable ({error})"));
        format!(
            "GitSync diagnostic summary\n\
             App version: {}\n\
             {}\n\
             GPUI Kit: {}\n\
             Config schema: {}\n\
             Repositories: {} total, {} actionable, {} with problems, {} uncommitted, {} unverified\n\
             Commits to push / pull: {} / {}\n\
             Remote computers: {}\n\
             Refresh interval: {} seconds\n\
             Refresh on wake: {}\n\
             Remote fetch: {}\n\
             Fetch timeout: {} seconds\n\
             Git executable: {}\n\
             Tested Git version: {}\n\
             Notifications: {} (push: {})\n\
             Theme: {:?}\n\
             Launch at login: {}\n\
             Last refresh outcome: {}\n\
             Data location: ~/Library/Application Support/GitSync",
            env!("CARGO_PKG_VERSION"),
            crate::macos::diagnostic_environment(),
            GPUI_KIT_VERSION,
            SCHEMA_VERSION,
            summary.total,
            summary.actionable,
            summary.problems,
            summary.uncommitted,
            summary.unverified,
            summary.push,
            summary.pull,
            self.repository_groups()
                .into_iter()
                .filter(|group| group.ssh_destination.is_some())
                .count(),
            self.refresh_interval_seconds(),
            enabled_label(self.refresh_on_wake()),
            enabled_label(self.fetch_enabled()),
            self.fetch_timeout_seconds(),
            self.git_executable().display(),
            self.tested_git_version
                .as_deref()
                .unwrap_or("Not tested this session"),
            enabled_label(self.notifications_enabled()),
            enabled_label(self.notify_on_push()),
            self.theme_preference(),
            launch_at_login,
            self.last_refresh_outcome
                .as_deref()
                .unwrap_or("No refresh completed this session"),
        )
    }

    pub fn storage_root(&self) -> &Path {
        self.store.root()
    }

    pub fn diagnostics_path(&self) -> PathBuf {
        self.store.diagnostics_path()
    }

    pub fn delete_diagnostics(&mut self, cx: &mut Context<Self>) {
        match self.store.delete_diagnostics() {
            Ok(()) => self.show_info("Diagnostic logs deleted.", cx),
            Err(error) => self.show_error(format!("Could not delete diagnostic logs: {error}")),
        }
        self.publish(cx);
    }

    pub fn rename_repository(&mut self, repository_id: Uuid, name: &str, cx: &mut Context<Self>) {
        let display_name = normalized_display_name(name);
        let Some(row) = self
            .repositories
            .iter_mut()
            .find(|row| row.record.id == repository_id)
        else {
            return;
        };
        if row.record.display_name == display_name {
            return;
        }
        row.record.display_name = display_name;
        row.record.updated_at = std::time::SystemTime::now();
        sync_config_records(&mut self.config, &self.repositories);
        self.persist();
        self.publish(cx);
    }

    /// Names a computer group. A blank name, or the default name, restores
    /// the default.
    pub fn rename_computer(&mut self, key: &str, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        let default_label = self
            .repository_groups()
            .into_iter()
            .find(|group| group.key == key)
            .map(|group| group.default_label);
        let Some(default_label) = default_label else {
            return;
        };
        if set_computer_name(&mut self.config.computer_names, key, &default_label, name) {
            self.persist();
        }
        // Publish even without a change, so a cleared field shows the
        // default name again.
        self.publish(cx);
    }

    pub fn show_message(&mut self, message: String, cx: &mut Context<Self>) {
        self.show_info(message, cx);
        self.publish(cx);
    }

    pub fn show_error_message(&mut self, message: String, cx: &mut Context<Self>) {
        self.show_error(message);
        self.publish(cx);
    }

    pub fn status_summary(&self) -> StatusSummary {
        let mut summary = StatusSummary {
            total: self.repositories.len(),
            ..StatusSummary::default()
        };
        for row in &self.repositories {
            match row.category() {
                RepositoryCategory::Actionable => summary.actionable += 1,
                RepositoryCategory::Problem => summary.problems += 1,
                RepositoryCategory::Uncommitted => summary.uncommitted += 1,
                RepositoryCategory::Unverified => summary.unverified += 1,
                RepositoryCategory::Current | RepositoryCategory::Paused => {}
            }
            // Cached comparisons remain useful when fetching is disabled or
            // the latest refresh failed. Keep their known commit counts visible
            // instead of turning the menu bar into zeroes until a fresh fetch.
            if !row.record.paused {
                match row.snapshot.synchronization {
                    Some(SynchronizationState::Ahead { count }) => {
                        summary.push = summary.push.saturating_add(count);
                    }
                    Some(SynchronizationState::Behind { count }) => {
                        summary.pull = summary.pull.saturating_add(count);
                    }
                    Some(SynchronizationState::Diverged { ahead, behind }) => {
                        summary.push = summary.push.saturating_add(ahead);
                        summary.pull = summary.pull.saturating_add(behind);
                    }
                    Some(SynchronizationState::UpToDate) | None => {}
                }
            }
        }
        summary
    }

    pub fn refresh(&mut self, repository_id: Uuid, cx: &mut Context<Self>) {
        let Some(index) = self
            .repositories
            .iter()
            .position(|row| row.record.id == repository_id)
        else {
            return;
        };
        let row = &mut self.repositories[index];
        if row.record.paused {
            return;
        }
        if row.refreshing {
            row.refresh_pending = true;
            return;
        }
        if self.queued_refreshes.contains(&repository_id) {
            return;
        }
        let destination = row.record.ssh_destination.clone();
        if !self.has_refresh_capacity(destination.as_deref()) {
            self.queued_refreshes.push_back(repository_id);
            self.publish(cx);
            return;
        }
        self.start_refresh(index, cx);
    }

    fn start_refresh(&mut self, index: usize, cx: &mut Context<Self>) {
        let (
            repository_id,
            sequence,
            cancellation,
            path,
            ssh_destination,
            previous,
            network_access_allowed,
        ) = {
            let row = &mut self.repositories[index];
            row.refresh_sequence += 1;
            let sequence = row.refresh_sequence;
            let cancellation = CancellationToken::default();
            row.cancellation = Some(cancellation.clone());
            row.refreshing = true;
            row.refresh_started_at = Some(Instant::now());
            (
                row.record.id,
                sequence,
                cancellation,
                row.record.canonical_path.clone(),
                row.record.ssh_destination.clone(),
                row.snapshot.clone(),
                row.record.network_access_allowed,
            )
        };
        let fetch_mode = if self.config.settings.fetch_enabled && network_access_allowed {
            FetchMode::Allowed
        } else if network_access_allowed {
            FetchMode::Offline(OfflineReason::DisabledByUser)
        } else {
            FetchMode::Offline(OfflineReason::ConsentNotGranted)
        };
        let options = self.refresh_options(fetch_mode);
        self.active_refreshes += 1;
        self.publish(cx);

        cx.spawn(async move |this, cx| {
            let snapshot = cx
                .background_spawn(async move {
                    if let Some(destination) = ssh_destination {
                        refresh_remote_repository(
                            &destination,
                            &path,
                            &options,
                            Some(&previous),
                            &cancellation,
                        )
                    } else {
                        refresh_repository(&path, &options, Some(&previous), &cancellation)
                    }
                })
                .await;
            this.update(cx, |model, cx| {
                model.finish_refresh(repository_id, sequence, snapshot, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Starts queued checks in order, skipping those whose computer is busy,
    /// so one computer's queue never blocks another's.
    fn start_queued_refreshes(&mut self, cx: &mut Context<Self>) {
        let mut position = 0;
        while position < self.queued_refreshes.len()
            && self.active_refreshes < MAX_CONCURRENT_REFRESHES
        {
            let repository_id = self.queued_refreshes[position];
            let Some(index) = self
                .repositories
                .iter()
                .position(|row| row.record.id == repository_id && !row.record.paused)
            else {
                self.queued_refreshes.remove(position);
                continue;
            };
            if self.repositories[index].refreshing {
                self.repositories[index].refresh_pending = true;
                self.queued_refreshes.remove(position);
                continue;
            }
            let destination = self.repositories[index].record.ssh_destination.clone();
            if !self.has_refresh_capacity(destination.as_deref()) {
                position += 1;
                continue;
            }
            self.queued_refreshes.remove(position);
            self.start_refresh(index, cx);
        }
    }

    pub fn allow_network_access(&mut self, repository_id: Uuid, cx: &mut Context<Self>) {
        self.allow_network_access_for(&[repository_id], cx);
    }

    /// Repositories that are monitored but may not fetch yet. Paused
    /// repositories are left out, so bulk consent never covers them.
    pub fn repositories_without_fetch_consent(&self) -> Vec<Uuid> {
        self.repositories
            .iter()
            .filter(|row| !row.record.paused && !row.record.network_access_allowed)
            .map(|row| row.record.id)
            .collect()
    }

    pub fn allow_network_access_for(&mut self, repository_ids: &[Uuid], cx: &mut Context<Self>) {
        let allowed = grant_network_access(&mut self.repositories, repository_ids);
        if allowed == 0 {
            return;
        }
        sync_config_records(&mut self.config, &self.repositories);
        self.persist();
        if allowed > 1 {
            self.show_info(
                format!(
                    "Fetch allowed for {allowed} repositories. GitSync uses each repository’s configured remote and credentials."
                ),
                cx,
            );
        }
        self.publish(cx);
        for repository_id in repository_ids {
            self.refresh(*repository_id, cx);
        }
    }

    pub fn toggle_paused(&mut self, repository_id: Uuid, cx: &mut Context<Self>) {
        let paused = match self
            .repositories
            .iter()
            .find(|row| row.record.id == repository_id)
        {
            Some(row) => !row.record.paused,
            None => return,
        };
        if !self.set_record_paused(repository_id, paused) {
            return;
        }
        sync_config_records(&mut self.config, &self.repositories);
        self.persist();
        self.publish(cx);
        self.start_queued_refreshes(cx);
        if !paused {
            self.refresh(repository_id, cx);
        }
    }

    /// Pauses or resumes every repository that belongs to one remote computer
    /// (or the local Mac when `destination` is `None`). This disables or
    /// re-enables the whole group's watchers without touching repository files.
    pub fn set_destination_paused(
        &mut self,
        destination: Option<&str>,
        paused: bool,
        cx: &mut Context<Self>,
    ) {
        let repository_ids: Vec<Uuid> = self
            .repositories
            .iter()
            .filter(|row| {
                row.record.ssh_destination.as_deref() == destination && row.record.paused != paused
            })
            .map(|row| row.record.id)
            .collect();
        if repository_ids.is_empty() {
            return;
        }
        for repository_id in &repository_ids {
            self.set_record_paused(*repository_id, paused);
        }
        sync_config_records(&mut self.config, &self.repositories);
        self.persist();
        self.publish(cx);
        self.start_queued_refreshes(cx);
        if !paused {
            for repository_id in repository_ids {
                self.refresh(repository_id, cx);
            }
        }
    }

    /// Removes every repository monitored from one remote computer (or the
    /// local Mac). Repository files are never changed.
    pub fn remove_destination(&mut self, destination: Option<&str>, cx: &mut Context<Self>) {
        let repository_ids: Vec<Uuid> = self
            .repositories
            .iter()
            .filter(|row| row.record.ssh_destination.as_deref() == destination)
            .map(|row| row.record.id)
            .collect();
        if repository_ids.is_empty() {
            return;
        }
        for repository_id in &repository_ids {
            self.remove_repository_record(*repository_id);
            self.record_diagnostic(
                "repository_removed",
                &format!("repository_id={repository_id} reason=destination"),
            );
        }
        sync_config_records(&mut self.config, &self.repositories);
        self.config
            .computer_names
            .remove(&repository_group_key(destination));
        self.persist();
        self.show_info(
            match destination {
                Some(destination) => format!(
                    "Removed every repository from {destination}. Files on the remote computer were not changed."
                ),
                None => "Removed every local repository from GitSync. Files were not changed."
                    .to_owned(),
            },
            cx,
        );
        self.publish(cx);
        self.start_queued_refreshes(cx);
    }

    /// Collapsible groups of repositories keyed by remote computer (or the local
    /// Mac), ordered with the local group last.
    pub fn repository_groups(&self) -> Vec<RepositoryGroup> {
        let mut groups: Vec<RepositoryGroup> = Vec::new();
        for row in &self.repositories {
            let destination = row.record.ssh_destination.as_deref();
            let key = repository_group_key(destination);
            let attention = matches!(
                row.category(),
                RepositoryCategory::Actionable
                    | RepositoryCategory::Problem
                    | RepositoryCategory::Uncommitted
            ) as usize;
            if let Some(group) = groups.iter_mut().find(|group| group.key == key) {
                group.repository_count += 1;
                group.attention += attention;
                group.paused = group.paused && row.record.paused;
            } else {
                let (_, default_label) = repository_group_identity(destination);
                let label = self
                    .config
                    .computer_names
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| default_label.clone());
                groups.push(RepositoryGroup {
                    key,
                    label,
                    default_label,
                    ssh_destination: row.record.ssh_destination.clone(),
                    paused: row.record.paused,
                    repository_count: 1,
                    attention,
                });
            }
        }
        groups.sort_by(|left, right| {
            left.ssh_destination
                .is_none()
                .cmp(&right.ssh_destination.is_none())
                .then_with(|| left.label.to_lowercase().cmp(&right.label.to_lowercase()))
                .then_with(|| left.label.cmp(&right.label))
        });
        groups
    }

    pub fn remove(&mut self, repository_id: Uuid, cx: &mut Context<Self>) {
        if !self
            .repositories
            .iter()
            .any(|row| row.record.id == repository_id)
        {
            return;
        }
        self.remove_repository_record(repository_id);
        self.record_diagnostic(
            "repository_removed",
            &format!("repository_id={repository_id}"),
        );
        sync_config_records(&mut self.config, &self.repositories);
        self.persist();
        self.show_info(
            "Repository removed from GitSync. Files were not changed.",
            cx,
        );
        self.publish(cx);
        self.start_queued_refreshes(cx);
    }

    /// Pauses or resumes a single repository, cancelling an in-flight refresh
    /// when it is paused. Returns whether the state changed. Callers persist and
    /// publish.
    fn set_record_paused(&mut self, repository_id: Uuid, paused: bool) -> bool {
        let Some(row) = self
            .repositories
            .iter_mut()
            .find(|row| row.record.id == repository_id)
        else {
            return false;
        };
        if row.record.paused == paused {
            return false;
        }
        row.record.paused = paused;
        row.record.updated_at = std::time::SystemTime::now();
        if paused {
            let was_active = row.refreshing;
            if let Some(cancellation) = row.cancellation.take() {
                cancellation.cancel();
            }
            row.refreshing = false;
            row.refresh_pending = false;
            row.refresh_sequence += 1;
            if was_active {
                row.refresh_started_at = None;
                self.active_refreshes = self.active_refreshes.saturating_sub(1);
            }
            self.queued_refreshes
                .retain(|queued| *queued != repository_id);
        }
        true
    }

    /// Cancels and forgets a single repository without syncing or persisting, so
    /// callers can batch removals.
    fn remove_repository_record(&mut self, repository_id: Uuid) {
        let mut was_active = false;
        if let Some(row) = self
            .repositories
            .iter_mut()
            .find(|row| row.record.id == repository_id)
        {
            was_active = row.refreshing;
            if let Some(cancellation) = row.cancellation.take() {
                cancellation.cancel();
            }
        }
        if was_active {
            self.active_refreshes = self.active_refreshes.saturating_sub(1);
        }
        self.queued_refreshes
            .retain(|queued| *queued != repository_id);
        self.repositories
            .retain(|row| row.record.id != repository_id);
        self.cache.snapshots.remove(&repository_id);
        self.cache.notification_fingerprints.remove(&repository_id);
    }

    pub fn clear_message(&mut self, cx: &mut Context<Self>) {
        self.message = None;
        self.publish(cx);
    }

    pub fn reset(&mut self, cx: &mut Context<Self>) {
        for row in &mut self.repositories {
            if let Some(cancellation) = row.cancellation.take() {
                cancellation.cancel();
            }
        }
        self.config = AppConfig::default();
        self.cache = SnapshotCache::default();
        self.repositories.clear();
        self.adding_repository = false;
        self.scan_progress = None;
        self.add_generation += 1;
        self.testing_git = false;
        self.active_refreshes = 0;
        self.pending_notifications.clear();
        self.queued_refreshes.clear();
        self.last_refresh_outcome = None;
        self.tested_git_version = None;
        if let Some(cancellation) = self.add_cancellation.take() {
            cancellation.cancel();
        }
        if let Some(cancellation) = self.git_test_cancellation.take() {
            cancellation.cancel();
        }
        // The login item is registered with macOS, outside the config file,
        // so a reset must remove it there too.
        let _ = crate::macos::set_launch_at_login(false);
        cx.set_global(RepositoryGroupState::default());
        self.show_info(
            "GitSync settings and cached statuses were reset. Repository files were unchanged.",
            cx,
        );
        apply_theme_preference(ThemePreference::System, cx);
        self.persist();
        self.publish(cx);
        cx.refresh_windows();
    }

    /// Adds the repositories a scan found. Returns the ids that were added,
    /// or `None` when the result belongs to a canceled or older scan.
    fn finish_add(
        &mut self,
        generation: u64,
        destination: Option<String>,
        root: &Path,
        result: Result<ScanResult, String>,
        cx: &mut Context<Self>,
    ) -> Option<Vec<Uuid>> {
        if generation != self.add_generation {
            return None;
        }
        self.adding_repository = false;
        self.add_cancellation = None;
        self.scan_progress = None;
        let finish = |model: &mut Self, outcome: Result<String, String>, cx: &mut Context<Self>| {
            match (&destination, outcome) {
                (Some(_), result) => {
                    match &result {
                        Ok(message) => model.show_info(message.clone(), cx),
                        // The remote window shows its own errors. When the
                        // user closed it during the scan, show the error here
                        // so it is not lost.
                        Err(error) if !crate::settings::remote_window_open(cx) => {
                            model.show_error(error.clone());
                        }
                        Err(_) => {}
                    }
                    cx.emit(ScanEvent::RemoteFinished {
                        result: result.map(|_| ()),
                    });
                }
                (None, Ok(message)) => model.show_info(message, cx),
                (None, Err(error)) => model.show_error(error),
            }
            model.publish(cx);
        };
        let mut scan = match result {
            Ok(scan) => scan,
            Err(error) => {
                finish(self, Err(error), cx);
                return None;
            }
        };
        if scan.discovered.is_empty() {
            finish(
                self,
                Err(with_scan_notes(
                    "No Git working trees were found in that folder.".to_owned(),
                    &scan,
                )),
                cx,
            );
            return None;
        }

        let mut skipped = 0;
        let mut last_issue = None;
        let mut candidates = Vec::new();
        for discovered in std::mem::take(&mut scan.discovered) {
            let invalid = matches!(
                &discovered.snapshot.issue,
                Some(RepositoryIssue::Unavailable { .. })
            );
            let Some(canonical_path) = discovered.snapshot.root.clone().filter(|_| !invalid) else {
                last_issue = Some(issue_message(
                    discovered.snapshot.issue.as_ref(),
                    "A discovered folder is not a supported Git working tree.",
                ));
                skipped += 1;
                continue;
            };
            candidates.push((discovered, canonical_path));
        }

        // Re-adding a folder syncs it: repositories already monitored are
        // kept, one that moved here takes over its old record, and records
        // in this folder whose path is gone are dropped.
        let existing: Vec<ExistingRecord> = self
            .repositories
            .iter()
            .filter(|row| row.record.ssh_destination == destination)
            .map(|row| ExistingRecord {
                id: row.record.id,
                selected_path: row.record.selected_path.clone(),
                canonical_path: row.record.canonical_path.clone(),
                missing: path_is_missing(&row.snapshot),
            })
            .collect();
        let found: Vec<PathBuf> = candidates
            .iter()
            .map(|(_, canonical_path)| canonical_path.clone())
            .collect();
        let plan = plan_scan(&existing, &found, root, scan.truncated);
        skipped += plan.already;
        let mut candidates: Vec<Option<(DiscoveredRepository, PathBuf)>> =
            candidates.into_iter().map(Some).collect();

        let mut moved = 0;
        for (repository_id, index) in &plan.relocate {
            if let Some((discovered, canonical_path)) = candidates[*index].take() {
                self.apply_relocation(
                    *repository_id,
                    discovered.selected_path,
                    canonical_path,
                    discovered.snapshot,
                );
                moved += 1;
            }
        }
        for repository_id in &plan.remove {
            self.remove_repository_record(*repository_id);
            self.record_diagnostic(
                "repository_removed",
                &format!("repository_id={repository_id} reason=missing"),
            );
        }
        let removed = plan.remove.len();

        let mut added = Vec::new();
        for index in &plan.add {
            let Some((discovered, canonical_path)) = candidates[*index].take() else {
                continue;
            };
            let record = RepositoryRecord {
                id: Uuid::new_v4(),
                selected_path: discovered.selected_path,
                canonical_path,
                ssh_destination: discovered.ssh_destination,
                display_name: None,
                paused: false,
                network_access_allowed: false,
                added_at: std::time::SystemTime::now(),
                updated_at: std::time::SystemTime::now(),
            };
            let repository_id = record.id;
            self.cache
                .snapshots
                .insert(record.id, discovered.snapshot.clone());
            self.repositories.push(RepositoryRow {
                record,
                snapshot: discovered.snapshot,
                refreshing: false,
                refresh_pending: false,
                refresh_sequence: 0,
                cancellation: None,
                refresh_started_at: None,
            });
            self.record_diagnostic(
                "repository_added",
                &format!("repository_id={repository_id}"),
            );
            added.push(repository_id);
        }
        sync_config_records(&mut self.config, &self.repositories);
        self.persist();
        if added.is_empty() && moved == 0 && removed == 0 {
            let message = last_issue
                .unwrap_or_else(|| "All discovered repositories are already monitored.".into());
            finish(self, Err(with_scan_notes(message, &scan)), cx);
            return None;
        }
        // A new group opens expanded so the user sees what was added.
        let group_key = repository_group_key(destination.as_deref());
        cx.global_mut::<RepositoryGroupState>()
            .set_collapsed(&group_key, false);
        let message = scan_summary(added.len(), moved, removed, skipped);
        finish(self, Ok(with_scan_notes(message, &scan)), cx);
        Some(added)
    }

    /// Points a monitored repository at another folder, for one that moved.
    /// `new_path` is a local path (`~/` allowed) or, on an SSH computer, a
    /// path in the remote home (`~/Git/app`) or an absolute one. The record
    /// keeps its name, consent and pause state.
    pub fn relocate_repository(
        &mut self,
        repository_id: Uuid,
        new_path: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self
            .repositories
            .iter()
            .find(|row| row.record.id == repository_id)
        else {
            return;
        };
        let destination = row.record.ssh_destination.clone();
        let trimmed = new_path.trim();
        if trimmed.is_empty() {
            return;
        }
        let path = match &destination {
            Some(_) => match normalize_remote_root(trimmed) {
                Ok(path) => path,
                Err(error) => {
                    self.show_error(error.diagnostic);
                    self.publish(cx);
                    return;
                }
            },
            None => expand_home(trimmed),
        };
        if paths_match(&row.record.selected_path, &path)
            || paths_match(&row.record.canonical_path, &path)
        {
            return;
        }
        let options = self.refresh_options(FetchMode::Offline(OfflineReason::ConsentNotGranted));
        let cancellation = CancellationToken::default();
        let check_path = path.clone();
        self.show_info(format!("Checking {}…", path.display()), cx);
        self.publish(cx);
        cx.spawn(async move |this, cx| {
            let snapshot = cx
                .background_spawn(async move {
                    match destination {
                        Some(destination) => refresh_remote_repository(
                            &destination,
                            &check_path,
                            &options,
                            None,
                            &cancellation,
                        ),
                        None => refresh_repository(&check_path, &options, None, &cancellation),
                    }
                })
                .await;
            this.update(cx, |model, cx| {
                model.finish_relocate(repository_id, path, snapshot, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_relocate(
        &mut self,
        repository_id: Uuid,
        selected_path: PathBuf,
        snapshot: RepositorySnapshot,
        cx: &mut Context<Self>,
    ) {
        let invalid = matches!(&snapshot.issue, Some(RepositoryIssue::Unavailable { .. }));
        let Some(canonical_path) = snapshot.root.clone().filter(|_| !invalid) else {
            self.show_error(issue_message(
                snapshot.issue.as_ref(),
                "That folder is not a Git working tree.",
            ));
            self.publish(cx);
            return;
        };
        let Some(destination) = self
            .repositories
            .iter()
            .find(|row| row.record.id == repository_id)
            .map(|row| row.record.ssh_destination.clone())
        else {
            return;
        };
        if let Some(other) = self.repositories.iter().find(|row| {
            row.record.id != repository_id
                && row.record.ssh_destination == destination
                && paths_match(&row.record.canonical_path, &canonical_path)
        }) {
            let name = other.record.display_name();
            self.show_error(format!("{name} already monitors that folder."));
            self.publish(cx);
            return;
        }
        self.apply_relocation(repository_id, selected_path, canonical_path, snapshot);
        sync_config_records(&mut self.config, &self.repositories);
        self.persist();
        self.show_info("Repository moved to the new folder.", cx);
        self.publish(cx);
        self.refresh(repository_id, cx);
    }

    /// Moves a record to a new folder and takes the snapshot checked there,
    /// cancelling a check of the old folder. Callers persist and publish.
    fn apply_relocation(
        &mut self,
        repository_id: Uuid,
        selected_path: PathBuf,
        canonical_path: PathBuf,
        snapshot: RepositorySnapshot,
    ) {
        let Some(row) = self
            .repositories
            .iter_mut()
            .find(|row| row.record.id == repository_id)
        else {
            return;
        };
        row.record.selected_path = selected_path;
        row.record.canonical_path = canonical_path;
        row.record.updated_at = std::time::SystemTime::now();
        let was_active = row.refreshing;
        if let Some(cancellation) = row.cancellation.take() {
            cancellation.cancel();
        }
        row.refreshing = false;
        row.refresh_pending = false;
        row.refresh_sequence += 1;
        row.refresh_started_at = None;
        row.snapshot = snapshot.clone();
        if was_active {
            self.active_refreshes = self.active_refreshes.saturating_sub(1);
        }
        self.queued_refreshes
            .retain(|queued| *queued != repository_id);
        self.cache.snapshots.insert(repository_id, snapshot);
        self.record_diagnostic(
            "repository_moved",
            &format!("repository_id={repository_id}"),
        );
    }

    fn finish_refresh(
        &mut self,
        repository_id: Uuid,
        sequence: u64,
        snapshot: RepositorySnapshot,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self
            .repositories
            .iter_mut()
            .find(|row| row.record.id == repository_id)
        else {
            return;
        };
        if row.refresh_sequence != sequence {
            return;
        }
        self.active_refreshes = self.active_refreshes.saturating_sub(1);
        row.refreshing = false;
        row.cancellation = None;
        let refresh_pending = std::mem::take(&mut row.refresh_pending);
        let duration_ms = row
            .refresh_started_at
            .take()
            .map(|started| started.elapsed().as_millis())
            .unwrap_or_default();

        // A canceled refresh says nothing about the repository. Keep the last
        // real result instead of flashing a failure until the re-run ends.
        if !is_cancelled(&snapshot) {
            let display_name = row.record.display_name();
            let diagnostic_outcome = diagnostic_outcome(&snapshot);
            self.last_refresh_outcome = Some(diagnostic_outcome.clone());
            let transition = notification_transition(
                self.cache
                    .notification_fingerprints
                    .get(&repository_id)
                    .copied(),
                &snapshot,
            );
            row.snapshot = snapshot.clone();
            self.cache.snapshots.insert(repository_id, snapshot);
            match transition {
                NotificationTransition::Ignore => {}
                NotificationTransition::Record(Some(fingerprint)) => {
                    self.cache
                        .notification_fingerprints
                        .insert(repository_id, fingerprint);
                }
                NotificationTransition::Record(None) => {
                    self.cache.notification_fingerprints.remove(&repository_id);
                }
                NotificationTransition::Notify(fingerprint) => {
                    self.cache
                        .notification_fingerprints
                        .insert(repository_id, fingerprint);
                    if self.should_notify(fingerprint) {
                        self.queue_notification(repository_id, display_name, fingerprint, cx);
                    }
                }
            }
            self.record_diagnostic(
                "refresh_completed",
                &format!(
                    "repository_id={repository_id} duration_ms={duration_ms} outcome={diagnostic_outcome}"
                ),
            );
            self.schedule_cache_save(cx);
        }
        self.publish(cx);
        if refresh_pending && !self.queued_refreshes.contains(&repository_id) {
            self.queued_refreshes.push_back(repository_id);
        }
        self.start_queued_refreshes(cx);
    }

    /// Commits to push are the user's own work, so they notify only when
    /// the user opted in. Remote changes always notify.
    fn should_notify(&self, fingerprint: NotificationFingerprint) -> bool {
        self.notifications_enabled()
            && (self.notify_on_push()
                || !matches!(fingerprint, NotificationFingerprint::Ahead { .. }))
    }

    /// Collects notifications for a short time so that one refresh pass
    /// raises one banner instead of one per repository.
    fn queue_notification(
        &mut self,
        repository_id: Uuid,
        display_name: String,
        fingerprint: NotificationFingerprint,
        cx: &mut Context<Self>,
    ) {
        self.pending_notifications
            .retain(|(id, _, _)| *id != repository_id);
        self.pending_notifications
            .push((repository_id, display_name, fingerprint));
        if self.notification_flush_scheduled {
            return;
        }
        self.notification_flush_scheduled = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(NOTIFICATION_COALESCE_DELAY)
                .await;
            this.update(cx, |model, cx| {
                model.notification_flush_scheduled = false;
                let pending = std::mem::take(&mut model.pending_notifications);
                post_notifications(&pending, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Writes the cache shortly after a refresh, so a refresh pass of many
    /// repositories causes one write instead of one per repository.
    fn schedule_cache_save(&mut self, cx: &mut Context<Self>) {
        if self.cache_save_scheduled || self.read_only {
            return;
        }
        self.cache_save_scheduled = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CACHE_SAVE_DELAY).await;
            this.update(cx, |model, _| model.flush_cache()).ok();
        })
        .detach();
    }

    /// Writes any pending cache changes now. Called before the app quits.
    pub fn flush_cache(&mut self) {
        self.cache_save_scheduled = false;
        if self.read_only {
            return;
        }
        if let Err(error) = self.store.save_cache(&self.cache) {
            let _ = self
                .store
                .append_diagnostic("persistence_failed", "document=cache");
            self.show_error(error.to_string());
        }
    }

    fn persist(&mut self) {
        if self.read_only {
            return;
        }
        if let Err(error) = self.store.save_config(&self.config) {
            let _ = self
                .store
                .append_diagnostic("persistence_failed", "document=config");
            self.show_error(error.to_string());
            return;
        }
        self.flush_cache();
    }

    fn publish(&self, cx: &mut Context<Self>) {
        if cx.has_global::<StatusItemController>() {
            let s = self.status_summary();
            cx.global_mut::<StatusItemController>().set_summary(s);
        }
        cx.notify();
    }

    fn refresh_options(&self, fetch_mode: FetchMode) -> RefreshOptions {
        RefreshOptions {
            git_executable: self.config.settings.git_executable.clone(),
            fetch_mode,
            timeout: Duration::from_secs(self.config.settings.fetch_timeout_seconds),
            github_cli: find_github_cli(),
        }
    }

    fn record_diagnostic(&mut self, event: &str, detail: &str) {
        if self.read_only {
            return;
        }
        if let Err(error) = self.store.append_diagnostic(event, detail) {
            self.show_error(format!("Diagnostics logging failed: {error}"));
        }
    }
}

impl Drop for AppModel {
    fn drop(&mut self) {
        if let Some(cancellation) = self.add_cancellation.take() {
            cancellation.cancel();
        }
        if let Some(cancellation) = self.git_test_cancellation.take() {
            cancellation.cancel();
        }
        for row in &mut self.repositories {
            if let Some(cancellation) = row.cancellation.take() {
                cancellation.cancel();
            }
        }
    }
}

fn sync_config_records(config: &mut AppConfig, rows: &[RepositoryRow]) {
    config.repositories = rows.iter().map(|row| row.record.clone()).collect();
}

pub(crate) struct ScanResult {
    discovered: Vec<DiscoveredRepository>,
    skipped_directories: usize,
    truncated: bool,
}

/// Runs the first offline check of every discovered root, counting progress
/// so the UI can show it.
fn check_discovered(
    discovery: Discovery,
    destination: Option<&str>,
    options: &RefreshOptions,
    progress: &ScanProgress,
    cancellation: &CancellationToken,
) -> ScanResult {
    progress
        .found
        .store(discovery.repositories.len(), Ordering::Relaxed);
    let mut discovered = Vec::with_capacity(discovery.repositories.len());
    for root in discovery.repositories {
        if cancellation.is_cancelled() {
            break;
        }
        let snapshot = match destination {
            Some(destination) => {
                refresh_remote_repository(destination, &root, options, None, cancellation)
            }
            None => refresh_repository(&root, options, None, cancellation),
        };
        progress.checked.fetch_add(1, Ordering::Relaxed);
        discovered.push(DiscoveredRepository {
            selected_path: root,
            ssh_destination: destination.map(str::to_owned),
            snapshot,
        });
    }
    ScanResult {
        discovered,
        skipped_directories: discovery.skipped_directories,
        truncated: discovery.truncated,
    }
}

fn repository_count_label(count: usize, truncated: bool) -> String {
    match (count, truncated) {
        (_, true) => format!("More than {count} repositories"),
        (0, false) => "No repositories".to_owned(),
        (1, false) => "1 repository".to_owned(),
        (count, false) => format!("{count} repositories"),
    }
}

fn with_scan_notes(mut message: String, scan: &ScanResult) -> String {
    if scan.skipped_directories > 0 {
        message.push_str(&format!(
            " {} unreadable {} skipped.",
            scan.skipped_directories,
            if scan.skipped_directories == 1 {
                "folder was"
            } else {
                "folders were"
            }
        ));
    }
    if scan.truncated {
        message.push_str(" The scan stopped at 1,000 repositories; choose a smaller folder.");
    }
    message
}

fn grant_network_access(rows: &mut [RepositoryRow], repository_ids: &[Uuid]) -> usize {
    let now = std::time::SystemTime::now();
    let mut allowed = 0;
    for row in rows {
        if repository_ids.contains(&row.record.id) && !row.record.network_access_allowed {
            row.record.network_access_allowed = true;
            row.record.updated_at = now;
            allowed += 1;
        }
    }
    allowed
}

fn issue_message(issue: Option<&RepositoryIssue>, fallback: &str) -> String {
    match issue {
        Some(RepositoryIssue::Unavailable { diagnostic, .. })
        | Some(RepositoryIssue::FetchFailed { diagnostic, .. })
            if !diagnostic.is_empty() =>
        {
            diagnostic.clone()
        }
        Some(RepositoryIssue::NoUpstream) => "The current branch has no upstream.".to_owned(),
        Some(RepositoryIssue::DetachedHead) => "The repository has a detached HEAD.".to_owned(),
        _ => fallback.to_owned(),
    }
}

fn diagnostic_outcome(snapshot: &RepositorySnapshot) -> String {
    match &snapshot.issue {
        Some(RepositoryIssue::NoUpstream) => "no_upstream".to_owned(),
        Some(RepositoryIssue::DetachedHead) => "detached_head".to_owned(),
        Some(RepositoryIssue::Unavailable { kind, .. }) => {
            format!("local_error:{kind:?}")
        }
        Some(RepositoryIssue::FetchFailed { kind, .. }) => {
            format!("fetch_error:{kind:?}")
        }
        None => match snapshot.synchronization {
            Some(gitsync::domain::SynchronizationState::UpToDate) => "up_to_date".to_owned(),
            Some(gitsync::domain::SynchronizationState::Ahead { count }) => {
                format!("ahead:{count}")
            }
            Some(gitsync::domain::SynchronizationState::Behind { count }) => {
                format!("behind:{count}")
            }
            Some(gitsync::domain::SynchronizationState::Diverged { ahead, behind }) => {
                format!("diverged:{ahead}:{behind}")
            }
            None => "unchecked".to_owned(),
        },
    }
}

fn paths_match(left: &Path, right: &Path) -> bool {
    if left == right || lexical_path(left) == lexical_path(right) {
        return true;
    }
    // Filesystem canonicalization is only meaningful for local paths that
    // exist on this Mac. Remote canonical paths (for example
    // `/home/developer/Git/project`) never resolve here, so a failed lookup
    // must not count as a mismatch override: it simply falls through to
    // `false` after the lexical comparison above.
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Best-effort lexical normalization without filesystem access: drops `.`
/// segments (including a leading `./` left over from a remote
/// `find .` scan) and resolves `..` segments. Trailing separators are
/// already insignificant to `Path` equality.
/// A monitored repository on the computer being scanned, as `plan_scan`
/// sees it.
#[derive(Clone, Debug)]
struct ExistingRecord {
    id: Uuid,
    selected_path: PathBuf,
    canonical_path: PathBuf,
    /// The last check found no folder at the record's path.
    missing: bool,
}

/// What a scan of one folder changes, by index into the found roots.
#[derive(Debug, Default, PartialEq)]
struct ScanPlan {
    add: Vec<usize>,
    /// A monitored repository whose folder is gone, moved to a found root
    /// with the same folder name.
    relocate: Vec<(Uuid, usize)>,
    /// Monitored repositories in the scanned folder whose path is gone.
    remove: Vec<Uuid>,
    /// Found roots that are already monitored.
    already: usize,
}

/// Decides how a scan of `root` merges with the repositories already
/// monitored on the same computer. A found root that is already monitored
/// is kept. A new one takes over a record whose folder is gone when exactly
/// one such record and exactly one new root share the folder name (a
/// repository moved, for example from `Private/app` to `Public/app`); any
/// other new root is added. Records under `root` whose folder is gone and
/// that nothing took over are removed, unless the scan stopped early.
fn plan_scan(
    existing: &[ExistingRecord],
    found: &[PathBuf],
    root: &Path,
    truncated: bool,
) -> ScanPlan {
    let mut plan = ScanPlan::default();
    let mut seen = Vec::new();
    let mut fresh = Vec::new();
    for (index, canonical_path) in found.iter().enumerate() {
        match existing
            .iter()
            .find(|record| paths_match(&record.canonical_path, canonical_path))
        {
            Some(record) => {
                seen.push(record.id);
                plan.already += 1;
            }
            None => fresh.push(index),
        }
    }
    let missing: Vec<&ExistingRecord> = existing
        .iter()
        .filter(|record| record.missing && !seen.contains(&record.id))
        .collect();
    for &index in &fresh {
        let name = found[index].file_name();
        let fresh_with_name = fresh
            .iter()
            .filter(|&&other| found[other].file_name() == name)
            .count();
        let candidates: Vec<&&ExistingRecord> = missing
            .iter()
            .filter(|record| record.canonical_path.file_name() == name)
            .collect();
        match candidates.as_slice() {
            [record] if name.is_some() && fresh_with_name == 1 => {
                plan.relocate.push((record.id, index));
            }
            _ => plan.add.push(index),
        }
    }
    if !truncated {
        let root = lexical_path(root);
        for record in missing {
            let moved = plan.relocate.iter().any(|(id, _)| *id == record.id);
            let inside =
                root == Path::new(".") || lexical_path(&record.selected_path).starts_with(&root);
            if !moved && inside {
                plan.remove.push(record.id);
            }
        }
    }
    plan
}

/// The last check found no folder at the repository's path.
fn path_is_missing(snapshot: &RepositorySnapshot) -> bool {
    matches!(
        &snapshot.issue,
        Some(RepositoryIssue::Unavailable {
            kind: ErrorKind::PathMissing,
            ..
        })
    )
}

/// `Added 2 Git projects. Moved 1. Removed 1 that no longer exists. Skipped 30.`
fn scan_summary(added: usize, moved: usize, removed: usize, skipped: usize) -> String {
    let mut parts = Vec::new();
    if added > 0 {
        parts.push(format!(
            "Added {added} Git {}.",
            if added == 1 { "project" } else { "projects" }
        ));
    }
    if moved > 0 {
        parts.push(format!(
            "Moved {moved} to {}.",
            if moved == 1 {
                "its new folder"
            } else {
                "their new folders"
            }
        ));
    }
    if removed > 0 {
        parts.push(format!(
            "Removed {removed} that no longer {}.",
            if removed == 1 { "exists" } else { "exist" }
        ));
    }
    if skipped > 0 {
        parts.push(format!("Skipped {skipped}."));
    }
    parts.join(" ")
}

/// `~/x` on this Mac, from the user's home.
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

fn lexical_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

fn normalized_display_name(name: &str) -> Option<String> {
    let normalized = name.trim();
    (!normalized.is_empty()).then(|| normalized.chars().take(80).collect())
}

fn refresh_priority(category: RepositoryCategory) -> u8 {
    match category {
        RepositoryCategory::Actionable => 0,
        RepositoryCategory::Problem => 1,
        RepositoryCategory::Uncommitted => 2,
        RepositoryCategory::Unverified => 3,
        RepositoryCategory::Current => 4,
        RepositoryCategory::Paused => 5,
    }
}

fn notification_body(fingerprint: NotificationFingerprint) -> String {
    match fingerprint {
        NotificationFingerprint::Ahead { count } => {
            format!(
                "{count} {} ready to push.",
                plural(count, "commit", "commits")
            )
        }
        NotificationFingerprint::Behind { count } => {
            format!(
                "{count} {} ready to pull.",
                plural(count, "commit", "commits")
            )
        }
        NotificationFingerprint::Diverged { ahead, behind } => {
            format!("Diverged: {ahead} ahead and {behind} behind.")
        }
    }
}

/// Posts one banner for one repository, or one combined banner when several
/// repositories changed in the same refresh pass.
fn post_notifications(
    pending: &[(Uuid, String, NotificationFingerprint)],
    cx: &mut Context<AppModel>,
) {
    match pending {
        [] => {}
        [(repository_id, display_name, fingerprint)] => {
            cx.show_system_notification(SystemNotification {
                tag: format!("repository:{repository_id}").into(),
                title: format!("{display_name} needs attention").into(),
                body: notification_body(*fingerprint).into(),
                actions: Vec::new(),
            });
        }
        many => {
            let body = many
                .iter()
                .take(4)
                .map(|(_, name, fingerprint)| {
                    format!("{name}: {}", short_notification_state(*fingerprint))
                })
                .collect::<Vec<_>>()
                .join(" · ");
            let more = many.len().saturating_sub(4);
            cx.show_system_notification(SystemNotification {
                tag: "repositories".into(),
                title: format!("{} repositories need attention", many.len()).into(),
                body: if more == 0 {
                    body.into()
                } else {
                    format!("{body} · and {more} more").into()
                },
                actions: Vec::new(),
            });
        }
    }
}

fn short_notification_state(fingerprint: NotificationFingerprint) -> String {
    match fingerprint {
        NotificationFingerprint::Ahead { count } => format!("{count} to push"),
        NotificationFingerprint::Behind { count } => format!("{count} to pull"),
        NotificationFingerprint::Diverged { .. } => "diverged".to_owned(),
    }
}

fn plural<'a>(count: u32, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn enabled_label(enabled: bool) -> &'static str {
    if enabled { "enabled" } else { "disabled" }
}

fn validate_git_executable(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("The Git executable path must be absolute.".to_owned());
    }
    let metadata = path
        .metadata()
        .map_err(|error| format!("The Git executable is unavailable: {error}"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err("The selected Git path is not an executable file.".to_owned());
    }
    Ok(())
}

fn apply_theme_preference(preference: ThemePreference, cx: &mut App) {
    let mode = match preference {
        ThemePreference::System => {
            cx.set_window_appearance(None);
            ThemeMode::from(cx.window_appearance())
        }
        ThemePreference::Light => {
            cx.set_window_appearance(Some(WindowAppearance::Light));
            ThemeMode::Light
        }
        ThemePreference::Dark => {
            cx.set_window_appearance(Some(WindowAppearance::Dark));
            ThemeMode::Dark
        }
    };
    install_dark_palette(cx);
    Theme::change(mode, None, cx);
}

/// The pinned GPUI Kit version, shown in the diagnostic summary. A test keeps it equal to `Cargo.toml`.
const GPUI_KIT_VERSION: &str = "0.11.0";

/// Names of the themes GitSync installs over GPUI Kit's defaults.
const DARK_PALETTE_NAME: &str = "GitSync Dark";
const LIGHT_PALETTE_NAME: &str = "GitSync Light";

/// Replaces the default dark theme with GitSync's palette: a charcoal panel
/// (`#232325`), a darker rail and title bar (`#1c1c1e`), quiet borders
/// (`#343436`), near-white text and bars (`#ededed`), gray secondary text
/// (`#888888`), and a lime accent (`#bfff00`). Every window and component
/// uses it in dark mode; light mode keeps the stock light colors. Both
/// themes use GitSync's rem size, so controls match native small controls.
fn install_dark_palette(cx: &mut App) {
    if !cx.has_global::<Theme>() {
        return;
    }
    let theme = cx.global_mut::<Theme>();
    if theme.light_theme.name.as_ref() != LIGHT_PALETTE_NAME {
        let mut config = (*theme.light_theme).clone();
        config.name = LIGHT_PALETTE_NAME.into();
        config.font_size = Some(crate::ui::REM_SIZE);
        theme.light_theme = std::rc::Rc::new(config);
    }
    if theme.dark_theme.name.as_ref() == DARK_PALETTE_NAME {
        return;
    }
    let mut config = (*theme.dark_theme).clone();
    config.name = DARK_PALETTE_NAME.into();
    config.font_size = Some(crate::ui::REM_SIZE);
    let colors = &mut config.colors;
    let set = |slot: &mut Option<gpui_kit::SharedString>, value: &'static str| {
        *slot = Some(value.into());
    };
    set(&mut colors.background, "#232325");
    set(&mut colors.foreground, "#ededed");
    set(&mut colors.border, "#343436");
    set(&mut colors.input, "#343436");
    set(&mut colors.ring, "#5c5c5d");
    set(&mut colors.muted, "#2b2b2d");
    set(&mut colors.muted_foreground, "#888888");
    set(&mut colors.accent, "#2e2e30");
    set(&mut colors.accent_foreground, "#ededed");
    set(&mut colors.secondary, "#28282a");
    set(&mut colors.secondary_hover, "#2f2f31");
    set(&mut colors.secondary_active, "#343436");
    set(&mut colors.secondary_foreground, "#ededed");
    set(&mut colors.primary, "#ededed");
    set(&mut colors.primary_hover, "#ffffff");
    set(&mut colors.primary_active, "#d6d6d6");
    set(&mut colors.primary_foreground, "#1c1c1e");
    set(&mut colors.popover, "#28282a");
    set(&mut colors.popover_foreground, "#ededed");
    set(&mut colors.list, "#232325");
    set(&mut colors.list_hover, "#2e2e30");
    set(&mut colors.list_active, "#2e2e30");
    set(&mut colors.title_bar, "#1c1c1e");
    set(&mut colors.title_bar_border, "#2e2e30");
    set(&mut colors.tab_bar, "#1c1c1e");
    set(&mut colors.tab_bar_segmented, "#1c1c1e");
    set(&mut colors.tab_active, "#343436");
    set(&mut colors.tab_active_foreground, "#ededed");
    set(&mut colors.tab_foreground, "#888888");
    set(&mut colors.switch, "#3a3a3c");
    set(&mut colors.scrollbar_thumb, "#3a3a3c");
    set(&mut colors.success, "#bfff00");
    set(&mut colors.success_foreground, "#1c1c1e");
    set(&mut colors.warning, "#ffcc00");
    set(&mut colors.danger, "#ff483a");
    theme.dark_theme = std::rc::Rc::new(config);
}

/// Applies the light or dark theme for `appearance`, keeping GitSync's dark
/// palette. Windows call this when macOS changes appearance.
pub(crate) fn follow_system_appearance(
    appearance: WindowAppearance,
    window: &mut gpui_kit::Window,
    cx: &mut App,
) {
    install_dark_palette(cx);
    Theme::change(ThemeMode::from(appearance), Some(window), cx);
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use gitsync::domain::{SynchronizationState, WorkingTreeState};

    use super::*;

    #[test]
    fn diagnostic_gpui_kit_version_matches_the_cargo_pin() {
        let pin = format!("gpui-kit = \"={GPUI_KIT_VERSION}\"");
        assert!(include_str!("../Cargo.toml").contains(&pin));
    }

    #[test]
    fn stale_status_is_unverified_unless_commits_are_known() {
        let mut row = repository_row();
        row.snapshot.synchronization = Some(SynchronizationState::UpToDate);
        row.snapshot.freshness = Freshness::Stale {
            last_successful_fetch_at: Some(SystemTime::now()),
            reason: StaleReason::CachedAtStartup,
        };
        assert_eq!(row.category(), RepositoryCategory::Unverified);

        // Local commits still need a push even when the remote is unknown.
        row.snapshot.synchronization = Some(SynchronizationState::Ahead { count: 1 });
        assert_eq!(row.category(), RepositoryCategory::Actionable);

        row.snapshot.synchronization = Some(SynchronizationState::UpToDate);
        row.snapshot.freshness = Freshness::Fresh {
            fetched_at: SystemTime::now(),
        };
        assert_eq!(row.category(), RepositoryCategory::Current);
    }

    #[test]
    fn repository_count_preview_reads_naturally() {
        assert_eq!(repository_count_label(0, false), "No repositories");
        assert_eq!(repository_count_label(1, false), "1 repository");
        assert_eq!(repository_count_label(14, false), "14 repositories");
        assert_eq!(
            repository_count_label(1_000, true),
            "More than 1000 repositories"
        );
    }

    #[test]
    fn only_lasting_failures_are_problems() {
        let mut row = repository_row();
        row.snapshot.synchronization = Some(SynchronizationState::UpToDate);
        row.snapshot.issue = Some(RepositoryIssue::FetchFailed {
            kind: ErrorKind::NetworkUnavailable,
            diagnostic: String::new(),
        });
        assert_eq!(row.category(), RepositoryCategory::Unverified);

        row.snapshot.issue = Some(RepositoryIssue::FetchFailed {
            kind: ErrorKind::AuthenticationRequired,
            diagnostic: String::new(),
        });
        assert_eq!(row.category(), RepositoryCategory::Problem);

        row.snapshot.issue = Some(RepositoryIssue::NoUpstream);
        assert_eq!(row.category(), RepositoryCategory::Problem);
    }

    #[test]
    fn repository_management_normalizes_names_and_validates_git() {
        assert_eq!(
            normalized_display_name("  Example  "),
            Some("Example".to_owned())
        );
        assert_eq!(normalized_display_name("  "), None);
        assert_eq!(
            normalized_display_name(&"x".repeat(81))
                .expect("non-empty name")
                .chars()
                .count(),
            80
        );
        assert!(
            refresh_priority(RepositoryCategory::Actionable)
                < refresh_priority(RepositoryCategory::Current)
        );
        assert!(validate_git_executable(Path::new("git")).is_err());
        assert!(
            validate_git_executable(
                &std::env::current_exe().expect("test executable path should resolve")
            )
            .is_ok()
        );
    }

    #[test]
    fn bulk_fetch_consent_only_changes_repositories_that_need_it() {
        let mut first = repository_row();
        first.record.id = Uuid::new_v4();
        first.record.network_access_allowed = false;
        let mut second = repository_row();
        second.record.id = Uuid::new_v4();
        let ids = vec![first.record.id, second.record.id];
        let mut rows = vec![first, second];

        assert_eq!(grant_network_access(&mut rows, &ids), 1);
        assert!(rows.iter().all(|row| row.record.network_access_allowed));
        assert_eq!(grant_network_access(&mut rows, &ids), 0);
    }

    #[test]
    fn status_summary_totals_pending_pushes_and_pulls() {
        let mut model = AppModel::empty_for_error("test".to_owned());
        model
            .repositories
            .push(actionable_row(SynchronizationState::Ahead { count: 2 }));
        model
            .repositories
            .push(actionable_row(SynchronizationState::Behind { count: 3 }));
        model
            .repositories
            .push(actionable_row(SynchronizationState::Diverged {
                ahead: 1,
                behind: 4,
            }));

        let summary = model.status_summary();

        assert_eq!(summary.actionable, 3);
        assert_eq!(summary.push, 3);
        assert_eq!(summary.pull, 7);
        assert_eq!(summary.actionable + summary.problems, 3);
    }

    #[test]
    fn repository_groups_aggregate_by_remote_computer_with_local_last() {
        let mut model = AppModel::empty_for_error("test".to_owned());

        let mut local = repository_row();
        local.record.id = Uuid::new_v4();
        let mut workstation_a = repository_row();
        workstation_a.record.id = Uuid::new_v4();
        workstation_a.record.ssh_destination = Some("developer@workstation".to_owned());
        let mut workstation_b = repository_row();
        workstation_b.record.id = Uuid::new_v4();
        workstation_b.record.ssh_destination = Some("developer@workstation".to_owned());
        let mut laptop = repository_row();
        laptop.record.id = Uuid::new_v4();
        laptop.record.ssh_destination = Some("me@laptop".to_owned());
        laptop.record.paused = true;

        model.repositories.push(workstation_a);
        model.repositories.push(local);
        model.repositories.push(workstation_b);
        model.repositories.push(laptop);

        let groups = model.repository_groups();

        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].label, "developer@workstation");
        assert_eq!(groups[0].key, "remote:developer@workstation");
        assert_eq!(groups[0].repository_count, 2);
        assert_eq!(groups[1].label, "me@laptop");
        assert!(groups[1].paused);
        assert_eq!(groups[2].label, "This Mac");
        assert_eq!(groups[2].key, "local");
        assert!(groups[2].ssh_destination.is_none());
    }

    #[test]
    fn uncommitted_changes_are_never_up_to_date() {
        let fresh = Freshness::Fresh {
            fetched_at: SystemTime::now(),
        };
        let dirty = WorkingTreeState {
            modified: 6,
            untracked: 6,
            ..WorkingTreeState::default()
        };
        let snapshot = |synchronization, working_tree, freshness| RepositorySnapshot {
            synchronization,
            working_tree,
            freshness,
            ..RepositorySnapshot::default()
        };

        let clean = snapshot(
            Some(SynchronizationState::UpToDate),
            WorkingTreeState::default(),
            fresh.clone(),
        );
        assert_eq!(category_for(false, &clean), RepositoryCategory::Current);

        let level_but_dirty = snapshot(Some(SynchronizationState::UpToDate), dirty, fresh.clone());
        assert_eq!(
            category_for(false, &level_but_dirty),
            RepositoryCategory::Uncommitted
        );
        // Local changes are known even when the remote state is not.
        let unchecked_but_dirty = snapshot(None, dirty, Freshness::Unchecked);
        assert_eq!(
            category_for(false, &unchecked_but_dirty),
            RepositoryCategory::Uncommitted
        );
        // Commits to push or pull, a problem, or a pause still come first.
        let ahead_and_dirty =
            snapshot(Some(SynchronizationState::Ahead { count: 2 }), dirty, fresh);
        assert_eq!(
            category_for(false, &ahead_and_dirty),
            RepositoryCategory::Actionable
        );
        assert_eq!(
            category_for(true, &level_but_dirty),
            RepositoryCategory::Paused
        );
        let broken = RepositorySnapshot {
            issue: Some(RepositoryIssue::DetachedHead),
            ..level_but_dirty
        };
        assert_eq!(category_for(false, &broken), RepositoryCategory::Problem);
    }

    #[test]
    fn renamed_computers_keep_their_default_label() {
        let mut model = AppModel::empty_for_error("test".to_owned());
        let mut remote = repository_row();
        remote.record.id = Uuid::new_v4();
        remote.record.ssh_destination = Some("developer@workstation".to_owned());
        model.repositories.push(remote);

        let names = &mut model.config.computer_names;
        let key = "remote:developer@workstation";
        assert!(set_computer_name(
            names,
            key,
            "developer@workstation",
            "  Build box "
        ));
        assert!(!set_computer_name(
            names,
            key,
            "developer@workstation",
            "Build box"
        ));
        let group = &model.repository_groups()[0];
        assert_eq!(group.label, "Build box");
        assert_eq!(group.default_label, "developer@workstation");

        // A blank name, or the default name, forgets the custom one.
        let names = &mut model.config.computer_names;
        assert!(set_computer_name(names, key, "developer@workstation", ""));
        assert!(!set_computer_name(
            names,
            key,
            "developer@workstation",
            "developer@workstation"
        ));
        assert_eq!(model.repository_groups()[0].label, "developer@workstation");
    }

    #[test]
    fn refresh_capacity_is_counted_per_computer() {
        let mut model = AppModel::empty_for_error("test".to_owned());
        for _ in 0..MAX_REFRESHES_PER_COMPUTER {
            let mut row = repository_row();
            row.record.id = Uuid::new_v4();
            row.record.ssh_destination = Some("developer@workstation".to_owned());
            row.refreshing = true;
            model.repositories.push(row);
            model.active_refreshes += 1;
        }
        // A busy SSH host does not hold up this Mac or another host.
        assert!(!model.has_refresh_capacity(Some("developer@workstation")));
        assert!(model.has_refresh_capacity(None));
        assert!(model.has_refresh_capacity(Some("me@laptop")));

        model.active_refreshes = MAX_CONCURRENT_REFRESHES;
        assert!(!model.has_refresh_capacity(None));
    }

    #[test]
    fn repository_group_paused_only_when_every_member_is_paused() {
        let mut model = AppModel::empty_for_error("test".to_owned());
        let mut first = repository_row();
        first.record.id = Uuid::new_v4();
        first.record.ssh_destination = Some("developer@workstation".to_owned());
        first.record.paused = true;
        let mut second = repository_row();
        second.record.id = Uuid::new_v4();
        second.record.ssh_destination = Some("developer@workstation".to_owned());
        second.record.paused = false;

        model.repositories.push(first);
        model.repositories.push(second);

        let groups = model.repository_groups();
        assert_eq!(groups.len(), 1);
        assert!(!groups[0].paused);

        model.repositories[1].record.paused = true;
        assert!(model.repository_groups()[0].paused);
    }

    #[test]
    fn actionable_rows_contribute_to_group_attention() {
        let mut model = AppModel::empty_for_error("test".to_owned());
        let mut actionable = actionable_row(SynchronizationState::Ahead { count: 1 });
        actionable.record.ssh_destination = Some("developer@workstation".to_owned());
        let mut current = repository_row();
        current.record.id = Uuid::new_v4();
        current.record.ssh_destination = Some("developer@workstation".to_owned());
        current.snapshot.synchronization = Some(SynchronizationState::UpToDate);
        current.snapshot.freshness = Freshness::Fresh {
            fetched_at: SystemTime::now(),
        };

        model.repositories.push(actionable);
        model.repositories.push(current);

        let groups = model.repository_groups();
        assert_eq!(groups[0].repository_count, 2);
        assert_eq!(groups[0].attention, 1);
    }

    fn actionable_row(synchronization: SynchronizationState) -> RepositoryRow {
        let mut row = repository_row();
        row.record.id = Uuid::new_v4();
        row.snapshot.synchronization = Some(synchronization);
        row.snapshot.freshness = Freshness::Fresh {
            fetched_at: SystemTime::now(),
        };
        row
    }

    fn repository_row() -> RepositoryRow {
        RepositoryRow {
            record: RepositoryRecord {
                id: Uuid::nil(),
                selected_path: PathBuf::from("/tmp/repository"),
                canonical_path: PathBuf::from("/tmp/repository"),
                ssh_destination: None,
                display_name: None,
                paused: false,
                network_access_allowed: true,
                added_at: SystemTime::now(),
                updated_at: SystemTime::now(),
            },
            snapshot: RepositorySnapshot::default(),
            refreshing: false,
            refresh_pending: false,
            refresh_sequence: 0,
            cancellation: None,
            refresh_started_at: None,
        }
    }

    fn record(id: u128, selected: &str, canonical: &str, missing: bool) -> ExistingRecord {
        ExistingRecord {
            id: Uuid::from_u128(id),
            selected_path: PathBuf::from(selected),
            canonical_path: PathBuf::from(canonical),
            missing,
        }
    }

    #[test]
    fn rescanning_keeps_monitored_repositories_and_adds_new_ones() {
        let existing = [record(1, "Git/a", "/home/u/Git/a", false)];
        let found = [
            PathBuf::from("/home/u/Git/a"),
            PathBuf::from("/home/u/Git/b"),
        ];
        let plan = plan_scan(&existing, &found, Path::new("Git"), false);
        assert_eq!(plan.already, 1);
        assert_eq!(plan.add, vec![1]);
        assert!(plan.relocate.is_empty() && plan.remove.is_empty());
    }

    #[test]
    fn a_repository_that_moved_takes_over_its_old_record() {
        let existing = [
            record(1, "Git/Private/app", "/home/u/Git/Private/app", true),
            record(2, "Git/Public/other", "/home/u/Git/Public/other", false),
        ];
        let found = [
            PathBuf::from("/home/u/Git/Public/app"),
            PathBuf::from("/home/u/Git/Public/other"),
        ];
        let plan = plan_scan(&existing, &found, Path::new("Git"), false);
        assert_eq!(plan.relocate, vec![(Uuid::from_u128(1), 0)]);
        assert!(plan.add.is_empty() && plan.remove.is_empty());
        assert_eq!(plan.already, 1);
    }

    #[test]
    fn an_ambiguous_name_is_added_not_moved() {
        let existing = [record(
            1,
            "Git/Private/app",
            "/home/u/Git/Private/app",
            true,
        )];
        let found = [
            PathBuf::from("/home/u/Git/Public/app"),
            PathBuf::from("/home/u/Git/Work/app"),
        ];
        let plan = plan_scan(&existing, &found, Path::new("Git"), false);
        assert_eq!(plan.add, vec![0, 1]);
        assert!(plan.relocate.is_empty());
        assert_eq!(plan.remove, vec![Uuid::from_u128(1)]);
    }

    #[test]
    fn missing_records_go_only_inside_the_scanned_folder_and_after_a_full_scan() {
        let existing = [
            record(1, "Git/gone", "/home/u/Git/gone", true),
            record(2, "Other/gone-too", "/home/u/Other/gone-too", true),
            record(3, "Git/offline", "/home/u/Git/offline", false),
        ];
        let plan = plan_scan(&existing, &[], Path::new("Git"), false);
        assert_eq!(plan.remove, vec![Uuid::from_u128(1)]);
        let plan = plan_scan(&existing, &[], Path::new("Git"), true);
        assert!(plan.remove.is_empty());
        let plan = plan_scan(&existing, &[], Path::new("."), false);
        assert_eq!(plan.remove, vec![Uuid::from_u128(1), Uuid::from_u128(2)]);
    }

    #[test]
    fn scan_summary_lists_only_what_happened() {
        assert_eq!(scan_summary(2, 0, 0, 0), "Added 2 Git projects.");
        assert_eq!(
            scan_summary(1, 1, 1, 30),
            "Added 1 Git project. Moved 1 to its new folder. Removed 1 that no longer exists. Skipped 30."
        );
        assert_eq!(
            scan_summary(0, 2, 0, 5),
            "Moved 2 to their new folders. Skipped 5."
        );
    }
}
