#!/usr/bin/env bash
#
# Refuse a release whose git tag disagrees with the manifests.
#
# The release workflow derives the published version from the pushed tag. On
# its own that means a mistyped tag publishes a crate at a version nobody
# chose, and the mistake is only visible once it is already on crates.io --
# where a version can never be reused. This script is the guard: it is the
# first job of the release workflow, and every later job depends on it.
#
# Usage:
#   script/check-version-consistency.sh            # manifests agree with each other
#   script/check-version-consistency.sh v0.5.0     # ... and with this tag
#
# Runs offline. No network, no cargo, no toolchain -- only grep over the
# manifests, so it gives the same answer on a laptop and in CI.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
status=0

fail() {
    printf 'version mismatch: %s\n' "$1" >&2
    status=1
}

# Read `version = "x.y.z"` from the first matching table of a manifest.
# $1 = manifest path, $2 = table name (e.g. workspace.package)
read_manifest_version() {
    awk -v table="[$2]" '
        /^[[:space:]]*\[/ { in_table = ($0 ~ "^[[:space:]]*\\" table "[[:space:]]*$"); next }
        in_table && /^[[:space:]]*version[[:space:]]*=/ {
            if (match($0, /"[^"]+"/)) {
                print substr($0, RSTART + 1, RLENGTH - 2)
                exit
            }
        }
    ' "$1"
}

workspace_version="$(read_manifest_version "$repo_root/Cargo.toml" workspace.package)"
if [[ -z "$workspace_version" ]]; then
    printf 'cannot read version from [workspace.package] in Cargo.toml\n' >&2
    exit 1
fi
printf 'workspace version: %s\n' "$workspace_version"

# Any member that hard-codes its version is a second source of truth. It is
# allowed only while it agrees; disagreement is a hard stop.
for manifest in "$repo_root"/claude-code-api/Cargo.toml "$repo_root"/claude-code-sdk-rs/Cargo.toml; do
    member_version="$(read_manifest_version "$manifest" package)"
    [[ -z "$member_version" ]] && continue  # inherits version.workspace = true
    rel="${manifest#"$repo_root"/}"
    if [[ "$member_version" != "$workspace_version" ]]; then
        fail "$rel declares $member_version, workspace declares $workspace_version"
    else
        printf 'note: %s hard-codes %s instead of inheriting the workspace version\n' \
            "$rel" "$member_version"
    fi
done

# Compare against the tag when one is supplied.
if [[ $# -gt 0 && -n "${1:-}" ]]; then
    tag="$1"
    tag_version="${tag#refs/tags/}"
    tag_version="${tag_version#v}"
    printf 'tag version: %s\n' "$tag_version"
    if [[ "$tag_version" != "$workspace_version" ]]; then
        fail "tag $tag resolves to $tag_version, workspace declares $workspace_version"
    fi
fi

if [[ $status -ne 0 ]]; then
    printf '\nrefusing to continue: publish the tag that matches the manifests, or fix the manifests first.\n' >&2
    exit 1
fi

printf 'versions are consistent\n'
