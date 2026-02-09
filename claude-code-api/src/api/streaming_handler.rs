//! Enhanced streaming handler with real chunking support

use crate::{
    models::{
        claude::ClaudeCodeOutput,
        openai::{
            ChatCompletionStreamResponse, DeltaFunctionCall, DeltaMessage, DeltaToolCall,
            StreamChoice,
        },
    },
    utils::text_chunker::{ChunkConfig, chunk_text},
};
use chrono::Utc;
use futures::stream::{Stream, StreamExt};
use std::pin::Pin;
use tokio::sync::mpsc;
use tracing::debug;
use uuid::Uuid;

/// Handle streaming response with text chunking for better UX
pub async fn handle_enhanced_streaming_response(
    model: String,
    mut rx: mpsc::Receiver<ClaudeCodeOutput>,
) -> Pin<Box<dyn Stream<Item = ChatCompletionStreamResponse> + Send>> {
    let stream = async_stream::stream! {
        let stream_id = Uuid::new_v4().to_string();

        // First, send the initial message with role
        yield ChatCompletionStreamResponse {
            id: stream_id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: Utc::now().timestamp(),
            model: model.clone(),
            choices: vec![StreamChoice {
                index: 0,
                delta: DeltaMessage {
                    role: Some("assistant".to_string()),
                    content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
        };

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
                                            yield ChatCompletionStreamResponse {
                                                id: stream_id.clone(),
                                                object: "chat.completion.chunk".to_string(),
                                                created: Utc::now().timestamp(),
                                                model: model.clone(),
                                                choices: vec![StreamChoice {
                                                    index: 0,
                                                    delta: DeltaMessage {
                                                        role: None,
                                                        content: Some(chunk),
                                                        tool_calls: None,
                                                    },
                                                    finish_reason: None,
                                                }],
                                            };
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
                                    yield ChatCompletionStreamResponse {
                                        id: stream_id.clone(),
                                        object: "chat.completion.chunk".to_string(),
                                        created: Utc::now().timestamp(),
                                        model: model.clone(),
                                        choices: vec![StreamChoice {
                                            index: 0,
                                            delta: DeltaMessage {
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
                                            finish_reason: None,
                                        }],
                                    };

                                    tool_call_index += 1;
                                },
                                _ => {
                                    debug!("Streaming: ignoring content block type: {}", block_type);
                                }
                            }
                        }
                    }
                }
                "result" => {
                    // Send the final chunk with finish_reason
                    yield ChatCompletionStreamResponse {
                        id: stream_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created: Utc::now().timestamp(),
                        model: model.clone(),
                        choices: vec![StreamChoice {
                            index: 0,
                            delta: DeltaMessage::default(),
                            finish_reason: Some("stop".to_string()),
                        }],
                    };
                }
                _ => {}
            }
        }
    };

    Box::pin(stream)
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
