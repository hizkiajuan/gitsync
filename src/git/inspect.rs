use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};

use crate::domain::{ErrorKind, Upstream, WorkingTreeState};

use super::command::{CancellationToken, CommandOutput, CommandRunner};
use super::{GitError, command_error, failed_output};

const UPSTREAM_FORMAT: &str = "%(upstream)%00%(upstream:remotename)%00%(upstream:remoteref)";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Head {
    Branch(String),
    Detached,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocalInspection {
    pub root: PathBuf,
    pub head: Head,
    pub upstream: Option<Upstream>,
    pub working_tree: WorkingTreeState,
}

pub(crate) fn inspect_local(
    selected_path: &Path,
    runner: &CommandRunner,
    cancellation: &CancellationToken,
) -> Result<LocalInspection, GitError> {
    if !runner.is_remote() {
        match selected_path.try_exists() {
            Ok(true) => {}
            Ok(false) => {
                return Err(GitError::new(
                    ErrorKind::PathMissing,
                    "the selected repository path no longer exists",
                ));
            }
            Err(error) => {
                return Err(GitError::new(
                    ErrorKind::PermissionDenied,
                    format!("cannot access the selected repository path: {error}"),
                ));
            }
        }
    }

    let bare_output = run(
        runner,
        Some(selected_path),
        ["rev-parse", "--is-bare-repository"],
        cancellation,
    )?;
    if !bare_output.status.success() {
        return Err(failed_output(bare_output, ErrorKind::NotRepository));
    }
    if text_line(&bare_output.stdout)? == "true" {
        return Err(GitError::new(
            ErrorKind::BareRepository,
            "bare repositories are not supported",
        ));
    }

    let root_output = run(
        runner,
        Some(selected_path),
        ["rev-parse", "--path-format=absolute", "--show-toplevel"],
        cancellation,
    )?;
    if !root_output.status.success() {
        return Err(failed_output(root_output, ErrorKind::NotRepository));
    }
    let root = path_from_line(&root_output.stdout)?;
    let root = if runner.is_remote() {
        root
    } else {
        std::fs::canonicalize(&root).map_err(|error| {
            GitError::new(
                ErrorKind::PermissionDenied,
                format!("cannot canonicalize the repository root: {error}"),
            )
        })?
    };

    let branch_output = run(
        runner,
        Some(&root),
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
        cancellation,
    )?;
    let head = if branch_output.status.success() {
        Head::Branch(text_line(&branch_output.stdout)?.to_owned())
    } else if branch_output.status.code() == Some(1) && branch_output.stdout.is_empty() {
        Head::Detached
    } else {
        return Err(failed_output(branch_output, ErrorKind::UnknownGitFailure));
    };

    let upstream = match &head {
        Head::Detached => None,
        Head::Branch(branch) => discover_upstream(&root, branch, runner, cancellation)?,
    };
    let working_tree = working_tree_state(&root, runner, cancellation)?;

    Ok(LocalInspection {
        root,
        head,
        upstream,
        working_tree,
    })
}

fn discover_upstream(
    root: &Path,
    branch: &str,
    runner: &CommandRunner,
    cancellation: &CancellationToken,
) -> Result<Option<Upstream>, GitError> {
    let branch_reference = format!("refs/heads/{branch}");
    let output = runner
        .run_git(
            Some(root),
            [
                OsString::from("for-each-ref"),
                OsString::from(format!("--format={UPSTREAM_FORMAT}")),
                OsString::from("--count=1"),
                OsString::from(branch_reference),
            ],
            cancellation,
        )
        .map_err(command_error)?;
    if !output.status.success() {
        return Err(failed_output(output, ErrorKind::UnknownGitFailure));
    }
    if output.stdout.is_empty() {
        return Ok(None);
    }

    let mut fields = output.stdout.splitn(3, |byte| *byte == 0);
    let reference = text_field(fields.next())?;
    let remote = text_field(fields.next())?;
    let remote_reference = text_field(fields.next())?;
    if reference.is_empty() {
        return Ok(None);
    }
    if remote.is_empty() || remote_reference.is_empty() {
        return Err(GitError::new(
            ErrorKind::UnsupportedUpstream,
            "the configured upstream does not identify a fetchable remote reference",
        ));
    }

    Ok(Some(Upstream {
        reference,
        remote,
        remote_reference,
    }))
}

fn working_tree_state(
    root: &Path,
    runner: &CommandRunner,
    cancellation: &CancellationToken,
) -> Result<WorkingTreeState, GitError> {
    let output = run(
        runner,
        Some(root),
        ["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
        cancellation,
    )?;
    if !output.status.success() {
        return Err(failed_output(output, ErrorKind::UnknownGitFailure));
    }

    parse_porcelain(&output.stdout)
}

fn parse_porcelain(output: &[u8]) -> Result<WorkingTreeState, GitError> {
    let records: Vec<&[u8]> = output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect();
    let mut state = WorkingTreeState::default();
    let mut index = 0;

    while index < records.len() {
        let record = records[index];
        if record.len() < 3 || record[2] != b' ' {
            return Err(GitError::new(
                ErrorKind::UnknownGitFailure,
                "Git returned malformed porcelain status",
            ));
        }

        let x = record[0];
        let y = record[1];
        let conflicted = matches!(
            (x, y),
            (b'D', b'D')
                | (b'A', b'U')
                | (b'U', b'D')
                | (b'U', b'A')
                | (b'D', b'U')
                | (b'A', b'A')
                | (b'U', b'U')
        );

        if conflicted {
            state.conflicted += 1;
        } else if (x, y) == (b'?', b'?') {
            state.untracked += 1;
        } else if (x, y) != (b'!', b'!') {
            if !matches!(x, b' ' | b'?' | b'!') {
                state.staged += 1;
            }
            if !matches!(y, b' ' | b'?' | b'!') {
                state.modified += 1;
            }
        }

        index += 1;
        if matches!(x, b'R' | b'C') {
            index += 1;
        }
    }

    Ok(state)
}

fn run<const N: usize>(
    runner: &CommandRunner,
    working_tree: Option<&Path>,
    arguments: [&str; N],
    cancellation: &CancellationToken,
) -> Result<CommandOutput, GitError> {
    runner
        .run_git(
            working_tree,
            arguments.into_iter().map(OsString::from),
            cancellation,
        )
        .map_err(command_error)
}

fn path_from_line(bytes: &[u8]) -> Result<PathBuf, GitError> {
    let bytes = trim_ascii_end(bytes).to_vec();
    if bytes.is_empty() {
        return Err(GitError::new(
            ErrorKind::UnknownGitFailure,
            "Git returned an empty repository root",
        ));
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn text_line(bytes: &[u8]) -> Result<&str, GitError> {
    std::str::from_utf8(trim_ascii_end(bytes)).map_err(|_| {
        GitError::new(
            ErrorKind::UnknownGitFailure,
            "Git returned non-UTF-8 metadata",
        )
    })
}

fn text_field(field: Option<&[u8]>) -> Result<String, GitError> {
    let field = field.ok_or_else(|| {
        GitError::new(
            ErrorKind::UnknownGitFailure,
            "Git returned incomplete upstream metadata",
        )
    })?;
    let field = std::str::from_utf8(trim_ascii_end(field)).map_err(|_| {
        GitError::new(
            ErrorKind::UnknownGitFailure,
            "Git returned non-UTF-8 upstream metadata",
        )
    })?;
    Ok(field.to_owned())
}

fn trim_ascii_end(mut bytes: &[u8]) -> &[u8] {
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::parse_porcelain;
    use crate::domain::WorkingTreeState;

    #[test]
    fn parses_orthogonal_working_tree_counts() {
        let state = parse_porcelain(
            b" M modified\0A  staged\0UU conflicted\0?? untracked\0R  renamed\0old-name\0",
        )
        .expect("porcelain should parse");

        assert_eq!(
            state,
            WorkingTreeState {
                staged: 2,
                modified: 1,
                conflicted: 1,
                untracked: 1,
            }
        );
    }
}
