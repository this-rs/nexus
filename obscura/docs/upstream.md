# Maintaining the Obscura fork

`obscura/` is a **light fork** of the
[Obscura workspace](https://github.com/h4ckf0r0day/obscura), vendored into the
nexus repository. The crates keep upstream's layout under `crates/`, plus the
vendored `vendor/` (patched taffy and cosmic-text). One crate is new:
`crates/obscura-server`, the `mcp` and `serp` command-line entry points.

## Why vendored in place

- Obscura's crates (`obscura-mcp`, `obscura-browser`, …) are **not published
  on crates.io** (workspace-internal, version 0.1.0).
- They are members of the nexus workspace, but **not default members**: a plain
  `cargo build` / `cargo test` at the repository root never compiles them, so
  the default build stays free of V8. Build them by name
  (`cargo build -p obscura-server`).
- Upstream's `[workspace.dependencies]` were written out in each crate's
  `Cargo.toml` (the nexus workspace has its own, with different versions), and
  upstream's `[patch.crates-io]` entries for taffy and cosmic-text moved to the
  nexus root `Cargo.toml`.

## Local modifications

See `NOTICE`. In short:

- `crates/obscura-mcp/src/lib.rs` — the `browser_web_search` MCP tool and the
  public `serp_search` / `SerpResult` helpers (keyless SERP search); the MCP
  `serverInfo.name` is `obscura`.
- `crates/obscura-server/` — new crate.
- Every `Cargo.toml` — workspace-inherited keys and dependencies written out.

## Merging upstream

There is no git history to merge from: upstream changes are brought in by
diffing an upstream tag against the previous one and applying that diff under
`obscura/`, then re-checking the modifications listed above.

```bash
git clone https://github.com/h4ckf0r0day/obscura /tmp/obscura-upstream
git -C /tmp/obscura-upstream diff <old-tag> <new-tag> -- crates vendor \
  | git apply --directory=obscura
```

When upstream publishes its crates on crates.io, switch to registry versions
and drop the vendored crates.
