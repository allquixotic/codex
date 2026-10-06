#!/usr/bin/env bash
# Upload completed GUI packages, verify GitHub's asset digests, then clean
# local Cargo build outputs. This script never builds or creates a release.
# Usage: publish-release.sh OWNER/REPO TAG PACKAGE [PACKAGE ...]
set -euo pipefail
if (( $# < 3 )); then
  echo "Usage: $0 OWNER/REPO TAG PACKAGE [PACKAGE ...]" >&2
  exit 2
fi
repo="$1"
tag="$2"
shift 2
for package in "$@"; do
  [[ -f "$package" ]] || { echo "Missing package: $package" >&2; exit 1; }
done
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gh release upload "$tag" "$@" --repo "$repo"
for package in "$@"; do
  name="$(basename "$package")"
  local_digest="sha256:$(shasum -a 256 "$package" | cut -d ' ' -f 1)"
  remote_digest="$(gh api "repos/${repo}/releases/tags/${tag}" --jq '.assets[] | [.name, .digest] | @tsv' | awk -F '\t' -v name="$name" '$1 == name { print $2 }')"
  [[ "$remote_digest" == "$local_digest" ]] || {
    echo "GitHub digest missing or different for ${name}; keeping local artifacts." >&2
    exit 1
  }
done
# Every package is verified before any local build output is removed.
cargo clean --manifest-path "$here/../../Cargo.toml"
# Delete only the exact published packages, not other files in dist/.
for package in "$@"; do
  rm -- "$package"
done
