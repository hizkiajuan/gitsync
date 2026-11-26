use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::command::{CancellationToken, CommandRunner, join_command_parts, shell_quote};
use super::{GitError, command_error, failed_output};
use crate::domain::ErrorKind;

const MAX_DISCOVERED_REPOSITORIES: usize = 1_000;

/// Folders that never hold a user's own working trees but can be very large.
/// Remote discovery prunes them so a scan of a home folder stays fast.
const REMOTE_PRUNED_DIRECTORIES: &[&str] = &[
    "node_modules",
    ".cache",
    ".npm",
    ".cargo",
    ".rustup",
    ".gradle",
    ".m2",
    ".venv",
    "venv",
    "__pycache__",
    ".Trash",
    "Library",
];

/// The working trees found beneath a selected folder.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Discovery {
    pub repositories: Vec<PathBuf>,
    /// Folders that could not be read and were skipped.
    pub skipped_directories: usize,
    /// True when the scan stopped at the repository limit.
    pub truncated: bool,
}

pub fn normalize_ssh_destination(value: &str) -> Result<String, GitError> {
    let trimmed = value.trim();
    let without_prefix = match trimmed.split_at_checked(3) {
        Some((prefix, rest))
            if prefix.eq_ignore_ascii_case("ssh") && rest.starts_with(char::is_whitespace) =>
        {
            rest.trim_start()
        }
        _ => trimmed,
    };
    let destination = without_prefix.trim();
    if destination.is_empty()
        || destination.eq_ignore_ascii_case("ssh")
        || destination.starts_with('-')
        || !destination
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-@[]:".contains(&byte))
    {
        return Err(GitError::new(
            ErrorKind::PermissionDenied,
            "enter an SSH host alias or user@host without options",
        ));
    }
    Ok(destination.to_owned())
}

pub fn normalize_remote_root(value: &str) -> Result<PathBuf, GitError> {
    let value = value.trim();
    if value.is_empty() || value == "." || value == "~" || value == "~/" {
        return Ok(PathBuf::from("."));
    }
    if let Some(home_relative) = value.strip_prefix("~/") {
        return Ok(PathBuf::from(home_relative));
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(GitError::new(
            ErrorKind::PathMissing,
            "use ~/folder for a folder in the remote home directory, or an absolute remote path",
        ));
    }
    Ok(path)
}

pub fn discover_local_repositories(
    selected_path: &Path,
    git_executable: PathBuf,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<Discovery, GitError> {
    match selected_path.try_exists() {
        Ok(true) => {}
        Ok(false) => {
            return Err(GitError::new(
                ErrorKind::PathMissing,
                "the selected folder no longer exists",
            ));
        }
        Err(error) => {
            return Err(GitError::new(
                ErrorKind::PermissionDenied,
                format!("cannot access the selected folder: {error}"),
            ));
        }
    }
    if !selected_path.is_dir() {
        return Err(GitError::new(
            ErrorKind::PathMissing,
            "select a folder to scan for Git working trees",
        ));
    }

    let runner = CommandRunner::new(git_executable, timeout);
    if let Some(root) = repository_root(selected_path, &runner, cancellation)? {
        return Ok(Discovery {
            repositories: vec![root],
            ..Discovery::default()
        });
    }

    let mut repositories = BTreeSet::new();
    let mut skipped_directories = 0;
    let mut truncated = false;
    let mut pending = vec![selected_path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        if cancellation.is_cancelled() {
            return Err(GitError::new(
                ErrorKind::Cancelled,
                "repository scan canceled",
            ));
        }
        if directory.join(".git").symlink_metadata().is_ok() {
            let canonical = fs::canonicalize(&directory).map_err(|error| {
                GitError::new(
                    ErrorKind::PermissionDenied,
                    format!("cannot canonicalize a discovered repository: {error}"),
                )
            })?;
            repositories.insert(canonical);
            if repositories.len() >= MAX_DISCOVERED_REPOSITORIES {
                truncated = true;
                break;
            }
            continue;
        }

        // Only the selected folder itself must be readable. Protected or
        // foreign-owned folders beneath it are skipped and counted, so one
        // unreadable folder does not hide every other repository.
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if directory == selected_path => {
                return Err(GitError::new(
                    ErrorKind::PermissionDenied,
                    format!("cannot scan {}: {error}", directory.display()),
                ));
            }
            Err(_) => {
                skipped_directories += 1;
                continue;
            }
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() && !file_type.is_symlink() {
                pending.push(entry.path());
            }
        }
    }
    Ok(Discovery {
        repositories: repositories.into_iter().collect(),
        skipped_directories,
        truncated,
    })
}

pub fn discover_remote_repositories(
    destination: &str,
    selected_root: &Path,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<Discovery, GitError> {
    let runner = CommandRunner::remote(destination.to_owned(), timeout);
    if let Some(root) = repository_root(selected_root, &runner, cancellation)? {
        return Ok(Discovery {
            repositories: vec![root],
            ..Discovery::default()
        });
    }

    let mut parts = vec![
        OsString::from("find"),
        shell_quote(selected_root.as_os_str()),
        OsString::from("-type"),
        OsString::from("d"),
        OsString::from("\\("),
    ];
    for (index, name) in REMOTE_PRUNED_DIRECTORIES.iter().enumerate() {
        if index > 0 {
            parts.push(OsString::from("-o"));
        }
        parts.push(OsString::from("-name"));
        parts.push(shell_quote(OsStr::new(name)));
    }
    parts.extend([
        OsString::from("\\)"),
        OsString::from("-prune"),
        OsString::from("-o"),
        OsString::from("-name"),
        shell_quote(OsStr::new(".git")),
        OsString::from("-print0"),
        OsString::from("-prune"),
    ]);
    let output = runner
        .run_remote_shell(join_command_parts(parts), cancellation)
        .map_err(command_error)?;
    // `find` exits non-zero when any folder is unreadable, but still prints
    // every repository it could reach. Fail only when it found nothing.
    let skipped_directories = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    if !output.status.success() && output.stdout.is_empty() {
        return Err(failed_output(output, ErrorKind::PathMissing));
    }

    let mut markers = BTreeSet::new();
    for marker in output.stdout.split(|byte| *byte == 0) {
        if marker.is_empty() {
            continue;
        }
        let marker = PathBuf::from(OsString::from(String::from_utf8_lossy(marker).into_owned()));
        if let Some(parent) = marker.parent() {
            markers.insert(parent.to_path_buf());
        }
    }
    let mut repositories = outermost_roots(markers);
    let truncated = repositories.len() > MAX_DISCOVERED_REPOSITORIES;
    repositories.truncate(MAX_DISCOVERED_REPOSITORIES);
    Ok(Discovery {
        repositories,
        skipped_directories,
        truncated,
    })
}

/// Drops repositories nested inside another discovered repository (for
/// example submodules or vendored checkouts), matching local discovery, which
/// stops descending at a repository root. `BTreeSet` orders paths by
/// component, so every descendant directly follows its ancestor.
fn outermost_roots(paths: BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for path in paths {
        if roots.last().is_some_and(|root| path.starts_with(root)) {
            continue;
        }
        roots.push(path);
    }
    roots
}

fn repository_root(
    selected_path: &Path,
    runner: &CommandRunner,
    cancellation: &CancellationToken,
) -> Result<Option<PathBuf>, GitError> {
    let output = runner
        .run_git(
            Some(selected_path),
            [
                OsString::from("rev-parse"),
                OsString::from("--path-format=absolute"),
                OsString::from("--show-toplevel"),
            ],
            cancellation,
        )
        .map_err(command_error)?;
    if !output.status.success() {
        let error = failed_output(output, ErrorKind::NotRepository);
        return if error.kind == ErrorKind::NotRepository {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if root.is_empty() {
        return Ok(None);
    }
    let root = PathBuf::from(root);
    if runner.is_remote() {
        Ok(Some(root))
    } else {
        fs::canonicalize(root)
            .map(Some)
            .map_err(|error| GitError::new(ErrorKind::PermissionDenied, error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn validates_and_normalizes_remote_inputs() {
        assert_eq!(
            normalize_ssh_destination(" ssh user@workstation "),
            Ok("user@workstation".to_owned())
        );
        assert_eq!(
            normalize_ssh_destination("SSH  workstation"),
            Ok("workstation".to_owned())
        );
        assert_eq!(
            normalize_ssh_destination("ssh\tworkstation"),
            Ok("workstation".to_owned())
        );
        assert!(normalize_ssh_destination("ssh ").is_err());
        assert!(normalize_ssh_destination("-oProxyCommand=bad").is_err());
        assert_eq!(normalize_remote_root("").unwrap(), PathBuf::from("."));
        assert_eq!(
            normalize_remote_root("~/Git").unwrap(),
            PathBuf::from("Git")
        );
        assert_eq!(normalize_remote_root("~").unwrap(), PathBuf::from("."));
        assert_eq!(
            normalize_remote_root("/srv/projects").unwrap(),
            PathBuf::from("/srv/projects")
        );
        assert!(normalize_remote_root("relative/projects").is_err());
        assert!(normalize_remote_root("~another-user/projects").is_err());
    }

    #[test]
    fn local_discovery_stops_at_repository_roots_and_skips_symlinks() {
        let temporary = tempdir().unwrap();
        let first = temporary.path().join("group/first");
        let nested = first.join("nested");
        let second = temporary.path().join("second");
        fs::create_dir_all(first.join(".git")).unwrap();
        fs::create_dir_all(nested.join(".git")).unwrap();
        fs::create_dir_all(second.join(".git")).unwrap();
        std::os::unix::fs::symlink(&second, temporary.path().join("linked")).unwrap();

        let repositories = discover_local_repositories(
            temporary.path(),
            PathBuf::from("/usr/bin/git"),
            Duration::from_secs(10),
            &CancellationToken::default(),
        )
        .unwrap()
        .repositories;

        assert_eq!(
            repositories,
            vec![
                fs::canonicalize(first).unwrap(),
                fs::canonicalize(second).unwrap()
            ]
        );
    }

    #[test]
    fn local_discovery_skips_unreadable_subfolders() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repository");
        let locked = temporary.path().join("locked");
        fs::create_dir_all(repository.join(".git")).unwrap();
        fs::create_dir_all(locked.join("inner")).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let discovery = discover_local_repositories(
            temporary.path(),
            PathBuf::from("/usr/bin/git"),
            Duration::from_secs(10),
            &CancellationToken::default(),
        );
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let discovery = discovery.unwrap();

        assert_eq!(
            discovery.repositories,
            vec![fs::canonicalize(repository).unwrap()]
        );
        assert_eq!(discovery.skipped_directories, 1);
    }

    #[test]
    fn nested_remote_roots_collapse_to_the_outermost_repository() {
        let paths = BTreeSet::from([
            PathBuf::from("./a"),
            PathBuf::from("./a/sub"),
            PathBuf::from("./a-b"),
            PathBuf::from("./c/d"),
        ]);

        assert_eq!(
            outermost_roots(paths),
            vec![
                PathBuf::from("./a"),
                PathBuf::from("./a-b"),
                PathBuf::from("./c/d")
            ]
        );
    }
}
