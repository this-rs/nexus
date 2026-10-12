#!/usr/bin/env bash
# Puts the prebuilt V8 static library of rusty_v8 (crate `v8`) for each TARGET in rusty_v8's own
# download cache, after checking its SHA-256 against the table below:
#
#   scripts/rusty-v8-cache.sh [TARGET...]      # default: the host
#   cargo build --target TARGET -p nexus-tools --features obscura ...
#
# Why: the `v8` build script downloads this archive from GitHub at build time and checks nothing
# (its ".sum" file holds the URL, not a hash). Before downloading, it looks for
# `$CARGO_HOME/.rusty_v8/<the URL, every non-alphanumeric character as _>` and copies that file
# instead: seeded here, so what links into nexus-tools is the archive whose hash is pinned in this
# file, and no download happens at build time. The version is pinned twice and checked:
# `v8 = "=X"` in obscura/crates/obscura-js/Cargo.toml (Cargo.lock is not committed) and
# V8_VERSION below.
#
# Seed the HOST too when cross-compiling: obscura-js builds its V8 snapshot at build time, so a
# host copy of V8 is linked into a build script. And build with an explicit `--target`: without
# it the host and target copies of `v8` share target/<profile>/gn_out and their build scripts can
# overwrite each other's library mid-link (seen once on a plain `cargo test`).
#
# The hashes are the GitHub release's asset digests for v137.3.0 (published 2025-08-14), each
# confirmed against a download. To move to another V8: bump both pins, then take the new digests
# from `gh api repos/denoland/rusty_v8/releases/tags/vX --jq '.assets[]|"\(.name) \(.digest)"'`.
set -euo pipefail

V8_VERSION=137.3.0

here="$(cd "$(dirname "$0")/.." && pwd)"
pinned="$(sed -n 's/^v8 = "=\([0-9.]*\)".*/\1/p' "$here/obscura/crates/obscura-js/Cargo.toml")"
if [ "$pinned" != "$V8_VERSION" ]; then
  echo "rusty-v8-cache: obscura-js pins v8 =$pinned but this script pins $V8_VERSION; update both" >&2
  exit 1
fi

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

cache="${CARGO_HOME:-$HOME/.cargo}/.rusty_v8"
mkdir -p "$cache"

seed() {
  local target="$1" sha name
  case "$target" in
    aarch64-apple-darwin)
      sha=60503d6724e552c46b01ec260a15e79e86c4715b7197c5c62748911bf1f4e070 ;;
    x86_64-apple-darwin)
      sha=66716c0a7d950ea2c72aaaf6a0c1a402a3bac55fdfc0b4341f49b38e9afecd75 ;;
    aarch64-unknown-linux-gnu)
      sha=e368d0cb41c179c43a990e4ec4a55e44dd9762f1d34be15696acc4433f8a6c92 ;;
    x86_64-unknown-linux-gnu)
      sha=a2681fde53018abd3366018f132617dd598002df78f156c7bc6d2ff608b56567 ;;
    x86_64-pc-windows-msvc)
      sha=15ada191cf2c4c589336d44e6f72c3b66a750de53c0f89e64ec6d10c65ad0e6c ;;
    *)
      echo "rusty-v8-cache: no prebuilt V8 is pinned for $target (pinned: aarch64/x86_64-apple-darwin, aarch64/x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc)" >&2
      exit 1 ;;
  esac
  case "$target" in
    *-windows-*) name="rusty_v8_release_${target}.lib.gz" ;;
    *) name="librusty_v8_release_${target}.a.gz" ;;
  esac
  local url="https://github.com/denoland/rusty_v8/releases/download/v${V8_VERSION}/${name}"
  # rusty_v8's cache key: the URL with every non-alphanumeric character replaced by `_`.
  local file="$cache/$(printf '%s' "$url" | LC_ALL=C sed 's/[^A-Za-z0-9]/_/g')"
  if [ -f "$file" ] && [ "$(sha256_of "$file")" = "$sha" ]; then
    echo "rusty-v8-cache: $target ok ($file)" >&2
    return
  fi
  rm -f "$file" "$file.part"
  curl -fsSL --retry 3 -o "$file.part" "$url"
  local actual
  actual="$(sha256_of "$file.part")"
  if [ "$actual" != "$sha" ]; then
    rm -f "$file.part"
    echo "rusty-v8-cache: SHA-256 mismatch for $name: expected $sha, got $actual" >&2
    exit 1
  fi
  mv "$file.part" "$file"
  echo "rusty-v8-cache: $target verified ($file)" >&2
}

if [ $# -eq 0 ]; then
  set -- "$(rustc -vV | sed -n 's/^host: //p')"
fi
for target in "$@"; do
  seed "$target"
done
