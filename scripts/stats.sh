#!/usr/bin/env bash
# Print download + traffic stats for the claws GitHub repo.
#
# Reads from the GitHub API via the `gh` CLI (uses your existing auth).
# `gh` has jq embedded — no separate jq install needed.
# Pure read-only — no telemetry, no third-party services.
#
# Usage:  scripts/stats.sh
# Deps:   gh (https://cli.github.com)

set -euo pipefail

REPO="${REPO:-dodontommy/claws}"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
dim()  { printf '\033[2m%s\033[0m\n' "$*"; }

bold "== releases & download counts ($REPO) =="
echo

# Per-release totals, newest first.
gh api --paginate "repos/$REPO/releases" --jq '
  [.[] | {
    tag: .tag_name,
    published: ((.published_at // "(draft)") | .[:10]),
    total: ([.assets[].download_count] | add // 0)
  }]
  | sort_by(.published) | reverse
  | .[] | "\(.tag)\t\(.published)\ttotal: \(.total)"
'

echo
total=$(gh api --paginate "repos/$REPO/releases" --jq '
  .[] | .assets[].download_count
' | awk '{s+=$1} END {print s+0}')
bold "lifetime asset downloads: $total"
dim "(does NOT include Homebrew installs from the tap, or cargo install users)"

echo
bold "== latest release per-asset =="
echo
gh release view --repo "$REPO" --json tagName,assets --jq '
  "tag: \(.tagName)",
  (.assets[] | "  \(.downloadCount)\t\(.name)")
'

echo
bold "== traffic (last 14 days) =="
echo
v_count=$(gh api "repos/$REPO/traffic/views"  --jq '.count // 0' 2>/dev/null || echo 0)
v_uniq=$(gh api  "repos/$REPO/traffic/views"  --jq '.uniques // 0' 2>/dev/null || echo 0)
c_count=$(gh api "repos/$REPO/traffic/clones" --jq '.count // 0' 2>/dev/null || echo 0)
c_uniq=$(gh api  "repos/$REPO/traffic/clones" --jq '.uniques // 0' 2>/dev/null || echo 0)
echo "views:    $v_count total / $v_uniq unique"
echo "clones:   $c_count total / $c_uniq unique"

echo
bold "== top referrers =="
echo
printf "views\tuniques\tsource\n"
gh api "repos/$REPO/traffic/popular/referrers" --jq '
  .[] | "\(.count)\t\(.uniques)\t\(.referrer)"
' 2>/dev/null || echo "(none)"

echo
bold "== top paths =="
echo
printf "views\tuniques\tpath\n"
gh api "repos/$REPO/traffic/popular/paths" --jq '
  .[] | "\(.count)\t\(.uniques)\t\(.path)"
' 2>/dev/null || echo "(none)"
