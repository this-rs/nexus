# Obscura (vendored)

A fork of [Obscura](https://github.com/h4ckf0r0day/obscura), the open-source
Rust headless browser (DOM in Rust, JavaScript through V8 via `deno_core`),
vendored into nexus so that nexus-tools can ship a keyless web search and a
browser without an external program.

Apache-2.0: see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE) (upstream
attribution and the local modifications). The rest of the nexus repository is
MIT; nothing under `obscura/` is relicensed.

## Not in the default build

The crates are members of the nexus workspace but **not default members**, so
`cargo build`, `cargo test` and CI at the repository root never compile V8.
Build them by name:

```bash
cargo build --release -p obscura-server
```

The first build downloads V8's prebuilt static library (rusty_v8) and is slow;
later builds reuse it.

## `obscura-server`

```
obscura-server [--proxy URL] [--user-agent UA] [--stealth] <command>

Commands:
  mcp    MCP server over stdio (JSON-RPC 2.0, newline-delimited): the browser_* tools
  serp   One-shot keyless web search; prints a JSON array of {url, title, snippet}
         serp <query> [--max-results N] [--engine duckduckgo]
```

`serp` is what nexus-tools' external search engine `obscura:<path-to-obscura-server>`
runs.

## Layout

- `crates/` — upstream crates (`obscura`, `obscura-browser`, `obscura-cdp`,
  `obscura-cli`, `obscura-dom`, `obscura-js`, `obscura-mcp`, `obscura-net`,
  `obscura-render`) and `obscura-server`.
- `vendor/` — taffy and cosmic-text with upstream Obscura's patches, applied
  through `[patch.crates-io]` in the nexus root `Cargo.toml`.
- `docs/tools.md` — the MCP tool catalog; `docs/upstream.md` — how the fork is
  maintained. `SECURITY.md` — security notes (SSRF protection, untrusted JS).

`crates/obscura-render/tests/layout_test.rs` reads upstream's `render-repros/`
fixtures, which the fork never carried: it builds only with the
`upstream-render-repros` feature, which nothing enables.

## In nexus-tools (`obscura` feature)

`cargo build -p nexus-tools --features tls,obscura` embeds Obscura in nexus-tools
(decision 1e9d7501): `--search-engine obscura` (no path) is the keyless search
run in process, and `nexus-tools obscura mcp` serves the `browser_*` tools — no
Obscura to install next to it. `nexus-tools --capabilities` says whether a build
embeds it. Only `obscura-mcp` and what it needs are linked: not `obscura-render`
(screenshots, PDF) nor `obscura-cdp` / `obscura-cli`.

## V8

`obscura-js` runs JavaScript through `deno_core`, which links V8 through
rusty_v8 (crate `v8`). rusty_v8 does not compile V8: its build script downloads
a **prebuilt static library** from the rusty_v8 GitHub release, and checks
nothing about it (its `.sum` file records the URL, not a hash).

How nexus pins it:

- **Versions**: `deno_core = "=0.350.0"` and `v8 = "=137.3.0"` in
  `crates/obscura-js/Cargo.toml` (exact pins: nexus does not commit Cargo.lock).
- **Checksum**: `scripts/rusty-v8-cache.sh [TARGET...]` downloads the archive for
  each target, checks its SHA-256 against the table in the script (the release's
  own asset digests, each confirmed by a download), and puts it in rusty_v8's
  download cache (`$CARGO_HOME/.rusty_v8/<escaped URL>`), which the build script
  reads before it would download. The script refuses a V8 version that differs
  from the `v8` pin. CI's `obscura` job and the release build run it first.
- **Explicit `--target`**: `obscura-js` builds a V8 snapshot at build time, so a
  host copy of `v8` is compiled too (seed the host as well when cross-compiling).
  Without `--target` both copies share `target/<profile>/gn_out/` and their build
  scripts can overwrite each other's library mid-link.

| Target | Prebuilt V8 137.3.0 | Status |
|---|---|---|
| aarch64-apple-darwin | yes, pinned | built and tested (macOS arm64, by hand) |
| x86_64-apple-darwin | yes, pinned | expected to build; not built here |
| x86_64-unknown-linux-gnu | yes, pinned | built and tested in CI (`obscura` job) |
| aarch64-unknown-linux-gnu | yes, pinned | expected to build natively or cross with the host V8 seeded; not built here |
| x86_64-pc-windows-msvc | yes, pinned | expected to build; not built here (Obscura upstream's Windows support is untested by nexus) |
| *-linux-musl, aarch64-pc-windows-msvc, others | **no prebuilt** | cannot build with the feature (V8 would have to be built from source) |

The license texts of what that library contains (V8, ICU, abseil, libc++…) are in
`licenses/v8/`, taken verbatim from the commits rusty_v8 v137.3.0 pins (see its `NOTICE`): a
release that ships nexus-tools with the feature ships them too.
