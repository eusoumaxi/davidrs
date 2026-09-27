#!/usr/bin/env bash
# Every check CI runs, runnable locally.
#
#   scripts/check.sh              everything, in order
#   scripts/check.sh <step> ...   one step: rules lint test features feature docs site deny coverage
#
# CI calls the same steps, so a green local run is a green CI run.
set -euo pipefail
cd "$(dirname "$0")/.."

# Line coverage (percent) the whole crate must keep.
COVERAGE_FLOOR="${COVERAGE_FLOOR:-98}"

say() { printf '\n==> %s\n' "$*"; }

# Every public feature, from Cargo.toml.
features() {
  cargo metadata --no-deps --format-version 1 --locked \
    | jq -r '.packages[0].features | keys[] | select(startswith("_") | not) | select(. != "default")'
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

deny() {
  say "dependency policy"
  cargo deny --all-features --locked check
}

coverage() {
  say "coverage (floor ${COVERAGE_FLOOR}% of lines)"
  cargo llvm-cov --locked --all-features --summary-only --fail-under-lines "$COVERAGE_FLOOR"
}

all() {
  rules
  lint
  test_all
  each_feature
  docs
  package
  if command -v cargo-deny >/dev/null; then deny; else echo "cargo-deny not installed: skipped" >&2; fi
}

if [ "$#" -eq 0 ]; then
  all
  exit 0
fi
case "$1" in
  rules | lint | docs | site | package | deny | coverage) "$1" ;;
  test) test_all ;;
  features) each_feature ;;
  feature) feature "$2" ;;
  list-features) features ;;
  *) echo "unknown step: $1" >&2; exit 2 ;;
esac
