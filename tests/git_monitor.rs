use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use gitsync::domain::{ErrorKind, Freshness, RepositoryIssue, StaleReason, SynchronizationState};
use gitsync::git::{CancellationToken, FetchMode, RefreshOptions, refresh_repository};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    remote: PathBuf,
    monitored: PathBuf,
    second_clone: PathBuf,
}

impl Fixture {
    fn new(monitored_name: &str) -> Self {
        let temp = TempDir::new().expect("temporary directory should be created");
        let remote = temp.path().join("remote.git");
        let seed = temp.path().join("seed");
        let monitored = temp.path().join(monitored_name);
        let second_clone = temp.path().join("second");

        git_with_paths(None, ["init", "--bare"], [&remote]);
        git(Some(&seed), ["init", "-b", "main"]);
        configure_identity(&seed);
        fs::write(seed.join("tracked.txt"), "initial\n").expect("seed file should be written");
        git(Some(&seed), ["add", "tracked.txt"]);
        git(Some(&seed), ["commit", "-m", "initial"]);
        git_with_paths(Some(&seed), ["remote", "add", "origin"], [&remote]);
        git(Some(&seed), ["push", "-u", "origin", "main"]);
        git(Some(&remote), ["symbolic-ref", "HEAD", "refs/heads/main"]);

        git_with_paths(None, ["clone"], [&remote, &monitored]);
        git_with_paths(None, ["clone"], [&remote, &second_clone]);
        configure_identity(&monitored);
        configure_identity(&second_clone);

        Self {
            _temp: temp,
            remote,
            monitored,
            second_clone,
        }
    }

    fn refresh(
        &self,
        previous: Option<&gitsync::domain::RepositorySnapshot>,
    ) -> gitsync::domain::RepositorySnapshot {
        refresh_repository(
            &self.monitored,
            &allowed_options(),
            previous,
            &CancellationToken::default(),
        )
    }
}

#[test]
fn reports_up_to_date_dirty_ahead_and_diverged() {
    let fixture = Fixture::new("repo ünicode ' -leading");

    let initial = fixture.refresh(None);
    assert_eq!(
        initial.synchronization,
        Some(SynchronizationState::UpToDate)
    );
    assert!(matches!(initial.freshness, Freshness::Fresh { .. }));
    assert_eq!(initial.issue, None);

    fs::write(fixture.monitored.join("tracked.txt"), "dirty\n")
        .expect("working-tree change should be written");
    fs::write(fixture.monitored.join("untracked.txt"), "untracked\n")
        .expect("untracked file should be written");
    let dirty = fixture.refresh(Some(&initial));
    assert_eq!(dirty.synchronization, Some(SynchronizationState::UpToDate));
    assert_eq!(dirty.working_tree.modified, 1);
    assert_eq!(dirty.working_tree.untracked, 1);

    git(Some(&fixture.monitored), ["add", "tracked.txt"]);
    git(Some(&fixture.monitored), ["commit", "-m", "local-only"]);
    let ahead = fixture.refresh(Some(&dirty));
    assert_eq!(
        ahead.synchronization,
        Some(SynchronizationState::Ahead { count: 1 })
    );

    commit_file(
        &fixture.second_clone,
        "remote.txt",
        "remote\n",
        "remote-only",
    );
    git(Some(&fixture.second_clone), ["push", "origin", "main"]);
    let diverged = fixture.refresh(Some(&ahead));
    assert_eq!(
        diverged.synchronization,
        Some(SynchronizationState::Diverged {
            ahead: 1,
            behind: 1,
        })
    );
}

#[test]
fn reports_behind_after_fetching_an_upstream_commit() {
    let fixture = Fixture::new("monitored");
    let initial = fixture.refresh(None);

    commit_file(
        &fixture.second_clone,
        "remote.txt",
        "remote\n",
        "remote-only",
    );
    git(Some(&fixture.second_clone), ["push", "origin", "main"]);
    let behind = fixture.refresh(Some(&initial));

    assert_eq!(
        behind.synchronization,
        Some(SynchronizationState::Behind { count: 1 })
    );
    assert!(matches!(behind.freshness, Freshness::Fresh { .. }));
}

#[test]
fn reports_setup_states_without_guessing() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let repository = temp.path().join("local");
    git(Some(&repository), ["init", "-b", "main"]);
    configure_identity(&repository);
    commit_file(&repository, "tracked.txt", "initial\n", "initial");

    let no_upstream = refresh_repository(
        &repository,
        &allowed_options(),
        None,
        &CancellationToken::default(),
    );
    assert_eq!(no_upstream.issue, Some(RepositoryIssue::NoUpstream));
    assert_eq!(no_upstream.synchronization, None);

    git(Some(&repository), ["checkout", "--detach"]);
    let detached = refresh_repository(
        &repository,
        &allowed_options(),
        Some(&no_upstream),
        &CancellationToken::default(),
    );
    assert_eq!(detached.issue, Some(RepositoryIssue::DetachedHead));
    assert_eq!(detached.branch, None);
    assert_eq!(detached.synchronization, None);
}

#[test]
fn distinguishes_missing_non_repository_and_bare_paths() {
    let temp = TempDir::new().expect("temporary directory should be created");
    let missing = temp.path().join("missing");
    let missing_snapshot = refresh_repository(
        &missing,
        &allowed_options(),
        None,
        &CancellationToken::default(),
    );
    assert!(matches!(
        missing_snapshot.issue,
        Some(RepositoryIssue::Unavailable {
            kind: ErrorKind::PathMissing,
            ..
        })
    ));

    let directory = temp.path().join("directory");
    fs::create_dir(&directory).expect("plain directory should be created");
    let non_repository = refresh_repository(
        &directory,
        &allowed_options(),
        None,
        &CancellationToken::default(),
    );
    assert!(matches!(
        non_repository.issue,
        Some(RepositoryIssue::Unavailable {
            kind: ErrorKind::NotRepository,
            ..
        })
    ));

    let bare = temp.path().join("bare.git");
    git_with_paths(None, ["init", "--bare"], [&bare]);
    let bare_snapshot = refresh_repository(
        &bare,
        &allowed_options(),
        None,
        &CancellationToken::default(),
    );
    assert!(matches!(
        bare_snapshot.issue,
        Some(RepositoryIssue::Unavailable {
            kind: ErrorKind::BareRepository,
            ..
        })
    ));
}

#[test]
fn failed_fetch_preserves_matching_last_known_state_and_recovers() {
    let fixture = Fixture::new("monitored");
    let fresh = fixture.refresh(None);
    let unavailable_remote = fixture.remote.with_extension("unavailable");
    fs::rename(&fixture.remote, &unavailable_remote).expect("remote should be moved aside");

    let stale = fixture.refresh(Some(&fresh));
    assert_eq!(stale.synchronization, Some(SynchronizationState::UpToDate));
    assert!(matches!(
        stale.freshness,
        Freshness::Stale {
            last_successful_fetch_at: Some(_),
            reason: StaleReason::FetchFailed(ErrorKind::RemoteMissing),
        }
    ));
    assert!(matches!(
        stale.issue,
        Some(RepositoryIssue::FetchFailed {
            kind: ErrorKind::RemoteMissing,
            ..
        })
    ));

    fs::rename(&unavailable_remote, &fixture.remote).expect("remote should be restored");
    let recovered = fixture.refresh(Some(&stale));
    assert_eq!(
        recovered.synchronization,
        Some(SynchronizationState::UpToDate)
    );
    assert!(matches!(recovered.freshness, Freshness::Fresh { .. }));
    assert_eq!(recovered.issue, None);
}

#[test]
fn offline_mode_is_explicitly_stale() {
    let fixture = Fixture::new("monitored");
    let options = RefreshOptions {
        fetch_mode: FetchMode::Offline(gitsync::domain::OfflineReason::ConsentNotGranted),
        ..allowed_options()
    };

    let snapshot = refresh_repository(
        &fixture.monitored,
        &options,
        None,
        &CancellationToken::default(),
    );

    assert_eq!(
        snapshot.synchronization,
        Some(SynchronizationState::UpToDate)
    );
    assert!(matches!(
        snapshot.freshness,
        Freshness::Stale {
            last_successful_fetch_at: None,
            reason: StaleReason::Offline(gitsync::domain::OfflineReason::ConsentNotGranted),
        }
    ));
}

fn allowed_options() -> RefreshOptions {
    RefreshOptions {
        fetch_mode: FetchMode::Allowed,
        ..RefreshOptions::default()
    }
}

fn configure_identity(repository: &Path) {
    git(Some(repository), ["config", "user.name", "GitSync Test"]);
    git(
        Some(repository),
        ["config", "user.email", "gitsync@example.invalid"],
    );
}

fn commit_file(repository: &Path, name: &str, contents: &str, message: &str) {
    fs::write(repository.join(name), contents).expect("fixture file should be written");
    git(Some(repository), ["add", name]);
    git(Some(repository), ["commit", "-m", message]);
}

fn git<const N: usize>(working_tree: Option<&Path>, arguments: [&str; N]) {
    let output = run_git(
        working_tree,
        arguments.into_iter().map(OsStr::new),
        std::iter::empty::<&Path>(),
    );
    assert_success(output);
}

fn git_with_paths<'a, const N: usize, const P: usize>(
    working_tree: Option<&Path>,
    arguments: [&'a str; N],
    paths: [&'a Path; P],
) {
    let output = run_git(working_tree, arguments.into_iter().map(OsStr::new), paths);
    assert_success(output);
}

fn run_git<'a>(
    working_tree: Option<&Path>,
    arguments: impl IntoIterator<Item = &'a OsStr>,
    paths: impl IntoIterator<Item = &'a Path>,
) -> Output {
    let mut command = Command::new("/usr/bin/git");
    if let Some(working_tree) = working_tree {
        fs::create_dir_all(working_tree).expect("working tree directory should exist");
        command.arg("-C").arg(working_tree);
    }
    command
        .args(arguments)
        .args(paths)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("fixture Git command should start")
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "fixture Git command failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
