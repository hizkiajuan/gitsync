use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TERMINATION_GRACE: Duration = Duration::from_millis(100);
const DEFAULT_OUTPUT_LIMIT: usize = 256 * 1024;

#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub(crate) struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
}

#[derive(Debug)]
pub(crate) enum CommandError {
    Spawn(io::Error),
    Io(io::Error),
    Timeout,
    Cancelled,
    OutputTooLarge,
}

#[derive(Clone, Debug)]
pub(crate) struct CommandRunner {
    executable: PathBuf,
    ssh_destination: Option<String>,
    timeout: Duration,
    output_limit: usize,
}

impl CommandRunner {
    pub fn new(executable: PathBuf, timeout: Duration) -> Self {
        Self {
            executable,
            ssh_destination: None,
            timeout,
            output_limit: DEFAULT_OUTPUT_LIMIT,
        }
    }

    pub fn remote(destination: String, timeout: Duration) -> Self {
        Self {
            executable: PathBuf::from("git"),
            ssh_destination: Some(destination),
            timeout,
            output_limit: DEFAULT_OUTPUT_LIMIT,
        }
    }

    pub fn is_remote(&self) -> bool {
        self.ssh_destination.is_some()
    }

    pub fn run_git(
        &self,
        working_tree: Option<&Path>,
        arguments: impl IntoIterator<Item = OsString>,
        cancellation: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        let arguments: Vec<OsString> = arguments.into_iter().collect();
        let mut command = if let Some(destination) = &self.ssh_destination {
            let mut command = ssh_command(destination);
            command.arg(remote_git_command(
                &self.executable,
                working_tree,
                &arguments,
            ));
            command
        } else {
            let mut command = Command::new(&self.executable);
            if let Some(working_tree) = working_tree {
                command.arg("-C").arg(working_tree);
            }
            command.args(arguments);
            command
        };
        self.run_command(&mut command, cancellation)
    }

    /// Runs the runner's executable on this Mac, never over SSH.
    pub fn run_local(
        &self,
        arguments: impl IntoIterator<Item = OsString>,
        cancellation: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        debug_assert!(!self.is_remote(), "local commands need a local runner");
        let mut command = Command::new(&self.executable);
        command
            .args(arguments)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1");
        self.run_command(&mut command, cancellation)
    }

    pub fn run_remote_shell(
        &self,
        command_text: OsString,
        cancellation: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        let destination = self
            .ssh_destination
            .as_ref()
            .expect("remote shell commands require an SSH runner");
        let mut command = ssh_command(destination);
        command.arg(command_text);
        self.run_command(&mut command, cancellation)
    }

    fn run_command(
        &self,
        command: &mut Command,
        cancellation: &CancellationToken,
    ) -> Result<CommandOutput, CommandError> {
        if cancellation.is_cancelled() {
            return Err(CommandError::Cancelled);
        }

        // `GIT_OPTIONAL_LOCKS=0` stops `git status` from taking `index.lock`
        // to refresh stat data, so background checks never collide with the
        // user's own commits.
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // SAFETY: `setpgid`, `sigemptyset`, and `pthread_sigmask` are
        // async-signal-safe and run in the child before exec. Giving each Git
        // invocation its own process group lets timeout and cancellation
        // terminate helpers (for example SSH) as well as Git.
        //
        // Commands start on GPUI background (GCD) threads, which block most
        // signals, and a blocked mask survives exec. Git stops its helpers
        // with SIGTERM when a command fails (for example `git-upload-pack`
        // after a fetch of a deleted branch) and then waits for them, so an
        // inherited mask makes the command hang until the timeout. Clear it.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                if libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn().map_err(CommandError::Spawn)?;
        let stdout = child
            .stdout
            .take()
            .expect("stdout is piped before the process starts");
        let stderr = child
            .stderr
            .take()
            .expect("stderr is piped before the process starts");
        let output_limit = self.output_limit;
        let stdout_reader = thread::spawn(move || read_bounded(stdout, output_limit));
        let stderr_reader = thread::spawn(move || read_bounded(stderr, output_limit));

        let started_at = Instant::now();
        let status = loop {
            if cancellation.is_cancelled() {
                terminate_process_group(&mut child);
                join_reader(stdout_reader)?;
                join_reader(stderr_reader)?;
                return Err(CommandError::Cancelled);
            }
            if started_at.elapsed() >= self.timeout {
                terminate_process_group(&mut child);
                join_reader(stdout_reader)?;
                join_reader(stderr_reader)?;
                return Err(CommandError::Timeout);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    terminate_process_group(&mut child);
                    join_reader(stdout_reader)?;
                    join_reader(stderr_reader)?;
                    return Err(CommandError::Io(error));
                }
            }
            thread::sleep(POLL_INTERVAL);
        };

        let (stdout, stdout_truncated) = join_reader(stdout_reader)?;
        let (stderr, stderr_truncated) = join_reader(stderr_reader)?;
        if stdout_truncated {
            return Err(CommandError::OutputTooLarge);
        }

        Ok(CommandOutput {
            status,
            stdout,
            stderr,
            stderr_truncated,
        })
    }
}

/// Builds an OpenSSH invocation for `destination`.
///
/// GitSync only runs Git inspection and fetch commands on the remote host, so
/// forwards from the user's SSH config are never needed. Clearing them avoids
/// "cannot listen to port" noise when a configured local forward collides on
/// this Mac. A shared control connection lets the several commands of one
/// refresh reuse a single SSH session instead of opening one each.
fn ssh_command(destination: &str) -> Command {
    let mut command = Command::new("/usr/bin/ssh");
    command.args([
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectionAttempts=1",
        "-o",
        "ClearAllForwardings=yes",
    ]);
    if let Some(directory) = control_directory() {
        let mut control_path = directory.as_os_str().to_owned();
        control_path.push("/%C");
        command
            .args(["-o", "ControlMaster=auto", "-o", "ControlPersist=60", "-o"])
            .arg({
                let mut option = OsString::from("ControlPath=");
                option.push(control_path);
                option
            });
    }
    command.arg("--").arg(destination);
    command
}

/// A private, short directory for SSH control sockets. Unix socket paths are
/// limited to about 104 bytes and OpenSSH appends a temporary suffix while it
/// creates the socket, so the Application Support folder is too long.
/// Returns `None` (no connection sharing) when the directory is not safe.
fn control_directory() -> Option<&'static Path> {
    static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
            // SAFETY: `getuid` has no preconditions and cannot fail.
            let uid = unsafe { libc::getuid() };
            let directory = PathBuf::from(format!("/tmp/gitsync-{uid}"));
            let _ = std::fs::DirBuilder::new().mode(0o700).create(&directory);
            let metadata = std::fs::symlink_metadata(&directory).ok()?;
            (metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0)
                .then_some(directory)
        })
        .as_deref()
}

pub(crate) fn shell_quote(value: &OsStr) -> OsString {
    let mut quoted = Vec::with_capacity(value.as_bytes().len() + 2);
    quoted.push(b'\'');
    for byte in value.as_bytes() {
        if *byte == b'\'' {
            quoted.extend_from_slice(b"'\"'\"'");
        } else {
            quoted.push(*byte);
        }
    }
    quoted.push(b'\'');
    OsString::from_vec(quoted)
}

fn remote_git_command(
    executable: &Path,
    working_tree: Option<&Path>,
    arguments: &[OsString],
) -> OsString {
    let mut parts = vec![
        OsString::from("env"),
        OsString::from("GIT_TERMINAL_PROMPT=0"),
        OsString::from("GIT_OPTIONAL_LOCKS=0"),
        OsString::from("LC_ALL=C"),
        OsString::from("LANG=C"),
        shell_quote(executable.as_os_str()),
    ];
    if let Some(working_tree) = working_tree {
        parts.push(OsString::from("-C"));
        parts.push(shell_quote(working_tree.as_os_str()));
    }
    parts.extend(
        arguments
            .iter()
            .map(|argument| shell_quote(argument.as_os_str())),
    );
    join_command_parts(parts)
}

pub(crate) fn join_command_parts(parts: impl IntoIterator<Item = OsString>) -> OsString {
    let mut command = Vec::new();
    for part in parts {
        if !command.is_empty() {
            command.push(b' ');
        }
        command.extend_from_slice(part.as_bytes());
    }
    OsString::from_vec(command)
}

fn read_bounded(mut reader: impl Read, limit: usize) -> Result<(Vec<u8>, bool), io::Error> {
    let mut retained = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok((retained, truncated));
        }

        let remaining = limit.saturating_sub(retained.len());
        let retained_count = remaining.min(count);
        retained.extend_from_slice(&buffer[..retained_count]);
        truncated |= retained_count < count;
    }
}

fn join_reader(
    reader: thread::JoinHandle<Result<(Vec<u8>, bool), io::Error>>,
) -> Result<(Vec<u8>, bool), CommandError> {
    reader
        .join()
        .map_err(|_| CommandError::Io(io::Error::other("output reader panicked")))?
        .map_err(CommandError::Io)
}

fn terminate_process_group(child: &mut Child) {
    let process_group = -(child.id() as i32);

    // SAFETY: The process group ID was created by `setpgid` above. ESRCH only
    // means the process exited between `try_wait` and this signal.
    unsafe {
        libc::kill(process_group, libc::SIGTERM);
    }

    let deadline = Instant::now() + TERMINATION_GRACE;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(_) => break,
        }
    }

    // SAFETY: As above; SIGKILL is the bounded fallback after the grace period.
    unsafe {
        libc::kill(process_group, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_terminates_a_process() {
        let runner = CommandRunner::new(PathBuf::from("/bin/sleep"), Duration::from_millis(30));
        let result = runner.run_git(None, [OsString::from("5")], &CancellationToken::default());

        assert!(matches!(result, Err(CommandError::Timeout)));
    }

    #[test]
    fn children_do_not_inherit_a_blocked_sigterm() {
        // Run on a fresh thread so the blocked mask cannot leak into other
        // tests, like the GCD worker threads that start commands in the app.
        let output = thread::spawn(|| {
            // SAFETY: Blocks SIGTERM on this short-lived test thread only.
            unsafe {
                let mut blocked: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut blocked);
                libc::sigaddset(&mut blocked, libc::SIGTERM);
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut());
            }
            let runner = CommandRunner::new(PathBuf::from("/bin/sh"), Duration::from_secs(5));
            runner.run_git(
                None,
                [
                    OsString::from("-c"),
                    OsString::from("kill -TERM $$; sleep 3"),
                ],
                &CancellationToken::default(),
            )
        })
        .join()
        .expect("test thread should not panic")
        .expect("the shell should start and be terminated");

        assert!(!output.status.success(), "SIGTERM must end the child");
    }

    #[test]
    fn cancellation_before_spawn_is_observed() {
        let token = CancellationToken::default();
        token.cancel();
        let runner = CommandRunner::new(PathBuf::from("/usr/bin/git"), Duration::from_secs(1));
        let result = runner.run_git(None, [OsString::from("--version")], &token);

        assert!(matches!(result, Err(CommandError::Cancelled)));
    }

    #[test]
    fn preserves_nonzero_status_and_bounds_stdout() {
        let runner = CommandRunner::new(PathBuf::from("/usr/bin/false"), Duration::from_secs(10));
        let output = runner
            .run_git(None, [], &CancellationToken::default())
            .expect("a nonzero exit is still a completed command");
        assert!(!output.status.success());

        let runner = CommandRunner {
            executable: PathBuf::from("/usr/bin/printf"),
            ssh_destination: None,
            timeout: Duration::from_secs(1),
            output_limit: 4,
        };
        let result = runner.run_git(
            None,
            [OsString::from("%s"), OsString::from("abcdef")],
            &CancellationToken::default(),
        );
        assert!(matches!(result, Err(CommandError::OutputTooLarge)));
    }

    #[test]
    fn quotes_remote_shell_arguments_without_interpolation() {
        assert_eq!(shell_quote(OsStr::new("plain")), OsString::from("'plain'"));
        assert_eq!(
            shell_quote(OsStr::new("project's folder; touch /tmp/no")),
            OsString::from("'project'\"'\"'s folder; touch /tmp/no'")
        );

        let command = remote_git_command(
            Path::new("git"),
            Some(Path::new("/srv/project's copy")),
            &[OsString::from("status"), OsString::from("--porcelain=v1")],
        );
        assert_eq!(
            command,
            OsString::from(
                "env GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0 LC_ALL=C LANG=C 'git' -C '/srv/project'\"'\"'s copy' 'status' '--porcelain=v1'"
            )
        );
    }
}
