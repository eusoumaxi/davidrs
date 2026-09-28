#!/usr/bin/env bash
# Prepare a checked commit for publication. Only release metadata may change.
set -euo pipefail
cd "$(dirname "$0")/.."

version() {
  cargo metadata --no-deps --format-version 1 --locked | jq -r '.packages[0].version'
}

if [[ ${GITHUB_REF_TYPE:?} == tag ]]; then
  tag=${GITHUB_REF_NAME:?}
  scripts/check.sh release "$tag"
else
  [[ ${GITHUB_REF_NAME:?} == main ]] || exit 1
  git fetch origin main --tags
  if [[ $(git rev-parse HEAD) != "$(git rev-parse origin/main)" ]]; then
    echo "A newer main commit will prepare the release."
    exit 0
  fi
  if ! previous=$(git describe --tags --match 'v[0-9]*' --abbrev=0 HEAD 2>/dev/null); then
    echo "Publish and tag the first version manually before automatic releases."
    exit 0
  fi
  if git diff --quiet "$previous" HEAD -- src Cargo.toml Cargo.lock build.rs rust-toolchain.toml; then
    echo "No Rust source or crate configuration changed since $previous."
    exit 0
  fi

  release-plz update
  tag="v$(version)"
  if [[ $tag == "$previous" ]]; then
    echo "The published package is unchanged."
    exit 0
  fi
  if ! git diff --quiet; then
    git switch -c "chore/release-${tag#v}"
    git add Cargo.toml Cargo.lock CHANGELOG.md
    git -c core.hooksPath=.githooks commit -m "chore(release): ${tag#v}"
  fi
  cargo publish --dry-run --locked --all-features
  git push origin HEAD:main
  git fetch origin main
  scripts/check.sh release "$tag"
fi

{
  echo "tag=$tag"
  echo "commit=$(git rev-parse HEAD)"
} >> "${GITHUB_OUTPUT:?}"
