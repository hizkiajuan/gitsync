#!/bin/sh
# Builds demo repositories in every state GitSync shows, then runs GitSync on them
# with a scratch HOME, so screenshots never show your own repositories and your
# own configuration in ~/Library/Application Support/GitSync is never touched.
#
#   ./scripts/demo.sh            build the demo and launch dist/GitSync.app on it
#   ./scripts/demo.sh --no-open  only build the demo
#
# GITSYNC_DEMO_DIR   where the demo lives (default /tmp/gitsync-demo; rebuilt each run)
# GITSYNC_DEMO_HOST  an SSH alias that reaches this Mac, shown as a second computer
#                    (default build-box; empty skips it). Add it to ~/.ssh/config:
#                      Host build-box
#                        HostName localhost
#                    Remote Login must be on and your own key in ~/.ssh/authorized_keys.
# GITSYNC_APP        the app bundle to launch (default dist/GitSync.app)
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
demo=${GITSYNC_DEMO_DIR:-/tmp/gitsync-demo}
host=${GITSYNC_DEMO_HOST-build-box}
app=${GITSYNC_APP:-$repo_root/dist/GitSync.app}
open_app=1
[ "${1:-}" = "--no-open" ] && open_app=0

rm -rf "$demo"
mkdir -p "$demo/home" "$demo/remotes" "$demo/mac/Developer" "$demo/box/Git" "$demo/work"
demo=$(CDPATH= cd -- "$demo" && pwd -P)

# Fixed identity and dates, and no user or system Git configuration.
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME="Demo" GIT_AUTHOR_EMAIL="demo@example.com"
export GIT_COMMITTER_NAME="Demo" GIT_COMMITTER_EMAIL="demo@example.com"
export GIT_AUTHOR_DATE="2026-11-20T09:00:00" GIT_COMMITTER_DATE="2026-11-20T09:00:00"

g() { git -c init.defaultBranch=main -c advice.detachedHead=false "$@"; }

commit() { # commit <dir> <file> <message>
    printf '%s\n' "$3" >>"$1/$2"
    g -C "$1" add -A >/dev/null
    g -C "$1" commit -qm "$3"
}

# repo <side: mac|box> <name> <state> [count]
# States: clean, ahead, behind, diverged, no-upstream, upstream-gone, dirty
repo() {
    side=$1 name=$2 state=$3 count=${4:-1}
    case $side in mac) parent=$demo/mac/Developer ;; *) parent=$demo/box/Git ;; esac
    bare=$demo/remotes/$name.git
    dir=$parent/$name
    other=$demo/work/$name
    g init -q --bare "$bare"
    g clone -q "$bare" "$dir" 2>/dev/null
    commit "$dir" README.md "# $name"
    commit "$dir" CHANGELOG.md "Initial version"
    g -C "$dir" push -q -u origin main
    g clone -q "$bare" "$other" 2>/dev/null
    i=0
    case $state in
    clean) ;;
    ahead)
        while [ $i -lt "$count" ]; do i=$((i + 1)); commit "$dir" notes.md "Local change $i"; done ;;
    behind)
        while [ $i -lt "$count" ]; do i=$((i + 1)); commit "$other" notes.md "Teammate change $i"; done
        g -C "$other" push -q
        g -C "$dir" fetch -q ;;
    diverged)
        commit "$dir" notes.md "Local change"; commit "$dir" notes.md "Second local change"
        commit "$other" other.md "Teammate change"; g -C "$other" push -q
        g -C "$dir" fetch -q ;;
    no-upstream)
        g -C "$dir" switch -q -c feature/backup-rotation
        commit "$dir" backup.sh "Rotate backups weekly" ;;
    upstream-gone)
        g -C "$dir" switch -q -c fix/retry-webhooks
        commit "$dir" retry.md "Retry failed webhooks"
        g -C "$dir" push -q -u origin fix/retry-webhooks
        g -C "$other" push -q origin --delete fix/retry-webhooks
        g -C "$dir" fetch -q --prune ;;
    dirty)
        printf 'draft\n' >>"$dir/README.md"
        printf 'idea\n' >"$dir/ideas.md" ;;
    esac
    printf '%s\n' "$side|$dir" >>"$demo/repos.txt"
}

: >"$demo/repos.txt"
repo mac notes-app ahead 3
repo mac dotfiles behind 2
repo mac design-tokens ahead 1
repo mac blog dirty
repo mac field-guide clean
repo mac recipe-box clean
repo mac photo-sorter clean
repo mac budget-sheet clean
use_host=0
if [ -n "$host" ] && ssh -o BatchMode=yes -o ConnectTimeout=5 "$host" true 2>/dev/null; then
    use_host=1
    repo box api-gateway diverged
    repo box infra-scripts no-upstream
    repo box billing-worker upstream-gone
    repo box search-indexer clean
    repo box data-pipeline clean
    repo box status-page clean
elif [ -n "$host" ]; then
    echo "demo: ssh $host doesn't connect in batch mode; skipping the second computer (see the comment at the top)." >&2
fi

# GitSync's configuration in the scratch HOME: every demo repository, fetch allowed.
store="$demo/home/Library/Application Support/GitSync"
mkdir -p "$store"
python3 - "$demo/repos.txt" "$host" "$store/config.json" <<'PY'
import json, sys, time, uuid
repos_file, host, out = sys.argv[1:4]
now = {"secs_since_epoch": int(time.time()), "nanos_since_epoch": 0}
records = []
for line in open(repos_file):
    side, path = line.strip().split("|", 1)
    records.append({
        "id": str(uuid.uuid4()),
        "selected_path": path,
        "canonical_path": path,
        "ssh_destination": host if side == "box" else None,
        "display_name": None,
        "paused": False,
        "network_access_allowed": True,
        "added_at": now,
        "updated_at": now,
    })
json.dump({"schema_version": 1, "settings": {}, "repositories": records}, open(out, "w"), indent=2)
PY

count=$(wc -l <"$demo/repos.txt" | tr -d ' ')
echo "demo: $count repositories in $demo$( [ $use_host = 1 ] && echo " (this Mac and $host)")"
if [ $open_app = 1 ]; then
    if pgrep -x gitsync >/dev/null 2>&1; then
        echo "demo: GitSync is already running; quit it, then run ./scripts/demo.sh again (both would claim ⌘G)." >&2
        exit 1
    fi
    [ -d "$app" ] || { echo "demo: $app is missing; run ./scripts/bundle.sh first." >&2; exit 1; }
    open -n --env "HOME=$demo/home" "$app"
    echo "demo: GitSync is running on the demo. Press ⌘G, take the screenshots, then quit it from Settings."
fi
