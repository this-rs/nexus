//! Behaviour tests for [`claude_code_api::core::hooks::Neo4jHookCallback`].
//!
//! The callback is the gateway's audit trail: every tool call, every user prompt
//! and every session stop is supposed to land in Neo4j as a node, and tool calls
//! additionally in Meilisearch. Nothing in the crate constructs it yet (hence the
//! `#![allow(dead_code)]` at the top of the module), so these tests are the first
//! thing to exercise it, and the interesting paths are the ones where a write is
//! lost: a graph that refuses the statement, a duration that was never measured,
//! an output too large to store, and a hook event the `match` drops.
//!
//! Every assertion goes through the production `HookCallback::execute`, against
//! the fake Bolt server in [`hook_bolt_fake`], which records the Cypher and the
//! parameter map the callback actually sends. The assertions that need a live
//! `tracing` subscriber live in `neo4j_hook_callback_logs.rs` instead — a global
//! subscriber has to own its process.

mod hook_bolt_fake;

use claude_code_api::core::hooks::{Neo4jHookCallback, Neo4jHookCallbackConfig};
use hook_bolt_fake::{
    FakeBolt, SESSION, assert_transparent, callback, fire, post_tool_use, pre_tool_use, stop,
    support, user_prompt,
};
use nexus_claude::{HookInput, PreCompactHookInput, SubagentStopHookInput};
use serde_json::{Value, json};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// init_schema
// ---------------------------------------------------------------------------

#[tokio::test]
async fn init_schema_sends_three_uniqueness_constraints_and_the_tool_name_index() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    cb.init_schema().await.expect("schema");

    let cypher = bolt.cypher();
    assert_eq!(
        cypher.len(),
        4,
        "three constraints and one index: {cypher:?}"
    );
    assert!(cypher[0].contains("CONSTRAINT nexus_tool_usage_id"));
    assert!(cypher[0].contains("t.id IS UNIQUE"));
    assert!(cypher[1].contains("CONSTRAINT nexus_user_prompt_id"));
    assert!(cypher[1].contains("p.id IS UNIQUE"));
    assert!(cypher[2].contains("CONSTRAINT nexus_session_event_id"));
    assert!(cypher[2].contains("e.id IS UNIQUE"));
    assert!(cypher[3].contains("INDEX nexus_tool_usage_name"));
    assert!(cypher[3].contains("ON (t.tool_name)"));
    // All four are `IF NOT EXISTS`, so a second call must stay a no-op.
    assert!(
        cypher.iter().all(|c| c.contains("IF NOT EXISTS")),
        "{cypher:?}"
    );
    // The constraints are exactly the three the module doc-comment advertises.
    assert!(cypher.iter().all(|c| c.contains("Nexus")), "{cypher:?}");
}

/// `init_schema` swallows **every** statement error with a `debug!` and then
/// returns `Ok(())`, so its `Result` has no reachable `Err`: a caller that
/// bootstraps the audit trail cannot tell a working Neo4j from one that rejected
/// all four statements and will go on writing nodes with no uniqueness guarantee.
#[tokio::test]
async fn init_schema_reports_success_even_though_every_statement_failed() {
    let bolt = FakeBolt::start().await;
    bolt.failing("constraints are not supported here");
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    let result = cb.init_schema().await;

    assert!(
        result.is_ok(),
        "init_schema hides schema failures; see the report"
    );
    assert_eq!(bolt.runs().len(), 4, "it still tried all four statements");
}

// ---------------------------------------------------------------------------
// init_meilisearch_index
// ---------------------------------------------------------------------------

/// `init_meilisearch_index` is a no-op whose body is a comment and a `debug!`.
/// It talks to neither Neo4j nor Meilisearch, so the index named by
/// `INDEX_TOOL_USAGE` does not exist after a successful call. Asserted against a
/// `wiremock` Meilisearch that records every request it receives.
#[tokio::test]
async fn init_meilisearch_index_creates_no_index_at_all() {
    let bolt = FakeBolt::start().await;
    let meili = support::http_mocks::meilisearch(Vec::new(), Vec::new()).await;
    let client = support::http_mocks::meilisearch_client(&meili)
        .await
        .expect("a client over the mock");
    let before = meili.received_requests().await.unwrap_or_default().len();

    let cb = Neo4jHookCallback::new(
        bolt.graph().await,
        Some(Arc::new(client)),
        Neo4jHookCallbackConfig::default(),
    );
    cb.init_meilisearch_index()
        .await
        .expect("the no-op cannot fail");

    let after = meili.received_requests().await.unwrap_or_default().len();
    assert_eq!(
        after, before,
        "init_meilisearch_index sent no request; it only logs. See the report."
    );
    assert!(bolt.runs().is_empty(), "and nothing went to Neo4j either");
}

#[tokio::test]
async fn init_meilisearch_index_is_also_ok_without_a_meilisearch_client() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    cb.init_meilisearch_index()
        .await
        .expect("no client is not an error");

    assert!(bolt.runs().is_empty());
}

// ---------------------------------------------------------------------------
// PreToolUse
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pre_tool_use_writes_nothing_and_stays_out_of_the_way() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    let output = fire(
        &cb,
        &pre_tool_use("Bash", json!({"command": "ls"})),
        Some("t-1"),
    )
    .await;

    assert_transparent(&output);
    assert!(
        bolt.runs().is_empty(),
        "PreToolUse only memorises a start time: {:?}",
        bolt.cypher()
    );
}

/// `log_pre_tool_use` is the only thing the flag changes: the event is still not
/// persisted, and the output is still the transparent default.
#[tokio::test]
async fn pre_tool_use_with_verbose_logging_still_persists_nothing() {
    let bolt = FakeBolt::start().await;
    let cb = callback(
        bolt.graph().await,
        Neo4jHookCallbackConfig {
            log_pre_tool_use: true,
            ..Default::default()
        },
    );

    let output = fire(&cb, &pre_tool_use("Read", json!({"file": "a.txt"})), None).await;

    assert_transparent(&output);
    assert!(bolt.runs().is_empty());
}

// ---------------------------------------------------------------------------
// PostToolUse — the node it writes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_tool_use_writes_a_tool_usage_node_with_the_serialised_input_and_output() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    let output = fire(
        &cb,
        &post_tool_use(
            "Read",
            json!({"file_path": "a.txt"}),
            json!({"content": "Hello"}),
        ),
        None,
    )
    .await;

    assert_transparent(&output);
    let run = bolt.run_matching("CREATE (t:NexusToolUsage");
    assert!(run.cypher.contains("created_at: datetime($now)"));
    assert!(
        run.cypher.contains("CREATE (c)-[:HAS_TOOL_USAGE]->(t)"),
        "it tries to attach the usage to a conversation: {}",
        run.cypher
    );
    assert_eq!(run.params["tool_name"], json!("Read"));
    assert_eq!(run.params["session_id"], json!(SESSION));
    assert_eq!(run.params["input"], json!(r#"{"file_path":"a.txt"}"#));
    assert_eq!(run.params["output"], json!(r#"{"content":"Hello"}"#));
    // `id` is a fresh UUID per event, not the tool_use_id.
    let id = run.params["id"].as_str().expect("an id parameter");
    assert_eq!(id.len(), 36, "a hyphenated UUID, got {id:?}");
    // `now` is handed to Cypher's `datetime()`, so it must be RFC 3339.
    let now = run.params["now"].as_str().expect("a now parameter");
    chrono::DateTime::parse_from_rfc3339(now).expect("now is RFC 3339");
}

/// A `PreToolUse` with the same `tool_use_id` is what makes the duration
/// measurable; without one, there is nothing to subtract from.
#[tokio::test]
async fn post_tool_use_times_the_call_when_pre_tool_use_saw_the_same_tool_use_id() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &pre_tool_use("Bash", json!({})), Some("t-1")).await;
    fire(
        &cb,
        &post_tool_use("Bash", json!({}), json!("ok")),
        Some("t-1"),
    )
    .await;

    let duration = bolt.last_params()["duration_ms"].clone();
    assert!(
        duration.as_i64().is_some_and(|ms| ms >= 0),
        "a measured duration is a non-negative integer, got {duration}"
    );
}

/// Regression test for the sentinel: `duration_ms` used to be written as `-1`
/// when no `PreToolUse` had been seen, a value indistinguishable from a
/// measurement and, worse, one that `handle_stop`'s
/// `sum(COALESCE(t.duration_ms, 0))` happily adds to the session total. The
/// module's own schema documents the property as nullable (`duration_ms: Int?`).
/// Against the previous code this asserts `-1` instead of `null`.
#[tokio::test]
async fn post_tool_use_writes_a_null_duration_when_the_call_was_never_timed() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &post_tool_use("Bash", json!({}), json!("ok")), None).await;

    assert_eq!(
        bolt.last_params()["duration_ms"],
        Value::Null,
        "an unmeasured duration must be null, not a sentinel"
    );
}

/// A `tool_use_id` that no `PreToolUse` registered is the same situation: the
/// lookup misses and the duration stays unknown.
#[tokio::test]
async fn post_tool_use_with_an_unknown_tool_use_id_is_untimed() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &pre_tool_use("Bash", json!({})), Some("t-1")).await;
    fire(
        &cb,
        &post_tool_use("Bash", json!({}), json!("ok")),
        Some("t-other"),
    )
    .await;

    assert_eq!(bolt.last_params()["duration_ms"], Value::Null);
}

/// The start time is `remove`d, not read: a duplicated `PostToolUse` for one
/// `PreToolUse` is timed once and then untimed, rather than reporting the wall
/// clock since the first call.
#[tokio::test]
async fn a_replayed_post_tool_use_consumes_the_start_time_and_becomes_untimed() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &pre_tool_use("Bash", json!({})), Some("t-1")).await;
    let event = post_tool_use("Bash", json!({}), json!("ok"));
    fire(&cb, &event, Some("t-1")).await;
    assert!(bolt.last_params()["duration_ms"].as_i64().is_some());

    fire(&cb, &event, Some("t-1")).await;
    assert_eq!(
        bolt.last_params()["duration_ms"],
        Value::Null,
        "the start time is consumed by the first PostToolUse"
    );
    assert_eq!(bolt.runs().len(), 2, "both events were still persisted");
}

// ---------------------------------------------------------------------------
// PostToolUse — truncation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_tool_use_truncates_an_output_over_the_configured_budget() {
    let bolt = FakeBolt::start().await;
    let cb = callback(
        bolt.graph().await,
        Neo4jHookCallbackConfig {
            max_output_size: 12,
            ..Default::default()
        },
    );

    // `"aaaa…"` with quotes is 22 bytes of JSON, so the first 12 are kept.
    fire(
        &cb,
        &post_tool_use("Bash", json!({}), json!("aaaaaaaaaaaaaaaaaaaa")),
        None,
    )
    .await;

    let params = bolt.last_params();
    assert_eq!(params["output"], json!("\"aaaaaaaaaaa...[truncated]"));
    assert_eq!(
        params["input"],
        json!("{}"),
        "the input is stored whole, whatever its size"
    );
}

#[tokio::test]
async fn post_tool_use_stores_an_output_of_exactly_the_budget_untouched() {
    let bolt = FakeBolt::start().await;
    let cb = callback(
        bolt.graph().await,
        Neo4jHookCallbackConfig {
            max_output_size: 6,
            ..Default::default()
        },
    );

    // `"abcd"` is exactly 6 bytes: the comparison is `>`, not `>=`.
    fire(&cb, &post_tool_use("Bash", json!({}), json!("abcd")), None).await;

    assert_eq!(bolt.last_params()["output"], json!("\"abcd\""));
}

/// Regression test for the UTF-8 panic. `max_output_size` is a **byte** budget
/// and `serde_json` does not escape non-ASCII, so a tool that answers in French
/// used to abort the hook: the old `&output_json[..max_output_size]` panicked
/// with "byte index 10 is not a char boundary" as soon as the cut landed inside a
/// multi-byte character. `"ééééé"` is 12 bytes of JSON, and byte 10 sits between
/// the two bytes of the fifth `é`.
#[tokio::test]
async fn post_tool_use_truncating_an_accented_output_stores_a_shorter_string_instead_of_panicking()
{
    let bolt = FakeBolt::start().await;
    let cb = callback(
        bolt.graph().await,
        Neo4jHookCallbackConfig {
            max_output_size: 10,
            ..Default::default()
        },
    );

    fire(&cb, &post_tool_use("Bash", json!({}), json!("ééééé")), None).await;

    assert_eq!(
        bolt.last_params()["output"],
        json!("\"éééé...[truncated]"),
        "the cut walks back to the start of the straddling character"
    );
}

/// The same for a 4-byte character, where the budget can land at three different
/// offsets inside it, and for a budget entirely consumed by one character.
#[tokio::test]
async fn post_tool_use_truncating_inside_an_emoji_keeps_the_prefix_before_it() {
    let bolt = FakeBolt::start().await;
    for (budget, expected) in [
        (2, "\"...[truncated]"),
        (3, "\"...[truncated]"),
        (4, "\"...[truncated]"),
        (5, "\"🦀...[truncated]"),
    ] {
        let cb = callback(
            bolt.graph().await,
            Neo4jHookCallbackConfig {
                max_output_size: budget,
                ..Default::default()
            },
        );
        fire(&cb, &post_tool_use("Bash", json!({}), json!("🦀🦀")), None).await;
        assert_eq!(
            bolt.last_params()["output"],
            json!(expected),
            "budget of {budget} bytes"
        );
    }
}

// ---------------------------------------------------------------------------
// PostToolUse — the Meilisearch branch
// ---------------------------------------------------------------------------

/// The branch guarded by `index_in_meilisearch` builds a `ToolUsageDocument` and
/// then only `debug!`s its `tool_name`: the index named by `INDEX_TOOL_USAGE` is
/// never written. A Meilisearch mock that records every request proves it — the
/// document is assembled and dropped.
#[tokio::test]
async fn post_tool_use_builds_a_meilisearch_document_and_then_indexes_nothing() {
    let bolt = FakeBolt::start().await;
    let meili = support::http_mocks::meilisearch(Vec::new(), Vec::new()).await;
    let client = support::http_mocks::meilisearch_client(&meili)
        .await
        .expect("a client over the mock");
    let before = meili.received_requests().await.unwrap_or_default().len();

    let cb = Neo4jHookCallback::new(
        bolt.graph().await,
        Some(Arc::new(client)),
        Neo4jHookCallbackConfig::default(),
    );
    fire(
        &cb,
        &post_tool_use("Read", json!({"file_path": "a.txt"}), json!("content")),
        None,
    )
    .await;

    assert_eq!(
        bolt.runs().len(),
        1,
        "the Neo4j half of the write did happen"
    );
    assert_eq!(
        meili.received_requests().await.unwrap_or_default().len(),
        before,
        "but nothing was indexed; see the report"
    );
}

/// What the branch *should* do. Left `#[ignore]` because the fix is out of this
/// agent's scope: `MeilisearchClient` keeps its `Client` private and exposes only
/// `messages_index()` / `conversations_index()`, so reaching `nexus_tool_usage`
/// means editing `core/storage/meilisearch.rs`, which belongs to another agent.
///
/// Faulty function: `Neo4jHookCallback::handle_post_tool_use`, the
/// `if self.config.index_in_meilisearch && let Some(ref _ms) = self.meilisearch`
/// branch. Triggering input: any `PostToolUse` on a callback built with
/// `Some(meilisearch)` and the default config.
#[tokio::test]
#[ignore = "needs an index accessor on MeilisearchClient, which is another agent's file"]
async fn post_tool_use_should_index_the_document_in_the_tool_usage_index() {
    let bolt = FakeBolt::start().await;
    let meili = support::http_mocks::meilisearch(Vec::new(), Vec::new()).await;
    let client = support::http_mocks::meilisearch_client(&meili)
        .await
        .expect("a client over the mock");

    let cb = Neo4jHookCallback::new(
        bolt.graph().await,
        Some(Arc::new(client)),
        Neo4jHookCallbackConfig::default(),
    );
    fire(
        &cb,
        &post_tool_use("Read", json!({}), json!("content")),
        None,
    )
    .await;

    let paths: Vec<String> = meili
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(
        paths.contains(&"/indexes/nexus_tool_usage/documents".to_string()),
        "the document should reach the index the module names: {paths:?}"
    );
}

#[tokio::test]
async fn post_tool_use_skips_the_meilisearch_branch_when_indexing_is_disabled() {
    let bolt = FakeBolt::start().await;
    let meili = support::http_mocks::meilisearch(Vec::new(), Vec::new()).await;
    let client = support::http_mocks::meilisearch_client(&meili)
        .await
        .expect("a client over the mock");
    let before = meili.received_requests().await.unwrap_or_default().len();

    let cb = Neo4jHookCallback::new(
        bolt.graph().await,
        Some(Arc::new(client)),
        Neo4jHookCallbackConfig {
            index_in_meilisearch: false,
            ..Default::default()
        },
    );
    fire(&cb, &post_tool_use("Read", json!({}), json!("ok")), None).await;

    assert_eq!(
        meili.received_requests().await.unwrap_or_default().len(),
        before
    );
    assert_eq!(bolt.runs().len(), 1, "the Neo4j write is unaffected");
}

/// The search summaries are cut at 500 and 1000 **bytes** by the same byte-budget
/// helper, which used to slice without regard for character boundaries. This is
/// the second call site of that panic: a 600-byte French tool input with an `é`
/// straddling byte 500 aborted the hook *after* the Neo4j node had been written.
/// Returning at all is the assertion.
#[tokio::test]
async fn post_tool_use_summarising_an_accented_input_does_not_panic() {
    let bolt = FakeBolt::start().await;
    let meili = support::http_mocks::meilisearch(Vec::new(), Vec::new()).await;
    let client = support::http_mocks::meilisearch_client(&meili)
        .await
        .expect("a client over the mock");

    // The input is `{"v":"aaa…éôôô…"}` with the `é` astride byte 500, which is
    // where `input_summary` cuts; the response is a plain string with an `é`
    // astride byte 1000, where `output_summary` cuts.
    let input = json!({"v": format!("{}é{}", "a".repeat(493), "ô".repeat(300))});
    let response = Value::from(format!("{}é{}", "a".repeat(992), "ô".repeat(60)));

    let serialised_input = serde_json::to_string(&input).expect("serialisable");
    assert!(
        !serialised_input.is_char_boundary(500) && serialised_input.len() > 500,
        "the fixture must put a character astride the 500-byte input budget"
    );
    let serialised_output = serde_json::to_string(&response).expect("serialisable");
    assert!(
        !serialised_output.is_char_boundary(1000) && serialised_output.len() > 1000,
        "and another astride the 1000-byte output budget"
    );
    assert!(
        serialised_output.len() < Neo4jHookCallbackConfig::default().max_output_size,
        "while staying under the Neo4j budget, so this is the summary path"
    );

    let cb = Neo4jHookCallback::new(
        bolt.graph().await,
        Some(Arc::new(client)),
        Neo4jHookCallbackConfig::default(),
    );
    let output = fire(&cb, &post_tool_use("Bash", input, response), None).await;

    assert_transparent(&output);
    assert_eq!(bolt.runs().len(), 1);
}

// ---------------------------------------------------------------------------
// PostToolUse — a graph that refuses the write
// ---------------------------------------------------------------------------

/// A failed write is logged at `warn!` and the hook still answers "continue":
/// the tool call is not blocked because its audit record was lost. That is the
/// right call for an audit hook, but it means the loss is invisible to the
/// caller — only the gateway log knows.
#[tokio::test]
async fn post_tool_use_reports_success_even_though_neo4j_refused_the_write() {
    let bolt = FakeBolt::start().await;
    bolt.failing("no write access");
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    let output = fire(&cb, &post_tool_use("Bash", json!({}), json!("ok")), None).await;

    assert_transparent(&output);
    assert_eq!(bolt.runs().len(), 1, "the statement was attempted");
}

// ---------------------------------------------------------------------------
// UserPromptSubmit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn user_prompt_submit_writes_the_prompt_verbatim() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    let output = fire(&cb, &user_prompt("Dis bonjour, s'il te plaît"), None).await;

    assert_transparent(&output);
    let run = bolt.run_matching("CREATE (p:NexusUserPrompt");
    assert!(run.cypher.contains("CREATE (c)-[:HAS_PROMPT]->(p)"));
    assert_eq!(run.params["prompt"], json!("Dis bonjour, s'il te plaît"));
    assert_eq!(run.params["session_id"], json!(SESSION));
    assert_eq!(
        run.params["id"].as_str().map(str::len),
        Some(36),
        "a fresh UUID per prompt"
    );
    chrono::DateTime::parse_from_rfc3339(run.params["now"].as_str().expect("now"))
        .expect("now is RFC 3339");
}

/// Unlike the tool output, the prompt is never truncated: `max_output_size` does
/// not apply to it, so a megabyte-long prompt goes to Neo4j whole. And an empty
/// prompt is stored rather than refused.
#[tokio::test]
async fn user_prompt_submit_stores_an_empty_prompt_and_never_truncates_a_long_one() {
    let bolt = FakeBolt::start().await;
    let cb = callback(
        bolt.graph().await,
        Neo4jHookCallbackConfig {
            max_output_size: 4,
            ..Default::default()
        },
    );

    fire(&cb, &user_prompt(""), None).await;
    assert_eq!(bolt.last_params()["prompt"], json!(""));

    let long = "é".repeat(5_000);
    fire(&cb, &user_prompt(&long), None).await;
    assert_eq!(
        bolt.last_params()["prompt"],
        json!(long),
        "the 4-byte output budget does not apply to prompts"
    );
}

#[tokio::test]
async fn user_prompt_submit_reports_success_even_though_neo4j_refused_the_write() {
    let bolt = FakeBolt::start().await;
    bolt.failing("no write access");
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    let output = fire(&cb, &user_prompt("perdu"), None).await;

    assert_transparent(&output);
    assert_eq!(bolt.runs().len(), 1);
}

// ---------------------------------------------------------------------------
// Stop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_writes_a_session_event_carrying_the_stop_hook_active_flag() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    let output = fire(&cb, &stop(true), None).await;

    assert_transparent(&output);
    let run = bolt.run_matching("CREATE (e:NexusSessionEvent");
    assert!(
        run.cypher.contains("event_type: 'stop'"),
        "the type is a Cypher literal, not a parameter: {}",
        run.cypher
    );
    assert!(run.cypher.contains("CREATE (s)-[:HAS_EVENT]->(e)"));
    assert_eq!(run.params["stop_hook_active"], json!(true));
    assert_eq!(run.params["session_id"], json!(SESSION));
}

/// The node the code writes is not the node the module documents. The schema at
/// the top of `neo4j_hook_callback.rs` gives `NexusSessionEvent` a `duration_ms`,
/// a `turn_count` and a `cost_usd`, and says `event_type` is one of `"start"`,
/// `"stop"` or `"compact"`. In reality the only statement that creates one of
/// these nodes writes `stop_hook_active`, `tool_count` and `total_duration_ms`,
/// and hard-codes `event_type: 'stop'` — so three documented properties are never
/// set and two of the three documented event types are unreachable. See the
/// report; the domain diagram should show one event type, not three.
#[tokio::test]
async fn the_stop_event_contradicts_the_documented_session_event_schema() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &stop(true), None).await;

    let run = bolt.run_matching("NexusSessionEvent");
    for documented in ["turn_count", "cost_usd"] {
        assert!(
            !run.cypher.contains(documented),
            "the doc-comment promises {documented}, the statement never sets it: {}",
            run.cypher
        );
    }
    // `duration_ms` appears only as the *tool* property being summed, never as a
    // property of the event itself.
    assert!(!run.cypher.contains("duration_ms: $"), "{}", run.cypher);
    for actual in ["stop_hook_active", "tool_count", "total_duration_ms"] {
        assert!(run.cypher.contains(actual), "{}", run.cypher);
    }
    // Only `'stop'` is ever produced; nothing in the module writes `'start'` or
    // `'compact'`.
    assert!(!run.cypher.contains("'start'") && !run.cypher.contains("'compact'"));
}

#[tokio::test]
async fn stop_writes_a_false_flag_as_false_rather_than_omitting_it() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &stop(false), None).await;

    assert_eq!(bolt.last_params()["stop_hook_active"], json!(false));
}

/// The stop statement is also where the session totals are computed, and it is
/// the reason `handle_post_tool_use` must write `null` rather than a sentinel:
/// `COALESCE(t.duration_ms, 0)` turns an unknown duration into zero, but it
/// cannot undo a `-1`.
#[tokio::test]
async fn stop_asks_cypher_to_total_the_tool_durations_treating_null_as_zero() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &stop(true), None).await;

    let run = bolt.run_matching("NexusSessionEvent");
    assert!(
        run.cypher.contains("sum(COALESCE(t.duration_ms, 0))"),
        "{}",
        run.cypher
    );
    assert!(run.cypher.contains("count(t) as tool_count"));
    assert!(run.cypher.contains("SET e.tool_count = tool_count"));
    assert!(run.cypher.contains("e.total_duration_ms = total_duration"));
    assert!(
        run.cypher
            .contains("OPTIONAL MATCH (t:NexusToolUsage {session_id: $session_id})"),
        "the total is scoped to the session: {}",
        run.cypher
    );
}

#[tokio::test]
async fn stop_reports_success_even_though_neo4j_refused_the_write() {
    let bolt = FakeBolt::start().await;
    bolt.failing("no write access");
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    let output = fire(&cb, &stop(true), None).await;

    assert_transparent(&output);
    assert_eq!(bolt.runs().len(), 1);
}

// ---------------------------------------------------------------------------
// HookCallback::execute — the variants the match drops
// ---------------------------------------------------------------------------

/// `SubagentStop` falls into the catch-all arm: a sub-agent finishing leaves no
/// `NexusSessionEvent`, so a session's event list has stops for the main agent
/// only.
#[tokio::test]
async fn execute_drops_a_subagent_stop_without_writing_anything() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    let input = HookInput::SubagentStop(SubagentStopHookInput {
        session_id: SESSION.to_string(),
        transcript_path: "transcript.jsonl".to_string(),
        cwd: "workdir".to_string(),
        permission_mode: None,
        stop_hook_active: true,
    });
    let output = fire(&cb, &input, None).await;

    assert_transparent(&output);
    assert!(
        bolt.runs().is_empty(),
        "nothing recorded the sub-agent stop: {:?}",
        bolt.cypher()
    );
}

/// `PreCompact` is dropped too, even though the schema at the top of the module
/// advertises `event_type: "compact"` on `NexusSessionEvent`: no code path can
/// ever write that value. See the report.
#[tokio::test]
async fn execute_drops_a_pre_compact_although_the_schema_documents_a_compact_event() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    let input = HookInput::PreCompact(PreCompactHookInput {
        session_id: SESSION.to_string(),
        transcript_path: "transcript.jsonl".to_string(),
        cwd: "workdir".to_string(),
        permission_mode: None,
        trigger: "auto".to_string(),
        custom_instructions: None,
    });
    let output = fire(&cb, &input, None).await;

    assert_transparent(&output);
    assert!(bolt.runs().is_empty(), "{:?}", bolt.cypher());
}

/// `tool_use_id` is only consulted by the two tool hooks; the others ignore it,
/// so passing one does not change the prompt or stop statements.
#[tokio::test]
async fn a_tool_use_id_is_ignored_by_the_prompt_and_stop_handlers() {
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &user_prompt("bonjour"), Some("t-1")).await;
    fire(&cb, &stop(false), Some("t-1")).await;

    // The start time registered by neither call is still absent, so a later
    // PostToolUse for `t-1` is untimed.
    fire(
        &cb,
        &post_tool_use("Bash", json!({}), json!("ok")),
        Some("t-1"),
    )
    .await;
    assert_eq!(bolt.last_params()["duration_ms"], Value::Null);
    assert_eq!(bolt.runs().len(), 3);
}
