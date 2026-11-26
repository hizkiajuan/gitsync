use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Upstream {
    pub reference: String,
    pub remote: String,
    pub remote_reference: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkingTreeState {
    pub staged: u32,
    pub modified: u32,
    pub conflicted: u32,
    pub untracked: u32,
}

impl WorkingTreeState {
    pub fn is_dirty(self) -> bool {
        self.staged > 0 || self.modified > 0 || self.conflicted > 0 || self.untracked > 0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SynchronizationState {
    UpToDate,
    Ahead { count: u32 },
    Behind { count: u32 },
    Diverged { ahead: u32, behind: u32 },
}

impl SynchronizationState {
    pub fn from_counts(ahead: u32, behind: u32) -> Self {
        match (ahead, behind) {
            (0, 0) => Self::UpToDate,
            (ahead, 0) => Self::Ahead { count: ahead },
            (0, behind) => Self::Behind { count: behind },
            (ahead, behind) => Self::Diverged { ahead, behind },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NotificationFingerprint {
    Ahead { count: u32 },
    Behind { count: u32 },
    Diverged { ahead: u32, behind: u32 },
}

impl NotificationFingerprint {
    pub fn from_snapshot(snapshot: &RepositorySnapshot) -> Option<Self> {
        if snapshot.issue.is_some() || !matches!(snapshot.freshness, Freshness::Fresh { .. }) {
            return None;
        }
        match snapshot.synchronization {
            Some(SynchronizationState::Ahead { count }) => Some(Self::Ahead { count }),
            Some(SynchronizationState::Behind { count }) => Some(Self::Behind { count }),
            Some(SynchronizationState::Diverged { ahead, behind }) => {
                Some(Self::Diverged { ahead, behind })
            }
            Some(SynchronizationState::UpToDate) | None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationTransition {
    Ignore,
    Record(Option<NotificationFingerprint>),
    Notify(NotificationFingerprint),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OfflineReason {
    ConsentNotGranted,
    DisabledByUser,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ErrorKind {
    GitUnavailable,
    PathMissing,
    PermissionDenied,
    NotRepository,
    BareRepository,
    AuthenticationRequired,
    NetworkUnavailable,
    Timeout,
    RemoteMissing,
    /// The upstream branch no longer exists on the remote, usually after a
    /// merged pull request deleted it.
    UpstreamGone,
    UnsupportedUpstream,
    RepositoryChanged,
    OutputTooLarge,
    Cancelled,
    UnknownGitFailure,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum StaleReason {
    Offline(OfflineReason),
    FetchFailed(ErrorKind),
    CachedAtStartup,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Freshness {
    Unchecked,
    Fresh {
        fetched_at: SystemTime,
    },
    LocalUpstream {
        scanned_at: SystemTime,
    },
    Stale {
        last_successful_fetch_at: Option<SystemTime>,
        reason: StaleReason,
    },
}

impl Freshness {
    pub fn last_successful_fetch_at(&self) -> Option<SystemTime> {
        match self {
            Self::Fresh { fetched_at } => Some(*fetched_at),
            Self::Stale {
                last_successful_fetch_at,
                ..
            } => *last_successful_fetch_at,
            Self::Unchecked | Self::LocalUpstream { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RepositoryIssue {
    NoUpstream,
    DetachedHead,
    Unavailable { kind: ErrorKind, diagnostic: String },
    FetchFailed { kind: ErrorKind, diagnostic: String },
}

/// Open pull requests that the signed-in GitHub user authored in the
/// repository, counted by review state.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PullRequestSummary {
    /// Ready for review, with no approval or change request yet.
    pub awaiting_review: u32,
    pub changes_requested: u32,
    pub approved: u32,
    pub draft: u32,
}

impl PullRequestSummary {
    pub fn total(self) -> u32 {
        self.awaiting_review
            .saturating_add(self.changes_requested)
            .saturating_add(self.approved)
            .saturating_add(self.draft)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RepositorySnapshot {
    pub root: Option<PathBuf>,
    pub branch: Option<String>,
    pub upstream: Option<Upstream>,
    pub synchronization: Option<SynchronizationState>,
    pub working_tree: WorkingTreeState,
    pub local_scanned_at: Option<SystemTime>,
    pub freshness: Freshness,
    pub issue: Option<RepositoryIssue>,
    /// `None` when the remote is not on GitHub, the GitHub CLI is missing,
    /// or no lookup has succeeded yet.
    #[serde(default)]
    pub pull_requests: Option<PullRequestSummary>,
}

impl Default for RepositorySnapshot {
    fn default() -> Self {
        Self {
            root: None,
            branch: None,
            upstream: None,
            synchronization: None,
            working_tree: WorkingTreeState::default(),
            local_scanned_at: None,
            freshness: Freshness::Unchecked,
            issue: None,
            pull_requests: None,
        }
    }
}

impl RepositorySnapshot {
    pub fn has_same_comparison(&self, other: &Self) -> bool {
        self.root == other.root && self.branch == other.branch && self.upstream == other.upstream
    }
}

pub fn notification_transition(
    previous: Option<NotificationFingerprint>,
    snapshot: &RepositorySnapshot,
) -> NotificationTransition {
    if snapshot.issue.is_some() || !matches!(snapshot.freshness, Freshness::Fresh { .. }) {
        return NotificationTransition::Ignore;
    }

    let current = NotificationFingerprint::from_snapshot(snapshot);
    let should_notify = match (previous, current) {
        (None, Some(_)) => true,
        (
            Some(NotificationFingerprint::Ahead { count: previous }),
            Some(NotificationFingerprint::Ahead { count: current }),
        )
        | (
            Some(NotificationFingerprint::Behind { count: previous }),
            Some(NotificationFingerprint::Behind { count: current }),
        ) => current > previous,
        (
            Some(NotificationFingerprint::Diverged {
                ahead: previous_ahead,
                behind: previous_behind,
            }),
            Some(NotificationFingerprint::Diverged { ahead, behind }),
        ) => ahead > previous_ahead || behind > previous_behind,
        (Some(previous), Some(current)) => previous != current,
        (_, None) => false,
    };

    if should_notify {
        NotificationTransition::Notify(current.expect("actionable transition has a fingerprint"))
    } else {
        NotificationTransition::Record(current)
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::{
        Freshness, NotificationFingerprint, NotificationTransition, RepositorySnapshot,
        StaleReason, SynchronizationState, notification_transition,
    };

    #[test]
    fn maps_all_ahead_behind_states() {
        assert_eq!(
            SynchronizationState::from_counts(0, 0),
            SynchronizationState::UpToDate
        );
        assert_eq!(
            SynchronizationState::from_counts(2, 0),
            SynchronizationState::Ahead { count: 2 }
        );
        assert_eq!(
            SynchronizationState::from_counts(0, 3),
            SynchronizationState::Behind { count: 3 }
        );
        assert_eq!(
            SynchronizationState::from_counts(2, 3),
            SynchronizationState::Diverged {
                ahead: 2,
                behind: 3,
            }
        );
    }

    #[test]
    fn notification_transitions_only_report_new_or_increased_actionable_states() {
        let mut snapshot = RepositorySnapshot {
            synchronization: Some(SynchronizationState::Ahead { count: 1 }),
            freshness: Freshness::Fresh {
                fetched_at: SystemTime::now(),
            },
            ..RepositorySnapshot::default()
        };

        assert_eq!(
            notification_transition(None, &snapshot),
            NotificationTransition::Notify(NotificationFingerprint::Ahead { count: 1 })
        );
        assert_eq!(
            notification_transition(Some(NotificationFingerprint::Ahead { count: 1 }), &snapshot),
            NotificationTransition::Record(Some(NotificationFingerprint::Ahead { count: 1 }))
        );

        snapshot.synchronization = Some(SynchronizationState::Ahead { count: 2 });
        assert_eq!(
            notification_transition(Some(NotificationFingerprint::Ahead { count: 1 }), &snapshot),
            NotificationTransition::Notify(NotificationFingerprint::Ahead { count: 2 })
        );

        snapshot.synchronization = Some(SynchronizationState::UpToDate);
        assert_eq!(
            notification_transition(Some(NotificationFingerprint::Ahead { count: 2 }), &snapshot),
            NotificationTransition::Record(None)
        );

        snapshot.freshness = Freshness::Stale {
            last_successful_fetch_at: None,
            reason: StaleReason::CachedAtStartup,
        };
        assert_eq!(
            notification_transition(None, &snapshot),
            NotificationTransition::Ignore
        );
    }
}
