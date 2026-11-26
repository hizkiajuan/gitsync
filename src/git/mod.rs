mod command;
mod discover;
mod inspect;
mod pull_requests;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub use command::CancellationToken;
pub use discover::{
    Discovery, discover_local_repositories, discover_remote_repositories, normalize_remote_root,
    normalize_ssh_destination,
};
pub use pull_requests::find_github_cli;

use command::{CommandError, CommandOutput, CommandRunner};
use inspect::{Head, LocalInspection, inspect_local};
use pull_requests::{PullRequestLookup, look_up_pull_requests};

use crate::domain::{
    ErrorKind, Freshness, OfflineReason, RepositoryIssue, RepositorySnapshot, StaleReason,
    SynchronizationState,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DIAGNOSTIC_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FetchMode {
    Allowed,
    Offline(OfflineReason),
}

#[derive(Clone, Debug)]
pub struct RefreshOptions {
    pub git_executable: PathBuf,
    pub fetch_mode: FetchMode,
    pub timeout: Duration,
    /// The GitHub CLI used to count open pull requests. `None` turns the
    /// lookup off.
    pub github_cli: Option<PathBuf>,
}

impl Default for RefreshOptions {
    fn default() -> Self {
        Self {
            git_executable: PathBuf::from("/usr/bin/git"),
            fetch_mode: FetchMode::Offline(OfflineReason::ConsentNotGranted),
            timeout: DEFAULT_TIMEOUT,
            github_cli: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitError {
    pub kind: ErrorKind,
    pub diagnostic: String,
}

impl GitError {
    fn new(kind: ErrorKind, diagnostic: impl Into<String>) -> Self {
        Self {
            kind,
            diagnostic: sanitize_diagnostic(&diagnostic.into(), false),
        }
    }
}

pub fn git_version(
    executable: PathBuf,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<String, GitError> {
    let runner = CommandRunner::new(executable, timeout);
    let output = runner
        .run_git(None, [OsString::from("--version")], cancellation)
        .map_err(command_error)?;
    if !output.status.success() {
        return Err(failed_output(output, ErrorKind::GitUnavailable));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version.is_empty() {
        return Err(GitError::new(
            ErrorKind::GitUnavailable,
            "Git returned an empty version",
        ));
    }
    Ok(version.chars().take(120).collect())
}

/// Checks that `destination` accepts a batch-mode SSH login and reports the
/// remote Git version.
pub fn remote_git_version(
    destination: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<String, GitError> {
    let runner = CommandRunner::remote(destination.to_owned(), timeout);
    let output = runner
        .run_git(None, [OsString::from("--version")], cancellation)
        .map_err(command_error)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if stderr.contains("not found") {
            return Err(GitError::new(
                ErrorKind::GitUnavailable,
                "SSH works, but Git is not on the remote PATH for non-interactive logins",
            ));
        }
        return Err(failed_output(output, ErrorKind::NetworkUnavailable));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(version.chars().take(120).collect())
}

pub fn refresh_repository(
    selected_path: &Path,
    options: &RefreshOptions,
    previous: Option<&RepositorySnapshot>,
    cancellation: &CancellationToken,
) -> RepositorySnapshot {
    let runner = CommandRunner::new(options.git_executable.clone(), options.timeout);
    refresh_with_runner(selected_path, options, previous, cancellation, runner)
}

pub fn refresh_remote_repository(
    destination: &str,
    selected_path: &Path,
    options: &RefreshOptions,
    previous: Option<&RepositorySnapshot>,
    cancellation: &CancellationToken,
) -> RepositorySnapshot {
    let runner = CommandRunner::remote(destination.to_owned(), options.timeout);
    refresh_with_runner(selected_path, options, previous, cancellation, runner)
}

fn refresh_with_runner(
    selected_path: &Path,
    options: &RefreshOptions,
    previous: Option<&RepositorySnapshot>,
    cancellation: &CancellationToken,
    runner: CommandRunner,
) -> RepositorySnapshot {
    let mut snapshot = refresh_sync_state(selected_path, options, previous, cancellation, &runner);
    // Pull requests belong to the repository, not to one branch, so the last
    // counts stay until a new lookup succeeds.
    snapshot.pull_requests = previous.and_then(|previous| previous.pull_requests);
    if let (Some(github_cli), FetchMode::Allowed, Some(root)) =
        (&options.github_cli, options.fetch_mode, &snapshot.root)
    {
        let remote = snapshot
            .upstream
            .as_ref()
            .map_or("origin", |upstream| upstream.remote.as_str());
        if let Some(url) = remote_url(&runner, root, remote, cancellation) {
            match look_up_pull_requests(github_cli, &url, options.timeout, cancellation) {
                PullRequestLookup::Found(summary) => snapshot.pull_requests = Some(summary),
                PullRequestLookup::NotGitHub => snapshot.pull_requests = None,
                PullRequestLookup::Failed => {}
            }
        }
    }
    snapshot
}

/// The fetch URL of `remote`, after `insteadOf` rewrites.
fn remote_url(
    runner: &CommandRunner,
    root: &Path,
    remote: &str,
    cancellation: &CancellationToken,
) -> Option<String> {
    if remote == "." {
        return None;
    }
    let output = runner
        .run_git(
            Some(root),
            [
                OsString::from("remote"),
                OsString::from("get-url"),
                OsString::from(remote),
            ],
            cancellation,
        )
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    let url = url.lines().next()?.trim();
    (!url.is_empty()).then(|| url.to_owned())
}

fn refresh_sync_state(
    selected_path: &Path,
    options: &RefreshOptions,
    previous: Option<&RepositorySnapshot>,
    cancellation: &CancellationToken,
    runner: &CommandRunner,
) -> RepositorySnapshot {
    let local = match inspect_local(selected_path, runner, cancellation) {
        Ok(local) => local,
        Err(error) => return unavailable_snapshot(error),
    };
    let scanned_at = SystemTime::now();
    let mut snapshot = snapshot_from_local(&local, scanned_at);

    let Head::Branch(_) = &local.head else {
        snapshot.issue = Some(RepositoryIssue::DetachedHead);
        return snapshot;
    };
    let Some(upstream) = &local.upstream else {
        snapshot.issue = Some(RepositoryIssue::NoUpstream);
        return snapshot;
    };

    match options.fetch_mode {
        FetchMode::Offline(reason) => {
            snapshot.freshness = Freshness::Stale {
                last_successful_fetch_at: previous
                    .and_then(|snapshot| snapshot.freshness.last_successful_fetch_at()),
                reason: StaleReason::Offline(reason),
            };
            match calculate_synchronization(&local.root, runner, cancellation) {
                Ok(synchronization) => snapshot.synchronization = Some(synchronization),
                Err(error) => {
                    snapshot.issue = Some(RepositoryIssue::Unavailable {
                        kind: error.kind,
                        diagnostic: error.diagnostic,
                    });
                }
            }
            return snapshot;
        }
        FetchMode::Allowed if upstream.remote == "." => {
            match calculate_synchronization(&local.root, runner, cancellation) {
                Ok(synchronization) => {
                    snapshot.synchronization = Some(synchronization);
                    snapshot.freshness = Freshness::LocalUpstream { scanned_at };
                }
                Err(error) => {
                    snapshot.issue = Some(RepositoryIssue::Unavailable {
                        kind: error.kind,
                        diagnostic: error.diagnostic,
                    });
                }
            }
            return snapshot;
        }
        FetchMode::Allowed => {}
    }

    if !upstream.reference.starts_with("refs/remotes/")
        || !upstream.remote_reference.starts_with("refs/")
    {
        let error = GitError::new(
            ErrorKind::UnsupportedUpstream,
            "refusing to fetch into a reference that is not remote-tracking",
        );
        return fetch_failed_snapshot(snapshot, previous, error);
    }

    let refspec = format!("+{}:{}", upstream.remote_reference, upstream.reference);
    let fetch_output = runner.run_git(
        Some(&local.root),
        [
            OsString::from("fetch"),
            OsString::from("--no-tags"),
            OsString::from("--no-write-fetch-head"),
            OsString::from("--"),
            OsString::from(&upstream.remote),
            OsString::from(refspec),
        ],
        cancellation,
    );
    match fetch_output {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let error = failed_output(output, ErrorKind::UnknownGitFailure);
            return fetch_failed_snapshot(snapshot, previous, error);
        }
        Err(error) => {
            return fetch_failed_snapshot(snapshot, previous, command_error(error));
        }
    }

    let refreshed_local = match inspect_local(selected_path, runner, cancellation) {
        Ok(refreshed_local) => refreshed_local,
        Err(error) => return fetch_failed_snapshot(snapshot, previous, error),
    };
    let refreshed_at = SystemTime::now();
    let refreshed_snapshot = snapshot_from_local(&refreshed_local, refreshed_at);
    if !snapshot.has_same_comparison(&refreshed_snapshot) {
        return fetch_failed_snapshot(
            refreshed_snapshot,
            previous,
            GitError::new(
                ErrorKind::RepositoryChanged,
                "branch or upstream changed while the repository was being refreshed",
            ),
        );
    }
    snapshot = refreshed_snapshot;

    match calculate_synchronization(&refreshed_local.root, runner, cancellation) {
        Ok(synchronization) => {
            snapshot.synchronization = Some(synchronization);
            snapshot.freshness = Freshness::Fresh {
                fetched_at: SystemTime::now(),
            };
        }
        Err(error) => {
            snapshot.issue = Some(RepositoryIssue::Unavailable {
                kind: error.kind,
                diagnostic: error.diagnostic,
            });
        }
    }

    snapshot
}

fn snapshot_from_local(local: &LocalInspection, scanned_at: SystemTime) -> RepositorySnapshot {
    RepositorySnapshot {
        root: Some(local.root.clone()),
        branch: match &local.head {
            Head::Branch(branch) => Some(branch.clone()),
            Head::Detached => None,
        },
        upstream: local.upstream.clone(),
        synchronization: None,
        working_tree: local.working_tree,
        local_scanned_at: Some(scanned_at),
        freshness: Freshness::Unchecked,
        issue: None,
        pull_requests: None,
    }
}

fn unavailable_snapshot(error: GitError) -> RepositorySnapshot {
    RepositorySnapshot {
        issue: Some(RepositoryIssue::Unavailable {
            kind: error.kind,
            diagnostic: error.diagnostic,
        }),
        ..RepositorySnapshot::default()
    }
}

fn fetch_failed_snapshot(
    mut current: RepositorySnapshot,
    previous: Option<&RepositorySnapshot>,
    error: GitError,
) -> RepositorySnapshot {
    let last_successful_fetch_at =
        previous.and_then(|snapshot| snapshot.freshness.last_successful_fetch_at());
    if let Some(previous) = previous
        && current.has_same_comparison(previous)
    {
        current.synchronization = previous.synchronization;
    }
    current.freshness = Freshness::Stale {
        last_successful_fetch_at,
        reason: StaleReason::FetchFailed(error.kind),
    };
    current.issue = Some(RepositoryIssue::FetchFailed {
        kind: error.kind,
        diagnostic: error.diagnostic,
    });
    current
}

fn calculate_synchronization(
    root: &Path,
    runner: &CommandRunner,
    cancellation: &CancellationToken,
) -> Result<SynchronizationState, GitError> {
    let output = runner
        .run_git(
            Some(root),
            [
                OsString::from("rev-list"),
                OsString::from("--left-right"),
                OsString::from("--count"),
                OsString::from("HEAD...@{upstream}"),
            ],
            cancellation,
        )
        .map_err(command_error)?;
    if !output.status.success() {
        return Err(failed_output(output, ErrorKind::RemoteMissing));
    }

    let counts = std::str::from_utf8(&output.stdout).map_err(|_| {
        GitError::new(
            ErrorKind::UnknownGitFailure,
            "Git returned non-UTF-8 ahead/behind counts",
        )
    })?;
    let mut counts = counts.split_ascii_whitespace();
    let ahead = parse_count(counts.next(), "ahead")?;
    let behind = parse_count(counts.next(), "behind")?;
    if counts.next().is_some() {
        return Err(GitError::new(
            ErrorKind::UnknownGitFailure,
            "Git returned extra ahead/behind fields",
        ));
    }

    Ok(SynchronizationState::from_counts(ahead, behind))
}

fn parse_count(count: Option<&str>, name: &str) -> Result<u32, GitError> {
    count
        .ok_or_else(|| {
            GitError::new(
                ErrorKind::UnknownGitFailure,
                format!("Git omitted the {name} count"),
            )
        })?
        .parse()
        .map_err(|_| {
            GitError::new(
                ErrorKind::UnknownGitFailure,
                format!("Git returned an invalid {name} count"),
            )
        })
}

fn command_error(error: CommandError) -> GitError {
    match error {
        CommandError::Spawn(error) if error.kind() == std::io::ErrorKind::NotFound => {
            GitError::new(
                ErrorKind::GitUnavailable,
                "the Git executable was not found",
            )
        }
        CommandError::Spawn(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            GitError::new(
                ErrorKind::PermissionDenied,
                "the Git executable is not executable",
            )
        }
        CommandError::Spawn(error) | CommandError::Io(error) => GitError::new(
            ErrorKind::UnknownGitFailure,
            format!("failed to run Git: {error}"),
        ),
        CommandError::Timeout => GitError::new(ErrorKind::Timeout, "the Git command timed out"),
        CommandError::Cancelled => {
            GitError::new(ErrorKind::Cancelled, "the Git command was canceled")
        }
        CommandError::OutputTooLarge => GitError::new(
            ErrorKind::OutputTooLarge,
            "Git produced more output than GitSync accepts",
        ),
    }
}

fn failed_output(output: CommandOutput, default_kind: ErrorKind) -> GitError {
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    let lower = diagnostic.to_ascii_lowercase();
    let kind = if lower.contains("authentication failed")
        || lower.contains("permission denied (publickey")
        || lower.contains("could not read username")
        || lower.contains("terminal prompts disabled")
    {
        ErrorKind::AuthenticationRequired
    } else if lower.contains("could not resolve host")
        || lower.contains("network is unreachable")
        || lower.contains("connection timed out")
        || lower.contains("couldn't connect to server")
    {
        ErrorKind::NetworkUnavailable
    } else if lower.contains("couldn't find remote ref") {
        ErrorKind::UpstreamGone
    } else if lower.contains("no such remote")
        || lower.contains("does not appear to be a git repository")
    {
        ErrorKind::RemoteMissing
    } else if lower.contains("permission denied") {
        ErrorKind::PermissionDenied
    } else {
        default_kind
    };

    GitError {
        kind,
        diagnostic: sanitize_diagnostic(&diagnostic, output.stderr_truncated),
    }
}

fn sanitize_diagnostic(input: &str, was_truncated: bool) -> String {
    let mut redacted = String::with_capacity(input.len().min(MAX_DIAGNOSTIC_BYTES));
    for segment in input.split_inclusive(char::is_whitespace) {
        let visible = segment.trim_end_matches(char::is_whitespace);
        let whitespace = &segment[visible.len()..];
        // Only the authority part (before the first `/` after the scheme) can
        // hold credentials; an `@` later in the path is not userinfo.
        if let Some(scheme_end) = visible.find("://")
            && let Some(at) = {
                let authority = &visible[scheme_end + 3..];
                let end = authority.find('/').unwrap_or(authority.len());
                authority[..end].find('@')
            }
        {
            redacted.push_str(&visible[..scheme_end + 3]);
            redacted.push_str("***@");
            redacted.push_str(&visible[scheme_end + 3 + at + 1..]);
        } else {
            redacted.push_str(visible);
        }
        redacted.push_str(whitespace);
    }

    let mut truncated = was_truncated;
    if redacted.len() > MAX_DIAGNOSTIC_BYTES {
        let mut end = MAX_DIAGNOSTIC_BYTES;
        while !redacted.is_char_boundary(end) {
            end -= 1;
        }
        redacted.truncate(end);
        truncated = true;
    }
    if truncated {
        redacted.push_str("\n[diagnostic truncated]");
    }
    redacted.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{CancellationToken, git_version, sanitize_diagnostic};

    #[test]
    fn redacts_url_userinfo() {
        assert_eq!(
            sanitize_diagnostic(
                "fatal: unable to access 'https://user:secret@example.com/repo': denied",
                false,
            ),
            "fatal: unable to access 'https://***@example.com/repo': denied"
        );
        assert_eq!(
            sanitize_diagnostic("see https://example.com/pkg@v1 for details", false),
            "see https://example.com/pkg@v1 for details"
        );
    }

    #[test]
    fn verifies_the_configured_git_executable() {
        let version = git_version(
            PathBuf::from("/usr/bin/git"),
            Duration::from_secs(5),
            &CancellationToken::default(),
        )
        .expect("system Git should be available");

        assert!(version.starts_with("git version "));
        assert!(
            git_version(
                PathBuf::from("/definitely/missing/git"),
                Duration::from_secs(1),
                &CancellationToken::default(),
            )
            .is_err()
        );
    }
}
