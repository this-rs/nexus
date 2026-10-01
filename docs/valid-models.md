# Valid Model Names for Claude Code API

## Overview

Claude Code CLI uses specific model names that may differ from the standard Claude API model names. This document lists the valid model names that can be used with claude-code-api.

## Where the valid names come from

Do not maintain a list here. `claude-code-api` resolves the catalogue at runtime:

| Source | When it is used |
|--------|-----------------|
| `GET https://api.anthropic.com/v1/models` (via `ModelRegistry::refresh`) | `ANTHROPIC_API_KEY` is set and the request succeeds |
| `ClaudeModel::all()` in `claude-code-api/src/models/claude.rs` | no key, or the fetch failed |

Read the live list from the server:

```bash
curl -s http://localhost:8080/v1/models | jq '.data[].id'   # what this server accepts
curl -s -X POST http://localhost:8080/v1/models/refresh      # force a re-fetch
```

`ModelRegistry::is_dynamic` reports which of the two answered, so a surprising list can be
traced to its source instead of guessed at.

### Aliases

- `opus` — newest Opus
- `sonnet` — newest Sonnet
- `haiku` — newest Haiku

> The list that used to sit here named Opus 4.1 as the latest model and was headed
> "(2025)". It was wrong for most of its life. `cargo test --test docs_model_drift` now
> fails if any model id written in the documentation is absent from `ClaudeModel::all()`.

## Invalid Model Names

Anything the resolver cannot map to a catalogue entry is rejected with
404 `not_found_error`. The forms that get tried most often:

- `opus-4.1`, `opus-4`, `sonnet-4` — bare version aliases are not supported; use `opus` / `sonnet` / `haiku`
- `claude-3-opus-20240229` — a retired id, and the dated `-YYYYMMDD` suffix is no longer part of the catalogue

## Recommended Usage

1. Send an alias (`opus`, `sonnet`, `haiku`) unless you have a reason to pin.
2. To pin, take the exact id from `GET /v1/models` on the server you are calling — not
   from a document, including this one.
3. Never hard-code an id in a test fixture: use the alias, or read the catalogue.

## Examples

### Valid request

```bash
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "opus",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

To pin, resolve the id first:

```bash
MODEL=$(curl -s http://localhost:8080/v1/models | jq -r '.data[0].id')
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d "{\"model\": \"$MODEL\", \"messages\": [{\"role\": \"user\", \"content\": \"Hello\"}]}"
```

### Rejected request

```bash
# 404 not_found_error: bare version alias
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "opus-4.1", "messages": [{"role": "user", "content": "Hello"}]}'
```
