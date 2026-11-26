#!/usr/bin/env bash
# Release gate: succeeds only when the CI workflow passed on the commit being released (HEAD).
set -euo pipefail
sha=$(git rev-parse HEAD)
conclusion=$(gh run list --commit "$sha" --workflow ci.yml --json conclusion --jq '.[0].conclusion // "none"')
if [[ $conclusion != success ]]; then
  echo "CI on ${sha:0:7} is '$conclusion', not success; wait for it or fix main first." >&2
  exit 1
fi
echo "CI passed on ${sha:0:7}."
