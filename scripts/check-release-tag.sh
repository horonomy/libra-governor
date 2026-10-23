#!/bin/sh
# Refuse to publish a release whose git tag disagrees with the version the
# workspace actually builds (HORO-2162).
#
# Usage: check-release-tag.sh <tag> [Cargo.toml]
#
# The tag must be exactly `v<workspace.package.version>`. Pre-release suffixes
# are not special-cased: `v0.0.4-rc.1` is refused until the workspace version
# says so, because the shipped `--version` output comes from Cargo.toml.
set -eu

tag="${1:-}"
manifest="${2:-$(dirname "$0")/../Cargo.toml}"

if [ -z "$tag" ]; then
  echo "usage: check-release-tag.sh <tag> [Cargo.toml]" >&2
  exit 2
fi

# `version` of the [workspace.package] table only; other tables also have
# `version = ...` keys (dependencies).
version=$(awk '
  /^\[/ { in_table = ($0 == "[workspace.package]") ; next }
  in_table && /^version[ \t]*=/ {
    gsub(/^version[ \t]*=[ \t]*"/, ""); gsub(/".*$/, ""); print; exit
  }' "$manifest")

if [ -z "$version" ]; then
  echo "check-release-tag: no [workspace.package] version in $manifest" >&2
  exit 2
fi

if [ "$tag" != "v$version" ]; then
  echo "check-release-tag: tag '$tag' does not match workspace version '$version' (expected 'v$version')" >&2
  exit 1
fi

echo "check-release-tag: tag $tag matches workspace version $version"
