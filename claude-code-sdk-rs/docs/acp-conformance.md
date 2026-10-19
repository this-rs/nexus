# ACP conformance matrix

The gate's answer to "which ACP agents does this SDK guarantee, and how is each
claim established?". The table is generated from `tests/support/acp_matrix.rs`
(the single source of truth, a data table) and is kept byte-for-byte identical
to it by `the_conformance_matrix_matches_the_doc`. A future "which agents do we
guarantee" query is answered by reading `ACP_AGENTS` there, never by scraping
this file.

**No live agent was ever run.** Every `Verified` claim is established offline:
`fake_acp` replays JSONL transcripts written from the public Agent Client
Protocol specification (version 1). `opencode`, a Gemini CLI and openclaw were
never executed; their rows below record what was read, and what was not.

## How to add or re-verify an agent

1. Add an `AcpAgentEntry` to `ACP_AGENTS` in `tests/support/acp_matrix.rs`, with
   `status: Verification::NotVerified` until a run actually proves it offline.
2. If that run establishes a new behaviour, add its name to `ACP_BEHAVIOURS` and
   wire it into the proof — `every_matrix_behaviour_is_a_known_behaviour` and
   `the_matrix_says_fake_acp_is_verified_for_exactly_what_the_suite_proves` fail
   otherwise.
3. Regenerate this file:

   ```text
   UPDATE_ACP_MATRIX=1 cargo test -p nexus-claude --features provider-acp --test acp_conformance_matrix
   ```

The items the gate does **not** prove stay in
`tests/transcripts/acp/1/schema/PROVENANCE.md` (the "NOT VERIFIED" table): the
`usage` shape of a `session/prompt` result, `usage_update` fields, `messageId`,
`rawOutput`, `availableCommands`, `session/load` result modes, the `-32000`
auth-refusal answer, the agent-published mode ids, opencode's `<server>_<tool>`
spelling, and the launch arguments of real agents.

## What this gate does not prove

A green gate is **not** a full conformance proof. The `Verified` claim is
derived from the contract's own constants — `wire::MESSAGE_KINDS` and
`Capabilities::FIELDS` — plus hand-written lists (`ACP_BEHAVIOURS`,
`FAKE_ACP_BARE`, `FAKE_ACP_FALLBACK`, `FAKE_ACP_EXTRA`). The gate catches drift
and classification errors between those lists and the suite, but it does **not**
prove that every `MESSAGE_KINDS` name has an actual assertion behind it in
`acp_conformance.rs`; a name wired into the claim without a real runtime
assertion still passes.
<!-- ACP-MATRIX:START -->
| agent | status | evidence | verified behaviours |
| --- | --- | --- | --- |
| `fake_acp` fake_acp (protocol v1) | Verified | offline: AcpProvider on fake_acp replaying spec-derived JSONL transcripts | initialize.params, initialize.result, session/new.params, session/new.result, session/load.params, session/load.result, session/prompt.params, session/prompt.result, session/cancel.params, session/set_mode.params, session/update:agent_message_chunk, session/update:agent_thought_chunk, session/update:tool_call, session/update:tool_call_update, session/update:plan, session/update:available_commands_update, session/update:current_mode_update, session/update:usage_update, session/update:user_message_chunk, session/request_permission.params, session/request_permission.response, resume, thinking, tools, per_session_mcp, interactive_permissions, permission_scopes, cost, context_window, secret_isolation, images_fallback, hooks_fallback, subagents_fallback, compaction_signal_fallback, compaction_manual_fallback, compaction_model_fallback, background_tasks_fallback, tool_cancel_fallback, native_question_fallback, set_model_live_fallback, sandbox_fallback, mcp_servers_refused_retry, system_prompt_as_prompt_prefix |
| `opencode` opencode acp | Not verified | never run; <server>_<tool> MCP title spelling read from the provider review | — |
| `gemini_cli` Gemini CLI (ACP) | Not verified | never run | — |
| `openclaw` openclaw acp (2026.9.x) | Not verified | published code read, not run | mcp_servers_refused_retry |
<!-- ACP-MATRIX:END -->
