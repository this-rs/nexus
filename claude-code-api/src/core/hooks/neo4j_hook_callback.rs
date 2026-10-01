//! Neo4j-backed hook callback implementation
//!
//! Captures all Claude Code events into Neo4j for knowledge graph persistence.

#![allow(dead_code)] // Public API - may not be used internally
//!
//! ## Schema
//!
//! ```cypher
//! // Nodes
//! (:NexusToolUsage {
//!     id: String,
//!     tool_name: String,
//!     input: String,      // JSON serialized
//!     output: String,     // JSON serialized (truncated if large)
//!     duration_ms: Int?,
//!     session_id: String,
//!     created_at: DateTime
//! })
//!
//! (:NexusUserPrompt {
//!     id: String,
//!     prompt: String,
//!     session_id: String,
//!     created_at: DateTime
//! })
//!
//! (:NexusSessionEvent {
//!     id: String,
//!     event_type: String,  // "start", "stop", "compact"
//!     session_id: String,
//!     duration_ms: Int?,
//!     turn_count: Int?,
//!     cost_usd: Float?,
//!     created_at: DateTime
//! })
//!
//! // Relationships
//! (:NexusConversation)-[:HAS_TOOL_USAGE]->(:NexusToolUsage)
//! (:NexusConversation)-[:HAS_PROMPT]->(:NexusUserPrompt)
//! (:NexusSession)-[:HAS_EVENT]->(:NexusSessionEvent)
//!
//! // Constraints
//! CREATE CONSTRAINT nexus_tool_usage_id IF NOT EXISTS FOR (t:NexusToolUsage) REQUIRE t.id IS UNIQUE;
//! CREATE CONSTRAINT nexus_user_prompt_id IF NOT EXISTS FOR (p:NexusUserPrompt) REQUIRE p.id IS UNIQUE;
//! CREATE CONSTRAINT nexus_session_event_id IF NOT EXISTS FOR (e:NexusSessionEvent) REQUIRE e.id IS UNIQUE;
//! ```

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use neo4rs::{Graph, query};
use nexus_claude::{
    HookCallback, HookContext, HookInput, HookJSONOutput, PostToolUseHookInput,
    PreToolUseHookInput, SdkError, StopHookInput, SyncHookJSONOutput, UserPromptSubmitHookInput,
};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::core::storage::meilisearch::MeilisearchClient;

/// Index name for tool usage documents in Meilisearch
pub const INDEX_TOOL_USAGE: &str = "nexus_tool_usage";

/// Configuration for Neo4jHookCallback
#[derive(Clone, Debug)]
pub struct Neo4jHookCallbackConfig {
    /// Maximum size of tool output to store (bytes)
    pub max_output_size: usize,
    /// Whether to index tool usage in Meilisearch
    pub index_in_meilisearch: bool,
    /// Whether to log PreToolUse events (verbose)
    pub log_pre_tool_use: bool,
}

impl Default for Neo4jHookCallbackConfig {
    fn default() -> Self {
        Self {
            max_output_size: 10_000, // 10KB max
            index_in_meilisearch: true,
            log_pre_tool_use: false,
        }
    }
}

/// Tool usage document for Meilisearch indexing
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolUsageDocument {
    pub id: String,
    pub tool_name: String,
    pub input_summary: String,
    pub output_summary: String,
    pub session_id: String,
    pub created_at: i64,
}

/// Neo4j-backed hook callback that captures all conversation events
pub struct Neo4jHookCallback {
    graph: Arc<Graph>,
    meilisearch: Option<Arc<MeilisearchClient>>,
    config: Neo4jHookCallbackConfig,
    /// Track PreToolUse timestamps for duration calculation
    tool_start_times: dashmap::DashMap<String, Instant>,
}

impl Neo4jHookCallback {
    /// Create a new Neo4jHookCallback
    pub fn new(
        graph: Arc<Graph>,
        meilisearch: Option<Arc<MeilisearchClient>>,
        config: Neo4jHookCallbackConfig,
    ) -> Self {
        Self {
            graph,
            meilisearch,
            config,
            tool_start_times: dashmap::DashMap::new(),
        }
    }

    /// Initialize Neo4j schema for hook events
    pub async fn init_schema(&self) -> Result<()> {
        let constraints = vec![
            "CREATE CONSTRAINT nexus_tool_usage_id IF NOT EXISTS FOR (t:NexusToolUsage) REQUIRE t.id IS UNIQUE",
            "CREATE CONSTRAINT nexus_user_prompt_id IF NOT EXISTS FOR (p:NexusUserPrompt) REQUIRE p.id IS UNIQUE",
            "CREATE CONSTRAINT nexus_session_event_id IF NOT EXISTS FOR (e:NexusSessionEvent) REQUIRE e.id IS UNIQUE",
        ];

        for constraint in constraints {
            if let Err(e) = self.graph.run(query(constraint)).await {
                debug!("Constraint creation result: {:?}", e);
            }
        }

        // Create index for tool_name searches
        let index = "CREATE INDEX nexus_tool_usage_name IF NOT EXISTS FOR (t:NexusToolUsage) ON (t.tool_name)";
        if let Err(e) = self.graph.run(query(index)).await {
            debug!("Index creation result: {:?}", e);
        }

        info!("Neo4j hook schema initialized");
        Ok(())
    }

    /// Initialize Meilisearch index for tool usage
    pub async fn init_meilisearch_index(&self) -> Result<()> {
        if let Some(ref _ms) = self.meilisearch {
            // Create index (ignore if exists)
            // For now, we'll just log - actual index creation is handled by MeilisearchClient
            debug!("Meilisearch tool usage index ready");
        }
        Ok(())
    }

    /// Handle PreToolUse event - record start time for duration tracking
    async fn handle_pre_tool_use(
        &self,
        input: &PreToolUseHookInput,
        tool_use_id: Option<&str>,
    ) -> Result<HookJSONOutput, SdkError> {
        // Record start time for this tool use
        if let Some(id) = tool_use_id {
            self.tool_start_times.insert(id.to_string(), Instant::now());
        }

        if self.config.log_pre_tool_use {
            debug!(
                "PreToolUse: {} (session: {})",
                input.tool_name, input.session_id
            );
        }

        Ok(HookJSONOutput::Sync(SyncHookJSONOutput::default()))
    }

    /// Handle PostToolUse event - persist tool usage to Neo4j
    async fn handle_post_tool_use(
        &self,
        input: &PostToolUseHookInput,
        tool_use_id: Option<&str>,
    ) -> Result<HookJSONOutput, SdkError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();

        // Calculate duration if we have a start time
        let duration_ms: Option<i64> = tool_use_id.and_then(|tid| {
            self.tool_start_times
                .remove(tid)
                .map(|(_, start)| start.elapsed().as_millis() as i64)
        });

        // Serialize input/output, truncating if needed
        let input_json =
            serde_json::to_string(&input.tool_input).unwrap_or_else(|_| "{}".to_string());

        let output_json =
            serde_json::to_string(&input.tool_response).unwrap_or_else(|_| "{}".to_string());

        let output_truncated = if output_json.len() > self.config.max_output_size {
            format!(
                "{}...[truncated]",
                floor_char_boundary(&output_json, self.config.max_output_size)
            )
        } else {
            output_json.clone()
        };

        // Store in Neo4j
        let q = query(
            "CREATE (t:NexusToolUsage {
                id: $id,
                tool_name: $tool_name,
                input: $input,
                output: $output,
                duration_ms: $duration_ms,
                session_id: $session_id,
                created_at: datetime($now)
            })
            WITH t
            OPTIONAL MATCH (c:NexusConversation)
            WHERE c.id CONTAINS $session_id OR EXISTS {
                MATCH (s:NexusSession {id: $session_id})-[:HAS_CONVERSATION]->(c)
            }
            WITH t, c LIMIT 1
            FOREACH (_ IN CASE WHEN c IS NOT NULL THEN [1] ELSE [] END |
                CREATE (c)-[:HAS_TOOL_USAGE]->(t)
            )
            RETURN t.id as id",
        )
        .param("id", id.clone())
        .param("tool_name", input.tool_name.clone())
        .param("input", input_json.clone())
        .param("output", output_truncated)
        // `Option<i64>` so an untimed tool use stores `null`, which is what the
        // schema at the top of this module documents (`duration_ms: Int?`) and
        // what `handle_stop`'s `sum(COALESCE(t.duration_ms, 0))` expects. The
        // previous sentinel `-1` was indistinguishable from a measurement and
        // subtracted a millisecond per untimed tool from the session total.
        .param("duration_ms", duration_ms)
        .param("session_id", input.session_id.clone())
        .param("now", now.to_rfc3339());

        if let Err(e) = self.graph.run(q).await {
            warn!("Failed to store tool usage in Neo4j: {}", e);
        } else if let Some(ms) = duration_ms {
            debug!("Stored tool usage: {} ({}ms)", input.tool_name, ms);
        } else {
            // Not `(-1ms)`: the log carried the same sentinel as the node, so a
            // tool call nobody timed read as a measured one.
            debug!("Stored tool usage: {} (untimed)", input.tool_name);
        }

        // Index in Meilisearch if configured
        if self.config.index_in_meilisearch
            && let Some(ref _ms) = self.meilisearch
        {
            let doc = ToolUsageDocument {
                id: id.clone(),
                tool_name: input.tool_name.clone(),
                input_summary: truncate_for_search(&input_json, 500),
                output_summary: truncate_for_search(&output_json, 1000),
                session_id: input.session_id.clone(),
                created_at: now.timestamp(),
            };

            // Use a custom index for tool usage - for now just log
            // In production, we'd create a dedicated tool_usage index
            debug!("Would index tool usage in Meilisearch: {}", doc.tool_name);
        }

        Ok(HookJSONOutput::Sync(SyncHookJSONOutput::default()))
    }

    /// Handle UserPromptSubmit event - persist user prompt to Neo4j
    async fn handle_user_prompt_submit(
        &self,
        input: &UserPromptSubmitHookInput,
    ) -> Result<HookJSONOutput, SdkError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();

        let q = query(
            "CREATE (p:NexusUserPrompt {
                id: $id,
                prompt: $prompt,
                session_id: $session_id,
                created_at: datetime($now)
            })
            WITH p
            OPTIONAL MATCH (c:NexusConversation)
            WHERE EXISTS {
                MATCH (s:NexusSession {id: $session_id})-[:HAS_CONVERSATION]->(c)
            }
            WITH p, c LIMIT 1
            FOREACH (_ IN CASE WHEN c IS NOT NULL THEN [1] ELSE [] END |
                CREATE (c)-[:HAS_PROMPT]->(p)
            )
            RETURN p.id as id",
        )
        .param("id", id)
        .param("prompt", input.prompt.clone())
        .param("session_id", input.session_id.clone())
        .param("now", now.to_rfc3339());

        if let Err(e) = self.graph.run(q).await {
            warn!("Failed to store user prompt in Neo4j: {}", e);
        } else {
            debug!("Stored user prompt for session: {}", input.session_id);
        }

        Ok(HookJSONOutput::Sync(SyncHookJSONOutput::default()))
    }

    /// Handle Stop event - finalize session with stats
    async fn handle_stop(&self, input: &StopHookInput) -> Result<HookJSONOutput, SdkError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();

        // Create session stop event
        let q = query(
            "CREATE (e:NexusSessionEvent {
                id: $id,
                event_type: 'stop',
                session_id: $session_id,
                stop_hook_active: $stop_hook_active,
                created_at: datetime($now)
            })
            WITH e
            OPTIONAL MATCH (s:NexusSession {id: $session_id})
            WITH e, s
            FOREACH (_ IN CASE WHEN s IS NOT NULL THEN [1] ELSE [] END |
                CREATE (s)-[:HAS_EVENT]->(e)
            )
            // Calculate session stats
            WITH e
            OPTIONAL MATCH (t:NexusToolUsage {session_id: $session_id})
            WITH e, count(t) as tool_count, sum(COALESCE(t.duration_ms, 0)) as total_duration
            SET e.tool_count = tool_count,
                e.total_duration_ms = total_duration
            RETURN e.id as id",
        )
        .param("id", id)
        .param("session_id", input.session_id.clone())
        .param("stop_hook_active", input.stop_hook_active)
        .param("now", now.to_rfc3339());

        if let Err(e) = self.graph.run(q).await {
            warn!("Failed to store session stop event in Neo4j: {}", e);
        } else {
            info!("Session stopped: {}", input.session_id);
        }

        Ok(HookJSONOutput::Sync(SyncHookJSONOutput::default()))
    }
}

#[async_trait]
impl HookCallback for Neo4jHookCallback {
    async fn execute(
        &self,
        input: &HookInput,
        tool_use_id: Option<&str>,
        _context: &HookContext,
    ) -> Result<HookJSONOutput, SdkError> {
        match input {
            HookInput::PreToolUse(pre) => self.handle_pre_tool_use(pre, tool_use_id).await,
            HookInput::PostToolUse(post) => self.handle_post_tool_use(post, tool_use_id).await,
            HookInput::UserPromptSubmit(prompt) => self.handle_user_prompt_submit(prompt).await,
            HookInput::Stop(stop) => self.handle_stop(stop).await,
            // Other hook types are not persisted. `SubagentStop` and
            // `PreCompact` reach this arm, so nothing records a sub-agent
            // finishing or a context compaction even though the schema above
            // lists `event_type: "compact"`. Name the dropped event rather than
            // discarding it without a trace.
            other => {
                debug!("Hook event not persisted by Neo4jHookCallback: {other:?}");
                Ok(HookJSONOutput::Sync(SyncHookJSONOutput::default()))
            },
        }
    }
}

/// Truncate a string for search indexing
///
/// `max_len` is a **byte** budget, so the cut is rounded down to the nearest
/// UTF-8 character boundary (see [`floor_char_boundary`]): the summaries come
/// from `serde_json`, which emits non-ASCII verbatim.
fn truncate_for_search(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", floor_char_boundary(s, max_len))
    }
}

/// The longest prefix of `s` that is at most `max_bytes` long **and** ends on a
/// UTF-8 character boundary.
///
/// Both truncations in this module measure a byte budget against a string built
/// by `serde_json`, which does not escape non-ASCII: a tool input or response
/// holding one accent, one emoji or one CJK glyph can put a multi-byte character
/// astride the limit. Slicing there with `&s[..max_bytes]` panics, and a panic
/// inside a hook callback takes the hook down rather than storing a shorter
/// string, so the cut walks back to the start of that character instead.
fn floor_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_for_search() {
        assert_eq!(truncate_for_search("hello", 10), "hello");
        assert_eq!(truncate_for_search("hello world", 5), "hello...");
    }

    /// `max_len` is a byte budget, so the boundary cases are the exact length
    /// and one byte over it.
    #[test]
    fn truncate_for_search_keeps_a_string_of_exactly_max_len() {
        assert_eq!(truncate_for_search("hello", 5), "hello");
        assert_eq!(truncate_for_search("hello!", 5), "hello...");
    }

    #[test]
    fn truncate_for_search_with_a_zero_budget_is_only_the_ellipsis() {
        assert_eq!(truncate_for_search("hello", 0), "...");
        assert_eq!(truncate_for_search("", 0), "");
    }

    /// Regression test for the panic: `truncate_for_search` used to slice at
    /// `&s[..max_len]`, which aborts when the budget lands inside a multi-byte
    /// character. `é` is two bytes, so a budget of 4 on `"aaaéaaa"` falls between
    /// them. Against the previous code this test panics with
    /// "byte index 4 is not a char boundary".
    #[test]
    fn truncate_for_search_does_not_panic_on_a_multibyte_boundary() {
        assert_eq!(truncate_for_search("aaaéaaa", 4), "aaa...");
        // A 3-byte CJK glyph and a 4-byte emoji, cut at every offset inside them.
        assert_eq!(truncate_for_search("ab漢字", 3), "ab...");
        assert_eq!(truncate_for_search("ab漢字", 4), "ab...");
        assert_eq!(truncate_for_search("ab漢字", 5), "ab漢...");
        assert_eq!(truncate_for_search("ab🦀!", 3), "ab...");
        assert_eq!(truncate_for_search("ab🦀!", 5), "ab...");
        assert_eq!(truncate_for_search("ab🦀!", 6), "ab🦀...");
    }

    /// The whole budget may be eaten by a single character, in which case the
    /// prefix is empty rather than a panic.
    #[test]
    fn floor_char_boundary_returns_an_empty_prefix_when_the_first_char_is_too_wide() {
        assert_eq!(floor_char_boundary("🦀x", 1), "");
        assert_eq!(floor_char_boundary("🦀x", 3), "");
        assert_eq!(floor_char_boundary("🦀x", 4), "🦀");
    }

    #[test]
    fn floor_char_boundary_returns_the_whole_string_when_it_fits() {
        assert_eq!(floor_char_boundary("héllo", 6), "héllo");
        assert_eq!(floor_char_boundary("héllo", 99), "héllo");
    }

    /// `INDEX_TOOL_USAGE` is the index `handle_post_tool_use` claims to write to.
    /// It is a `pub const` that `hooks::mod` does not re-export and that nothing
    /// in the crate reads: the Meilisearch branch builds a
    /// [`ToolUsageDocument`] and then only `debug!`s it. See the report.
    #[test]
    fn index_tool_usage_names_an_index_nothing_writes_to() {
        assert_eq!(INDEX_TOOL_USAGE, "nexus_tool_usage");
    }

    /// The document is the hook's Meilisearch contract, so its field names are
    /// part of the stored shape: `created_at` is a Unix timestamp (seconds),
    /// unlike the Neo4j node, which stores an ISO-8601 `datetime()`.
    #[test]
    fn tool_usage_document_serialises_with_the_field_names_meilisearch_indexes() {
        let doc = ToolUsageDocument {
            id: "doc-1".to_string(),
            tool_name: "Read".to_string(),
            input_summary: "{\"file\":\"a.txt\"}".to_string(),
            output_summary: "ok".to_string(),
            session_id: "sess-1".to_string(),
            created_at: 1_700_000_000,
        };

        let json = serde_json::to_value(&doc).expect("a document is serialisable");
        assert_eq!(
            json,
            serde_json::json!({
                "id": "doc-1",
                "tool_name": "Read",
                "input_summary": "{\"file\":\"a.txt\"}",
                "output_summary": "ok",
                "session_id": "sess-1",
                "created_at": 1_700_000_000,
            })
        );

        let back: ToolUsageDocument =
            serde_json::from_value(json).expect("and round-trips back out of the index");
        assert_eq!(back.id, doc.id);
        assert_eq!(back.created_at, doc.created_at);
    }

    #[test]
    fn config_can_be_built_without_meilisearch_indexing() {
        let config = Neo4jHookCallbackConfig {
            max_output_size: 16,
            index_in_meilisearch: false,
            log_pre_tool_use: true,
        };
        // `Clone` + `Debug` are part of the public surface: the gateway stores
        // the config by value in the callback and logs it on startup.
        let clone = config.clone();
        assert_eq!(clone.max_output_size, 16);
        assert!(!clone.index_in_meilisearch);
        assert!(clone.log_pre_tool_use);
        assert!(format!("{config:?}").contains("max_output_size: 16"));
    }

    #[test]
    fn test_config_default() {
        let config = Neo4jHookCallbackConfig::default();
        assert_eq!(config.max_output_size, 10_000);
        assert!(config.index_in_meilisearch);
        assert!(!config.log_pre_tool_use);
    }

    #[tokio::test]
    #[ignore]
    async fn test_neo4j_hook_callback_integration() {
        // This test requires a running Neo4j instance
        use crate::core::storage::Neo4jConfig;

        let config = Neo4jConfig::default();
        let graph = neo4rs::Graph::new(&config.uri, &config.user, &config.password)
            .await
            .unwrap();

        let callback =
            Neo4jHookCallback::new(Arc::new(graph), None, Neo4jHookCallbackConfig::default());

        callback.init_schema().await.unwrap();

        // Test PostToolUse handling
        let input = HookInput::PostToolUse(PostToolUseHookInput {
            agent_id: None,
            agent_type: None,
            session_id: "test-session".to_string(),
            transcript_path: "/tmp/transcript".to_string(),
            cwd: "/tmp".to_string(),
            permission_mode: None,
            tool_name: "Read".to_string(),
            tool_input: serde_json::json!({"file": "test.txt"}),
            tool_response: serde_json::json!({"content": "Hello, World!"}),
        });

        let context = HookContext { signal: None };
        let result = callback.execute(&input, Some("tool-123"), &context).await;

        assert!(result.is_ok());
    }
}
