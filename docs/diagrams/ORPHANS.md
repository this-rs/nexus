# Source files with no owning diagram (nexus)

The charter asks that no source file be left without a diagram that owns it.
This is the list of the ones that are, so the gap is a number somebody can act
on rather than a vague sense that the documentation is thin.

**74 of 77 `src/` files have no owner.** Of the three diagrams this repository
has so far, two (`release-readiness`, `po-bugs`) cover configuration,
documentation and their own gates; `nexus-model-catalogue` is the first to own
library code, claiming three files of the model registry. That is the honest starting
point, and publishing it is the point: a diagram per domain is task 0.2's work,
and it cannot be planned against a gap nobody has measured.

## Ratchet

`claude-code-api/tests/diagram_index.rs` asserts the real count never exceeds
the ceiling recorded below. The number can go down, never up — so a pull
request that adds an unowned source file fails until either a diagram claims it
or the ceiling is lowered in the same change.

<!-- orphan-ceiling: 74 -->

Lower the ceiling in the same pull request that adds the `covers` globs. Never
raise it: the way out is a diagram, not a bigger number.

Only a `status: verified` entry owns anything. A `planned` entry names a
diagram somebody intends to write — counting its globs would let this ceiling
fall without a single diagram existing, which is the index buying credit for
intentions. The gate enforces that distinction
(`owning_globs`), and it was a real bug here before a fixture test caught it.

## What is excluded, and why

- `examples/**` and `tests/**` are not counted. An example exists to be read
  and a test to be executed; neither is logic a diagram would describe. They
  are still compiled, so they cannot rot silently.
- Only `.rs` files are counted. Manifests, workflows and documentation are
  owned through the `covers` globs of the diagrams that discuss them.

## Suggested grouping

Not a decision — a starting point for whoever writes these diagrams. The
directory structure already suggests the domains, and the charter's naming
would give `po-<domain>` for cartography of existing code.

| Likely diagram | Directories |
|---|---|
| `po-nexus-sdk` | `claude-code-sdk-rs/src/`, `src/transport/` — already a planned entry in the main repository's index |
| `po-nexus-sdk-memory` | `claude-code-sdk-rs/src/memory/` |
| `po-nexus-api-core` | `claude-code-api/src/core/`, `core/hooks/` |
| `po-nexus-api-storage` | `claude-code-api/src/core/storage/`, `core/memory/` |
| `po-nexus-api-http` | `claude-code-api/src/api/`, `middleware/`, `models/` |

## The list

Regenerate with:

```
cargo test -p claude-code-api --test diagram_index -- --nocapture \
  the_orphan_count_never_grows
```

### claude-code-api/src/  (1)
  main.rs

### claude-code-api/src/api/  (7)
  chat.rs, conversations.rs, mod.rs, projects.rs, sessions.rs, stats.rs, streaming_handler.rs

### claude-code-api/src/bin/  (1)
  ccapi.rs

### claude-code-api/src/core/  (12)
  auth.rs, cache.rs, claude_manager.rs, config.rs, conversation.rs, interactive_session.rs, mod.rs, objective_tracker.rs, process_pool.rs, retry.rs, session_manager.rs, session_process.rs

### claude-code-api/src/core/hooks/  (3)
  mod.rs, neo4j_hook_callback.rs, neo4j_permission_provider.rs

### claude-code-api/src/core/memory/  (6)
  long_term.rs, medium_term.rs, mod.rs, short_term.rs, traits.rs, unified.rs

### claude-code-api/src/core/storage/  (7)
  combined.rs, meilisearch.rs, memory.rs, mod.rs, neo4j.rs, tiered_cache.rs, traits.rs

### claude-code-api/src/middleware/  (3)
  error_handler.rs, mod.rs, request_id.rs

### claude-code-api/src/models/  (4)
  error.rs, mod.rs, openai.rs, tests.rs

### claude-code-api/src/utils/  (5)
  function_calling.rs, mod.rs, parser.rs, streaming.rs, text_chunker.rs

### claude-code-sdk-rs/src/  (15)
  cli_download.rs, client.rs, client_working.rs, errors.rs, interactive.rs, internal_query.rs, lib.rs, message_parser.rs, model_recommendation.rs, optimized_client.rs, perf_utils.rs, query.rs, sdk_mcp.rs, token_tracker.rs, types.rs

### claude-code-sdk-rs/src/bin/  (1)
  test_interactive.rs

### claude-code-sdk-rs/src/memory/  (6)
  integration.rs, message_document.rs, mod.rs, provider.rs, scoring.rs, tool_context.rs

### claude-code-sdk-rs/src/transport/  (3)
  mock.rs, mod.rs, subprocess.rs
