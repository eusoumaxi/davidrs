#!/usr/bin/env bash
# Every check CI runs, runnable locally.
#
#   scripts/check.sh                     everything, in order
#   scripts/check.sh <step>              one step: rules lint test features docs site
#                                        package spelling workflows deny coverage
#   scripts/check.sh feature <name>      one feature alone
#   scripts/check.sh branch [name]       a branch name (default: the current branch)
#   scripts/check.sh commit-msg <file>   a commit message, as the commit-msg hook does
#   scripts/check.sh commits <range>     every commit message in a range, merges excluded
#   scripts/check.sh release <tag>       a release tag against Cargo.toml, CHANGELOG.md and main
#   scripts/check.sh release-notes <tag> the CHANGELOG.md section of a release
#
# CI calls the same steps, so a green local run is a green CI run.
set -euo pipefail
cd "$(dirname "$0")/.."

# Line coverage (percent) the whole crate must keep.
COVERAGE_FLOOR="${COVERAGE_FLOOR:-98}"

# The Conventional Commits types. A branch name starts with one of them too.
TYPES='build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test'

say() { printf '\n==> %s\n' "$*"; }
fail() {
  printf '%s\n' "$@" >&2
  exit 1
}

# Every public feature, from Cargo.toml.
features() {
  cargo metadata --no-deps --format-version 1 --locked \
    | jq -r '.packages[0].features | keys[] | select(startswith("_") | not) | select(. != "default")'
}

# The version in Cargo.toml.
version() {
  cargo metadata --no-deps --format-version 1 --locked | jq -r '.packages[0].version'
}

rules() {
  say "comments are rustdoc only"
  if grep -rnE '^\s*//([^/!]|$)' src tests examples docs README.md; then
    echo "plain // comments found: move them into the item's /// docs" >&2
    exit 1
  fi
  say "tests live in tests/"
  if grep -rn '#\[cfg(test)\]' src; then
    echo "#[cfg(test)] found in src/: move the test to tests/" >&2
    exit 1
  fi
}

lint() {
  say "format"
  cargo fmt --all --check
  say "clippy, all features"
  cargo clippy --locked --all-targets --all-features -- -D warnings
  say "no features"
  cargo clippy --locked --all-targets --no-default-features -- -D warnings
}

test_all() {
  say "tests, all features"
  cargo test --locked --all-features
}

feature() {
  say "feature $1 alone"
  cargo clippy --locked --all-targets --no-default-features --features "$1" -- -D warnings
  cargo test --locked --no-default-features --features "$1" --tests
}

each_feature() {
  for name in $(features); do feature "$name"; done
}

docs() {
  say "docs"
  RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features
}

# The GitHub Pages site: the API reference and the guide, opening on the crate.
site() {
  docs
  printf '<!doctype html><meta charset="utf-8"><meta http-equiv="refresh" content="0; url=davidrs/index.html"><title>davidrs</title><a href="davidrs/index.html">davidrs documentation</a>\n' \
    > target/doc/index.html
  touch target/doc/.nojekyll
}

package() {
  say "package"
  cargo package --locked --allow-dirty --all-features --quiet
}

spelling() {
  say "spelling"
  typos
}

# The GitHub Actions workflows (actionlint also runs shellcheck on their
# scripts) and the shell scripts.
workflows() {
  say "workflows and scripts"
  actionlint
  shellcheck scripts/check.sh .githooks/commit-msg
}

deny() {
  say "dependency policy"
  cargo deny --all-features --locked check
}

coverage() {
  say "coverage (floor ${COVERAGE_FLOOR}% of lines)"
  cargo llvm-cov --locked --all-features --summary-only --fail-under-lines "$COVERAGE_FLOOR"
}

# <type>/<description>: lowercase words joined by `-` (dots allowed, for a
# version). `main`, Dependabot's branches and the branches GitHub's revert
# button creates are accepted as they are. A detached HEAD (a rebase, a
# bisect) has no name and passes.
branch() {
  local LC_ALL=C name
  name=${1:-$(git symbolic-ref --quiet --short HEAD || true)}
  local conventional="^($TYPES)/[a-z0-9]+([.-][a-z0-9]+)*$"
  if [[ -z $name || $name == main || $name == dependabot/* || $name =~ ^revert-[0-9]+- ]]; then
    return 0
  fi
  if [[ ! $name =~ $conventional ]]; then
    fail "branch name '$name' does not follow <type>/<description>" \
      "  type: ${TYPES//|/, }" \
      "  description: lowercase words joined by '-', e.g. feat/sqs-visibility or fix/42-jwks-refresh" \
      "  rename it with: git branch -m <type>/<description>" \
      "  see CONTRIBUTING.md, \"Branches\""
  fi
}

# Conventional Commits: `<type>(<scope>)!: <description>`, the scope and the
# `!` optional, at most 72 characters without a trailing period, and a blank
# line before any body. Git's own merge, revert, fixup and squash subjects are
# accepted as they are. Comment lines and everything below the scissors line
# of `git commit --verbose` are ignored, as Git ignores them.
commit_msg() {
  local LC_ALL=C message subject
  local generated='^(Merge|Revert|fixup!|squash!|amend!) '
  local conventional="^($TYPES)(\([a-z0-9-]+\))?!?: [^[:space:]]"
  message=$(sed -e '/^# -* >8 -*$/,$d' -e '/^#/d' "$1")
  subject=$(printf '%s\n' "$message" | sed -n 1p)
  if [[ $subject =~ $generated ]]; then
    return 0
  fi
  if [[ ! $subject =~ $conventional ]]; then
    fail "commit subject '$subject' does not follow <type>(<scope>): <description>" \
      "  type: ${TYPES//|/, }" \
      "  scope: optional, lowercase, e.g. http, queue, mcp, deps" \
      "  example: fix(queue): report unattempted records as failures" \
      "  see CONTRIBUTING.md, \"Commit messages\""
  fi
  if [ "${#subject}" -gt 72 ]; then
    fail "commit subject is ${#subject} characters long; the limit is 72: '$subject'"
  fi
  if [[ $subject == *. ]]; then
    fail "commit subject ends with a period: '$subject'"
  fi
  if [[ $(printf '%s\n' "$message" | sed -n 2p) =~ [^[:space:]] ]]; then
    fail "leave a blank line between the commit subject and the body"
  fi
}

# Every commit of a pull request, as CI checks it.
commits() {
  local shas sha
  say "commit messages in $1"
  shas=$(git rev-list --no-merges --reverse "$1")
  for sha in $shas; do
    git log -1 --format='%h %s' "$sha"
    commit_msg <(git log -1 --format=%B "$sha")
  done
}

# A release tag names the version in Cargo.toml, CHANGELOG.md has a dated
# section for that version, and the tagged commit is on main.
release() {
  local current
  current=$(version)
  say "release $1"
  if [ "$1" != "v$current" ]; then
    fail "tag $1 does not match version $current in Cargo.toml"
  fi
  if ! grep -Eq "^## \[${current//./\\.}\] - [0-9]{4}-[0-9]{2}-[0-9]{2}$" CHANGELOG.md; then
    fail "CHANGELOG.md has no '## [$current] - YYYY-MM-DD' section"
  fi
  if ! git merge-base --is-ancestor HEAD origin/main; then
    fail "the commit tagged $1 is not on origin/main"
  fi
}

# The body of a GitHub release: the version's CHANGELOG.md section, without
# its heading and without the link definitions at the end of the file.
release_notes() {
  awk -v heading="## [${1#v}]" '
    /^## \[/ { keep = index($0, heading) == 1; next }
    /^\[[^]]+\]: / { keep = 0 }
    keep
  ' CHANGELOG.md
}

all() {
  rules
  lint
  test_all
  each_feature
  docs
  package
  if command -v typos >/dev/null; then spelling; else echo "typos not installed: skipped" >&2; fi
  if command -v actionlint >/dev/null && command -v shellcheck >/dev/null; then
    workflows
  else
    echo "actionlint or shellcheck not installed: skipped" >&2
  fi
  if command -v cargo-deny >/dev/null; then deny; else echo "cargo-deny not installed: skipped" >&2; fi
}

if [ "$#" -eq 0 ]; then
  all
  exit 0
fi
case "$1" in
  rules | lint | docs | site | package | spelling | workflows | deny | coverage) "$1" ;;
  test) test_all ;;
  features) each_feature ;;
  feature) feature "${2:?a feature name}" ;;
  list-features) features ;;
  branch) branch "${2:-}" ;;
  commit-msg) commit_msg "${2:?a commit message file}" ;;
  commits) commits "${2:?a revision range, such as origin/main..HEAD}" ;;
  release) release "${2:?a tag, such as v0.1.0}" ;;
  release-notes) release_notes "${2:?a tag, such as v0.1.0}" ;;
  *)
    echo "unknown step: $1" >&2
    exit 2
    ;;
esac
