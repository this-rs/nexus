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
fixtures, which the fork never carried: that test target does not build here.
