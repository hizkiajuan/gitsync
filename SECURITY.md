# Security policy

## Supported versions

Only the latest release receives security fixes.

## Reporting a vulnerability

Please do not open a public issue. Report the problem privately through
GitHub: **Security → Report a vulnerability** on
[hizkiajuan/gitsync](https://github.com/hizkiajuan/gitsync/security/advisories/new).
If you cannot use GitHub, email oss@hizkiajuan.com.

Include the GitSync and macOS versions, the steps to reproduce, and the
effect you observed. You will get a first reply within 7 days. When the fix is
released, the advisory is published and credits you unless you ask otherwise.

## Scope

GitSync runs `git` and `/usr/bin/ssh` on your Mac and on the SSH computers you
add, and stores its data under `~/Library/Application Support/GitSync/`.
Problems in that boundary are in scope, for example a command that writes to a
repository, a leaked path or credential in notifications or diagnostics, or an
injection through a repository path, branch name, or SSH destination.
