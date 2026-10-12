# MCP tool catalog

The `mcp` command exposes the Obscura browser tools under the `obscura`
serverName (agent-facing names: `mcp__obscura__<tool>`), plus the fork addition
`browser_web_search`.

All tools except `browser_web_search` are vendored verbatim from
[Obscura's obscura-mcp](https://github.com/h4ckf0r0day/obscura). `browser_web_search`
is a local modification of this fork (see `NOTICE`).

## Navigation

| Tool | Purpose |
|---|---|
| `browser_navigate` | Go to a URL; waitUntil: load / domcontentloaded / networkidle0 |
| `browser_back` / `browser_forward` / `browser_reload` | History and reload |
| `browser_close` | Close the active tab (or all tabs) |

## Reading the page

| Tool | Purpose |
|---|---|
| `browser_snapshot` | URL, title, body text (capped ~4000 chars) + interactive-refs summary |
| `browser_markdown` | Page rendered to Markdown |
| `browser_links` | Extract links (optionally same-origin only) |
| `browser_interactive_elements` | Interactables with stable refs (`e1`, `e2`…) |
| `browser_extract` | Extract by CSS selector |
| `browser_get_attribute` / `browser_count` | Attribute lookup / element count |
| `browser_evaluate` | Run JS in the page |
| `browser_search` | Find text in the current page (NOT web search) |
| `browser_network_requests` / `browser_console_messages` | DevTools streams |

## Interaction

| Tool | Purpose |
|---|---|
| `browser_click` / `browser_fill` / `browser_type` | Act on an element by ref or selector |
| `browser_press_key` | Key presses (Enter, Tab…) |
| `browser_select_option` | `<select>` option |
| `browser_scroll` | Scroll the viewport |
| `browser_wait_for` / `browser_wait_for_text` | Wait for a condition/text |
| `browser_detect_forms` / `browser_fill_form` | Form discovery + bulk fill |

## Tabs & state

| Tool | Purpose |
|---|---|
| `browser_tab_new` / `browser_tab_list` / `browser_tab_switch` / `browser_tab_close` | Multi-tab management |
| `browser_get_cookies` / `browser_set_cookie` / `browser_clear_cookies` | Cookies |
| `browser_storage_state` / `browser_set_storage_state` | Export/restore auth session |

## Media (render build only)

| Tool | Purpose |
|---|---|
| `browser_screenshot` | PNG of the viewport |
| `browser_pdf` | PDF export |

## Web search (fork addition)

| Tool | Purpose |
|---|---|
| `browser_web_search` | **Keyless** web search; navigates the active tab to a SERP and extracts `{url, title, snippet}`. Engine: `duckduckgo` (default). |

```json
{
  "name": "browser_web_search",
  "arguments": { "query": "rust headless browser", "max_results": 5 }
}
```
