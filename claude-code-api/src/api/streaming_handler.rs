//! Enhanced streaming handler with real chunking support

use crate::{
    core::interactive_session::InteractiveSessionManager,
    models::{
        claude::ClaudeCodeOutput,
        openai::{
            ChatCompletionStreamResponse, DeltaFunctionCall, DeltaMessage, DeltaToolCall,
            StreamError, Usage,
        },
    },
    utils::text_chunker::{ChunkConfig, chunk_text},
};
use futures::stream::{Stream, StreamExt};
use serde_json::Value;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Guard that auto-interrupts the CLI session when the SSE stream is dropped.
///
/// When the HTTP client disconnects (Escape, close tab, network drop), Axum
/// drops the SSE response stream. This guard detects that drop and sends an
/// interrupt to the CLI process so it stops generating tokens.
///
/// The guard is "defused" (no interrupt) when the stream completes normally
/// (i.e. a `result` message was received from the CLI).
struct SseDisconnectGuard {
    session_manager: Arc<InteractiveSessionManager>,
    conversation_id: String,
    /// Set to `true` when the stream completes normally (result received).
    /// When `false` at drop time, the guard fires an interrupt.
    completed_normally: Arc<AtomicBool>,
}

impl Drop for SseDisconnectGuard {
    fn drop(&mut self) {
        if self.completed_normally.load(Ordering::SeqCst) {
            debug!(
                "SSE stream completed normally for session {}, no interrupt needed",
                self.conversation_id
            );
            return;
        }

        // Client disconnected before the stream finished — interrupt the CLI
        let session_manager = self.session_manager.clone();
        let conversation_id = self.conversation_id.clone();

        warn!(
            "SSE client disconnected for session {}, sending interrupt to CLI",
            conversation_id
        );

        // We can't await in Drop, so spawn a task.
        // interrupt_session() is synchronous (uses read lock + try_send),
        // but we still spawn to avoid blocking the drop.
        tokio::spawn(async move {
            match session_manager.interrupt_session(&conversation_id) {
                Ok(true) => {
                    info!(
                        "Auto-interrupted session {} on SSE disconnect",
                        conversation_id
                    );
                },
                Ok(false) => {
                    debug!(
                        "Session {} not found for auto-interrupt (already closed?)",
                        conversation_id
                    );
                },
                Err(e) => {
                    warn!(
                        "Failed to auto-interrupt session {} on SSE disconnect: {}",
                        conversation_id, e
                    );
                },
            }
        });
    }
}

/// Handle streaming response with text chunking for better UX.
///
/// When `session_manager` and `conversation_id` are provided, an
/// `SseDisconnectGuard` is installed that auto-interrupts the CLI if the
/// HTTP client drops the SSE connection before the stream finishes.
pub async fn handle_enhanced_streaming_response(
    model: String,
    mut rx: mpsc::Receiver<ClaudeCodeOutput>,
    session_manager: Option<Arc<InteractiveSessionManager>>,
    conversation_id: Option<String>,
) -> Pin<Box<dyn Stream<Item = ChatCompletionStreamResponse> + Send>> {
    let stream = async_stream::stream! {
        let stream_id = Uuid::new_v4().to_string();
        // Did this turn ask for a tool? It decides the `finish_reason`: OpenAI says
        // `tool_calls` when the model stopped to call one, and a client that drives
        // an agent loop branches on exactly that.
        let mut saw_tool_call = false;

        // Install disconnect guard if session info is provided.
        // The guard is held alive for the lifetime of the stream.
        // If the stream is dropped (client disconnect), the guard fires.
        let completed_flag = Arc::new(AtomicBool::new(false));
        let _guard = session_manager.zip(conversation_id).map(|(sm, cid)| {
            SseDisconnectGuard {
                session_manager: sm,
                conversation_id: cid,
                completed_normally: completed_flag.clone(),
            }
        });

        // First, send the initial message with role
        yield ChatCompletionStreamResponse::empty(stream_id.clone(), model.clone())
            .with_choice(
                DeltaMessage {
                    role: Some("assistant".to_string()),
                    content: None,
                    tool_calls: None,
                },
                None,
            );

        while let Some(output) = rx.recv().await {
            // Skip messages from subagent sidechains (Task tool executions).
            // Only top-level messages should be streamed to the client.
            if output.is_sidechain() {
                debug!(
                    "Streaming: skipping sidechain message (parent_tool_use_id: {:?})",
                    output.parent_tool_use_id()
                );
                continue;
            }

            match output.r#type.as_str() {
                "assistant" => {
                    // Extract content blocks (text + tool_use) from the assistant message
                    if let Some(message) = output.data.get("message")
                        && let Some(content_array) = message.get("content").and_then(|c| c.as_array()) {

                        let mut tool_call_index: i32 = 0;

                        for content in content_array {
                            let block_type = content.get("type").and_then(|t| t.as_str()).unwrap_or("");

                            match block_type {
                                "text" => {
                                    if let Some(text) = content.get("text").and_then(|t| t.as_str()) {
                                        // Chunk the text for streaming
                                        let config = ChunkConfig {
                                            chunk_size: 15,  // Smaller chunks for better streaming effect
                                            chunk_delay_ms: 30,  // 30ms between chunks
                                            word_boundary: true,
                                        };

                                        let mut chunker = chunk_text(text.to_string(), Some(config));

                                        while let Some(chunk) = chunker.next().await {
                                            yield ChatCompletionStreamResponse::empty(
                                                stream_id.clone(),
                                                model.clone(),
                                            )
                                            .with_choice(
                                                DeltaMessage {
                                                    role: None,
                                                    content: Some(chunk),
                                                    tool_calls: None,
                                                },
                                                None,
                                            );
                                        }
                                    }
                                },
                                "tool_use" => {
                                    // Stream tool_use as OpenAI tool_call delta
                                    let tool_id = content.get("id")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string())
                                        .unwrap_or_else(|| format!("call_{}", Uuid::new_v4()));
                                    let tool_name = content.get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let tool_input = content.get("input")
                                        .cloned()
                                        .unwrap_or(serde_json::json!({}));

                                    debug!(
                                        "Streaming tool_use: id={}, name={}, index={}",
                                        tool_id, tool_name, tool_call_index
                                    );

                                    // Send the complete tool call in a single chunk
                                    // (Claude CLI gives us complete tool_use, not incremental)
                                    yield ChatCompletionStreamResponse::empty(
                                        stream_id.clone(),
                                        model.clone(),
                                    )
                                    .with_choice(
                                        DeltaMessage {
                                            role: None,
                                            content: None,
                                            tool_calls: Some(vec![DeltaToolCall {
                                                index: tool_call_index,
                                                id: Some(tool_id),
                                                tool_type: Some("function".to_string()),
                                                function: Some(DeltaFunctionCall {
                                                    name: Some(tool_name),
                                                    arguments: Some(tool_input.to_string()),
                                                }),
                                            }]),
                                        },
                                        None,
                                    );

                                    tool_call_index += 1;
                                    saw_tool_call = true;
                                },
                                _ => {
                                    debug!("Streaming: ignoring content block type: {}", block_type);
                                }
                            }
                        }
                    }
                }
                "result" => {
                    if is_error_result(&output) {
                        // A failed turn must not claim it stopped normally: `stop`
                        // tells the client the answer is complete. It also leaves
                        // the disconnect guard armed, so the CLI session is
                        // interrupted instead of left running.
                        warn!("Streaming: result flagged as an error, emitting an error frame");
                        yield error_frame(&stream_id, &model, error_message(&output));
                        continue;
                    }

                    // Defuse the disconnect guard — stream completed normally
                    completed_flag.store(true, Ordering::SeqCst);

                    // Send the final chunk with finish_reason
                    let finish_reason = if saw_tool_call { "tool_calls" } else { "stop" };
                    yield ChatCompletionStreamResponse::empty(stream_id.clone(), model.clone())
                        .with_choice(DeltaMessage::default(), Some(finish_reason.to_string()));

                    // Then the usage chunk, when the CLI reported any. A turn whose
                    // counts are unknown gets no chunk rather than a chunk of zeros.
                    if let Some(usage) = usage_of(&output) {
                        let mut chunk =
                            ChatCompletionStreamResponse::empty(stream_id.clone(), model.clone());
                        chunk.usage = Some(usage);
                        yield chunk;
                    }
                }
                "error" => {
                    // Without this arm the output fell into `_ => {}` and the client
                    // was handed a `[DONE]` it could not tell from a short answer.
                    warn!("Streaming: CLI reported an error output");
                    yield error_frame(&stream_id, &model, error_message(&output));
                }
                _ => {}
            }
        }
    };

    Box::pin(stream)
}

/// Does this `result` output say the turn failed?
fn is_error_result(output: &ClaudeCodeOutput) -> bool {
    output.data.get("is_error").and_then(|v| v.as_bool()) == Some(true)
        || output.subtype.as_deref() == Some("error")
}

/// The most specific message a failed output carries, or a stand-in.
fn error_message(output: &ClaudeCodeOutput) -> String {
    for key in ["error", "message", "result"] {
        match output.data.get(key) {
            Some(Value::String(text)) if !text.is_empty() => return text.clone(),
            Some(Value::Object(object)) => {
                if let Some(Value::String(text)) = object.get("message") {
                    return text.clone();
                }
            },
            _ => {},
        }
    }
    format!("the CLI reported a failed `{}` output", output.r#type)
}

/// One in-band error frame: no choices, no `finish_reason`, an `error` object.
fn error_frame(stream_id: &str, model: &str, message: String) -> ChatCompletionStreamResponse {
    let mut chunk = ChatCompletionStreamResponse::empty(stream_id.to_string(), model.to_string());
    chunk.error = Some(StreamError::upstream(message));
    chunk
}

/// The turn's token counts, as OpenAI spells them, from the CLI's `usage` object.
///
/// `prompt_tokens` is everything the model read, cache included — that is what an
/// OpenAI client bills against. `None` when the CLI reported no count at all:
/// unknown is not zero.
fn usage_of(output: &ClaudeCodeOutput) -> Option<Usage> {
    let usage = output.data.get("usage")?;
    let count = |key: &str| usage.get(key).and_then(|v| v.as_i64());
    let input = count("input_tokens");
    let output_tokens = count("output_tokens");
    if input.is_none() && output_tokens.is_none() {
        return None;
    }
    let prompt_tokens = (input.unwrap_or(0)
        + count("cache_read_input_tokens").unwrap_or(0)
        + count("cache_creation_input_tokens").unwrap_or(0)) as i32;
    let completion_tokens = output_tokens.unwrap_or(0) as i32;
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
    })
}

/// Configuration for streaming enhancement
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct StreamingConfig {
    /// Whether to enable text chunking
    pub enable_chunking: bool,
    /// Chunk configuration
    pub chunk_config: ChunkConfig,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            enable_chunking: true,
            chunk_config: ChunkConfig::default(),
        }
    }
}
