//! Open pull request counts from the GitHub CLI (`gh`).
//!
//! GitSync stores no GitHub token. It runs `gh` on this Mac, which uses the
//! user's own `gh auth login`. For SSH repositories, only the remote URL is
//! read on the remote computer; `gh` always runs locally.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use super::command::{CancellationToken, CommandRunner};
use crate::domain::PullRequestSummary;

/// `gh` is rarely on the minimal `PATH` that apps opened from Finder get, so
/// the usual install locations are checked first.
const GITHUB_CLI_CANDIDATES: [&str; 3] =
    ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"];
/// Upper bound on the pull requests read per repository.
const PULL_REQUEST_LIMIT: &str = "100";

/// The path of the GitHub CLI, if it is installed.
pub fn find_github_cli() -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("gh"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    GITHUB_CLI_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .chain(from_path)
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Result of one pull request lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PullRequestLookup {
    /// The remote is not on GitHub.
    NotGitHub,
    /// The lookup did not finish, for example because `gh` is not signed in
    /// or the network is down. The last known counts stay.
    Failed,
    Found(PullRequestSummary),
}

/// Counts the open pull requests that the `gh` user authored in the GitHub
/// repository behind `remote_url`.
pub(crate) fn look_up_pull_requests(
    github_cli: &Path,
    remote_url: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> PullRequestLookup {
    let Some(repository) = github_repository(remote_url) else {
        return PullRequestLookup::NotGitHub;
    };
    let runner = CommandRunner::new(github_cli.to_path_buf(), timeout);
    let output = runner.run_local(
        [
            "pr",
            "list",
            "--repo",
            &repository,
            "--author",
            "@me",
            "--state",
            "open",
            "--limit",
            PULL_REQUEST_LIMIT,
            "--json",
            "isDraft,reviewDecision",
        ]
        .map(OsString::from),
        cancellation,
    );
    match output {
        Ok(output) if output.status.success() => parse_pull_requests(&output.stdout)
            .map_or(PullRequestLookup::Failed, |summary| {
                PullRequestLookup::Found(summary)
            }),
        _ => PullRequestLookup::Failed,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestEntry {
    is_draft: bool,
    #[serde(default)]
    review_decision: Option<String>,
}

fn parse_pull_requests(json: &[u8]) -> Option<PullRequestSummary> {
    let entries: Vec<PullRequestEntry> = serde_json::from_slice(json).ok()?;
    let mut summary = PullRequestSummary::default();
    for entry in entries {
        let count = if entry.is_draft {
            &mut summary.draft
        } else {
            match entry.review_decision.as_deref() {
                Some("APPROVED") => &mut summary.approved,
                Some("CHANGES_REQUESTED") => &mut summary.changes_requested,
                // `REVIEW_REQUIRED`, or empty when the branch needs no review.
                _ => &mut summary.awaiting_review,
            }
        };
        *count = count.saturating_add(1);
    }
    Some(summary)
}

/// `owner/repository` for a remote URL on github.com, in any of the forms
/// Git accepts: HTTPS, `ssh://`, or scp-like `git@github.com:owner/repo`.
pub(crate) fn github_repository(remote_url: &str) -> Option<String> {
    let url = remote_url.trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let host = host.split_once(':').map_or(host, |(host, _)| host);
        (host, path)
    } else {
        // scp-like syntax: `[user@]host:path`, with no slash before the colon.
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (host, path)
    };
    if !matches!(
        host.to_ascii_lowercase().as_str(),
        "github.com" | "www.github.com" | "ssh.github.com"
    ) {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let valid = |part: &str| {
        !part.is_empty()
            && !part.starts_with(['-', '.'])
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (valid(owner) && valid(name)).then(|| format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::{github_repository, parse_pull_requests};
    use crate::domain::PullRequestSummary;

    #[test]
    fn reads_github_repository_from_every_remote_form() {
        for url in [
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo",
            "https://user:token@github.com/owner/repo.git/",
            "ssh://git@github.com/owner/repo.git",
            "ssh://git@ssh.github.com:443/owner/repo.git",
            "git@github.com:owner/repo.git",
            "github.com:owner/repo",
        ] {
            assert_eq!(
                github_repository(url).as_deref(),
                Some("owner/repo"),
                "{url}"
            );
        }
    }

    #[test]
    fn ignores_other_hosts_and_unsafe_paths() {
        for url in [
            "https://gitlab.com/owner/repo.git",
            "git@example.com:owner/repo.git",
            "/srv/git/repo.git",
            "../repo",
            "https://github.com/owner",
            "https://github.com/owner/repo/extra",
            "git@github.com:-owner/repo.git",
            "file:///github.com/owner/repo",
        ] {
            assert_eq!(github_repository(url), None, "{url}");
        }
    }

    #[test]
    fn counts_pull_requests_by_review_state() {
        let json = br#"[
            {"isDraft": true, "reviewDecision": ""},
            {"isDraft": false, "reviewDecision": "REVIEW_REQUIRED"},
            {"isDraft": false, "reviewDecision": ""},
            {"isDraft": false, "reviewDecision": "APPROVED"},
            {"isDraft": false, "reviewDecision": "CHANGES_REQUESTED"}
        ]"#;
        assert_eq!(
            parse_pull_requests(json),
            Some(PullRequestSummary {
                awaiting_review: 2,
                changes_requested: 1,
                approved: 1,
                draft: 1,
            })
        );
        assert_eq!(
            parse_pull_requests(b"[]"),
            Some(PullRequestSummary::default())
        );
        assert_eq!(parse_pull_requests(b"not json"), None);
    }
}
