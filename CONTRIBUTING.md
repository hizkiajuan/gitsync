# Contributing to GitSync

Thank you for helping. GitSync is a small, focused app with one maintainer, so
please read this page before you open a pull request.

## What is welcome

- **Bug reports.** Use the bug report form and include the sanitized
  diagnostic summary from **Settings → Advanced → Copy Summary**.
- **Bug-fix pull requests.** Open them directly.
- **Features.** Open an issue first and wait for a reply before you start the
  work. GitSync deliberately does a few things; read the
  [non-goals](README.md#non-goals) before you propose one.

AI-assisted contributions are welcome when you review and test the change
yourself and can explain it in the review.

## Development setup

You need macOS 13 or later and Xcode Command Line Tools. Rust 1.98.1 is
installed automatically through `rust-toolchain.toml`.

```sh
git clone https://github.com/hizkiajuan/gitsync.git
cd gitsync
cargo check
```

Optional tools: `cargo-nextest` (faster tests), `cargo-deny` (license and
advisory checks), and `sccache` (set `RUSTC_WRAPPER=sccache`).

## Checks

Every pull request must pass the same checks that CI runs:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run        # or: cargo test
cargo deny check
./scripts/bundle.sh      # builds dist/GitSync.app
```

For UI changes, add screenshots of the panel or Settings in light and dark
mode. The `GITSYNC_DEBUG_OPEN` variable described in the README opens a window
without a menu bar click.

## Landing page

The page at [gitsync.hizkiajuan.com](https://gitsync.hizkiajuan.com) lives in `site/`, a static page served by a Cloudflare Worker. Preview it with `cd site && bun install && bun run dev`. Pushes to `main` that change `site/` deploy it; other branches get a preview build.

## Pull requests

- One topic per pull request. Pull requests are squash-merged.
- The title is the commit message, so use a
  [Conventional Commit](https://www.conventionalcommits.org/) title:
  `fix: keep stale counts after a failed fetch`, `feat: …`, `docs: …`,
  `chore: …`. `feat` and `fix` titles appear in the changelog.
- Mark a change that requires user action with `!`
  (`feat!: require macOS 14`) and explain it in the description.

## Versions and releases

- Versions follow SemVer and tags are `vX.Y.Z`. The only version source is
  `version` in `Cargo.toml`; `scripts/bundle.sh` writes it into the bundle.
- Before 1.0, a new feature or a breaking change bumps the minor version
  (`0.2.0`) and a fix bumps the patch version (`0.1.1`).
- "Breaking" means the user must act: a higher minimum macOS version, a
  configuration reset, or a removed feature. An internal refactor with no
  visible change is a patch.
- The bundle build number (`CFBundleVersion`) is the commit count, so it always
  goes up.
- The maintainer cuts releases with `hzkjn-release` once CI is green on
  `main`. It picks the bump from the squash-merged PR titles, writes the
  version to `Cargo.toml` and `Cargo.lock`, adds a `CHANGELOG.md` section,
  tags, pushes, and publishes the GitHub release. The tag starts the release
  workflow, which checks and bundles the tagged commit on macOS.

## Release checklist

The maintainer runs this on a fresh `./scripts/bundle.sh` build before cutting
a release. Use a scratch `HOME` or a test account, and record the
macOS version and architecture.

**Panel and discovery**

- [ ] The menu bar shows the mark and no Dock icon. ⌘G opens and closes the
      panel from another app; clicking another app or pressing Escape closes it.
- [ ] **+ → Add Local Folder…** imports every working tree, skips nested
      contents, does not follow symlinks, counts unreadable folders, and can be
      canceled. GitSync then asks once for fetch consent.
- [ ] Adding the same folder again adds nothing twice. After moving a repository to another folder and scanning its parent again, the old entry moves (name and consent kept) and no failing duplicate stays. **Change Folder…** and editing the path in Settings point a repository at a new folder; a folder that isn't a working tree, or one already monitored, is refused.
- [ ] **+ → Add SSH Computer…**: **Test Connection** reports the remote Git
      version and repository count. A scan adds the repositories under a new
      rail item. Unknown host, rejected key, and timeout show a recoverable
      error in the SSH window, never a password prompt.

**Status**

- [ ] Rows reach **Up to date**, **Push N**, **Pull N**, diverged, or a
      specific problem (**Upstream needed**, **Detached HEAD**,
      **Upstream deleted**, **Auth needed**).
- [ ] A modified and an untracked file give a blue **2 uncommitted** pill; a
      merge conflict shows **Conflicts**.
- [ ] Offline, rows become **Unverified** (or keep **Push N**), never red, and
      a failed fetch keeps the old counts as stale.
- [ ] `git commit` in a repository during a refresh never hits `index.lock`.
- [ ] After a relaunch, rows show last-known data at once and then refresh.

**Settings**

- [ ] Refresh interval (including Manual), fetch on/off, appearance, Git
      executable **Test**, and timeouts persist after a relaunch.
- [ ] Repository and computer renames persist; a blank name restores the
      default.
- [ ] Pause, Remove (with cancel once), and group Remove change only GitSync
      metadata. Repository files stay unchanged.
- [ ] A file picker left open for 20 seconds and canceled does not quit the
      app.

**macOS integration**

- [ ] Notifications: permission is asked only when enabled; one combined
      banner per check; no paths, branches, or remotes in the text; clicking
      one opens the panel on that repository.
- [ ] Launch at login turns on (with approval in System Settings) and off.
- [ ] Refresh after wake runs once after sleep.
- [ ] **Copy Summary** and the logs contain no paths, names, branches,
      remotes, or credentials. **Reset…** clears GitSync data and leaves
      repositories untouched.
- [ ] Light and dark mode, both menu bar edges, a second display, and
      VoiceOver button names look right. **Quit** removes the status item.

## Security

Do not report vulnerabilities in public issues. See [SECURITY.md](SECURITY.md).

## Code of conduct

This project follows the [Contributor Covenant](https://github.com/hizkiajuan/.github/blob/main/CODE_OF_CONDUCT.md).
