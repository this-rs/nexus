#!/usr/bin/env bash
# Prints, comma-separated, every feature of the default workspace members except
# `nexus-tools/obscura`: what CI passes where it used to pass `--all-features`.
#
# `--all-features` would also turn on nexus-tools' `obscura` feature, which compiles the vendored
# Obscura browser and links V8 (a prebuilt static library downloaded at build time). That has its
# own job (`obscura` in .github/workflows/ci.yml, with the archive pinned and checksum-verified);
# every other job stays V8-free. A new feature of a default member is picked up here by itself.
set -euo pipefail
cargo metadata --no-deps --format-version 1 | jq -r '
  .workspace_default_members as $members
  | [ .packages[]
      | select(.id as $id | $members | index($id))
      | .name as $crate
      | .features | keys[]
      | select(. != "default")
      | "\($crate)/\(.)"
      | select(. != "nexus-tools/obscura") ]
  | join(",")'
