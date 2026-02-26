use axum::{Json, extract::{Path, State}, response::IntoResponse};
use chrono::Utc;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use uuid::Uuid;

use crate::{
    api::streaming_handler::handle_enhanced_streaming_response,
    core::claude_manager::ClaudeManager,
    models::{
        claude::ClaudeCodeOutput,
        error::{ApiError, ApiResult},
        openai::{
            ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, MessageContent,
            Usage,
        },
    },
    utils::streaming::create_sse_stream,
};
use once_cell::sync::Lazy;
use parking_lot::Mutex;

type TempFileEntry = (String, std::time::Instant);
type TempFileStore = Arc<Mutex<Vec<TempFileEntry>>>;

static TEMP_FILES: Lazy<TempFileStore> = Lazy::new(|| {
    let tracker = Arc::new(Mutex::new(Vec::new()));
    let tracker_clone = tracker.clone();
    tokio::spawn(async move {
        cleanup_temp_files(tracker_clone).await;
    });
    tracker
});

async fn cleanup_temp_files(tracker: Arc<Mutex<Vec<(String, std::time::Instant)>>>) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(300)).await; // 每5分钟检查一次

        let mut files = tracker.lock();
        let now = std::time::Instant::now();

        files.retain(|(path, created)| {
            if now.duration_since(*created).as_secs() > 900 {
                if let Err(e) = std::fs::remove_file(path) {
                    error!("Failed to remove temp file {}: {}", path, e);
                } else {
                    info!("Cleaned up temp file: {}", path);
                }
                false
            } else {
                true
            }
        });
    }
}

#[derive(Clone)]
pub struct ChatState {
    pub claude_manager: Arc<ClaudeManager>,
    pub process_pool: Arc<crate::core::process_pool::ProcessPool>,
    pub interactive_session_manager:
        Arc<crate::core::interactive_session::InteractiveSessionManager>,
    pub conversation_manager: Arc<crate::core::conversation::DefaultConversationManager>,
    pub cache: Arc<crate::core::cache::ResponseCache>,
    pub use_interactive_sessions: bool,
    pub settings: Arc<crate::core::config::Settings>,
}

impl ChatState {
    pub fn new(
        claude_manager: Arc<ClaudeManager>,
        process_pool: Arc<crate::core::process_pool::ProcessPool>,
        interactive_session_manager: Arc<
            crate::core::interactive_session::InteractiveSessionManager,
        >,
        conversation_manager: Arc<crate::core::conversation::DefaultConversationManager>,
        cache: Arc<crate::core::cache::ResponseCache>,
        use_interactive_sessions: bool,
        settings: Arc<crate::core::config::Settings>,
    ) -> Self {
        Self {
            claude_manager,
            process_pool,
            interactive_session_manager,
            conversation_manager,
            cache,
            use_interactive_sessions,
            settings,
        }
    }
}

pub async fn chat_completions(
    State(state): State<ChatState>,
    Json(request): Json<ChatCompletionRequest>,
) -> ApiResult<impl IntoResponse> {
    use crate::core::cache::ResponseCache;

    info!(
        "Received chat completion request for model: {}",
        request.model
    );

    if request.messages.is_empty() {
        return Err(ApiError::BadRequest("Messages cannot be empty".to_string()));
    }

    let conversation_id = if let Some(ref conv_id) = request.conversation_id {
        conv_id.clone()
    } else {
        state
            .conversation_manager
            .create_conversation(Some(request.model.clone()))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
    };

    let context_messages = state
        .conversation_manager
        .get_context_messages(&conversation_id, &request.messages)
        .await;

    if !request.stream.unwrap_or(false) {
        let cache_key = ResponseCache::generate_key(&request.model, &context_messages);
        if let Some(cached_response) = state.cache.get(&cache_key) {
            info!("Returning cached response");
            return Ok(axum::Json(cached_response).into_response());
        }
    }

    let formatted_message = format_messages_for_claude(&context_messages).await?;

    // 根据配置选择使用交互式会话管理器或进程池
    let (session_id, rx) = if state.use_interactive_sessions {
        // 使用交互式会话管理器复用进程
        state
            .interactive_session_manager
            .get_or_create_session_and_send(
                request.conversation_id.clone(),
                request.model.clone(),
                formatted_message,
            )
            .await
            .map_err(|e| ApiError::ClaudeProcess(e.to_string()))?
    } else {
        // 使用进程池
        state
            .process_pool
            .get_or_create(request.model.clone(), formatted_message)
            .await
            .map_err(|e| ApiError::ClaudeProcess(e.to_string()))?
    };

    if request.stream.unwrap_or(false) {
        Ok(handle_streaming_response(
            request.model,
            rx,
            state.interactive_session_manager.clone(),
            conversation_id.clone(),
        )
        .await?
        .into_response())
    } else {
        let cache_key = ResponseCache::generate_key(&request.model, &context_messages);
        let response = handle_non_streaming_response(
            request.model.clone(),
            rx,
            session_id,
            state.claude_manager.clone(),
            state.settings.claude.timeout_seconds,
            request.tools.clone(),
        )
        .await?;

        for msg in &request.messages {
            state
                .conversation_manager
                .add_message(&conversation_id, msg.clone())
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
        }

        if let Some(choice) = response.0.choices.first() {
            state
                .conversation_manager
                .add_message(&conversation_id, choice.message.clone())
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
        }

        let mut response_data = response.0;
        response_data.conversation_id = Some(conversation_id.clone());

        state.cache.put(cache_key.clone(), response_data.clone());

        Ok(Json(response_data).into_response())
    }
}

/// Interrupt the active request in an interactive session.
///
/// `POST /v1/sessions/:conversation_id/interrupt`
///
/// Sends a control_request interrupt to the CLI process without closing the
/// session. Returns 200 if the interrupt was sent, 404 if no session exists.
pub async fn interrupt_session(
    Path(conversation_id): Path<String>,
    State(state): State<ChatState>,
) -> impl IntoResponse {
    info!(
        "Received interrupt request for session: {}",
        conversation_id
    );

    match state
        .interactive_session_manager
        .interrupt_session(&conversation_id)
    {
        Ok(true) => {
            info!("Session interrupted: {}", conversation_id);
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({"status": "interrupted", "conversation_id": conversation_id})),
            )
        },
        Ok(false) => {
            info!("Session not found for interrupt: {}", conversation_id);
            (
                axum::http::StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "session not found", "conversation_id": conversation_id})),
            )
        },
        Err(e) => {
            error!("Failed to interrupt session {}: {}", conversation_id, e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string(), "conversation_id": conversation_id})),
            )
        },
    }
}

async fn format_messages_for_claude(messages: &[ChatMessage]) -> ApiResult<String> {
    let mut conversation = String::new();
    let mut all_image_paths = Vec::new();

    for (i, message) in messages.iter().enumerate() {
        let (mut content, msg_images) = extract_content_and_images(message).await?;

        if !msg_images.is_empty() {
            content.push_str("\n\n");
            for path in &msg_images {
                content.push_str(&format!("Image: {path}\n"));
            }
            all_image_paths.extend(msg_images);
        }

        if i == messages.len() - 1 {
            conversation.push_str(&content);
        } else {
            match message.role.as_str() {
                "user" => conversation.push_str(&format!("User: {content}\n")),
                "assistant" => conversation.push_str(&format!("Assistant: {content}\n")),
                "system" => conversation.push_str(&format!("System: {content}\n")),
                _ => {},
            }
        }
    }

    Ok(conversation)
}

async fn extract_content_and_images(message: &ChatMessage) -> ApiResult<(String, Vec<String>)> {
    let mut text_parts = Vec::new();
    let mut image_paths = Vec::new();

    match &message.content {
        Some(MessageContent::Text(text)) => {
            text_parts.push(text.clone());
        },
        Some(MessageContent::Array(parts)) => {
            for part in parts {
                match part {
                    crate::models::openai::ContentPart::Text { text } => {
                        text_parts.push(text.clone());
                    },
                    crate::models::openai::ContentPart::ImageUrl { image_url } => {
                        let path = process_image_url(&image_url.url).await?;
                        image_paths.push(path);
                    },
                }
            }
        },
        None => {
            // No content, which is valid for function calls
        },
    }

    Ok((text_parts.join(" "), image_paths))
}

async fn process_image_url(url: &str) -> ApiResult<String> {
    use base64::{Engine as _, engine::general_purpose};
    use std::io::Write;

    if url.starts_with("data:image/") {
        let parts: Vec<&str> = url.split(',').collect();
        if parts.len() != 2 {
            return Err(ApiError::BadRequest("Invalid data URL format".to_string()));
        }

        let base64_data = parts[1];
        let image_data = general_purpose::STANDARD
            .decode(base64_data)
            .map_err(|e| ApiError::BadRequest(format!("Invalid base64 data: {e}")))?;

        let temp_dir = std::env::temp_dir();
        let file_name = format!("claude_image_{}.png", Uuid::new_v4());
        let file_path = temp_dir.join(&file_name);

        let mut file = std::fs::File::create(&file_path)
            .map_err(|e| ApiError::Internal(format!("Failed to create temp file: {e}")))?;

        file.write_all(&image_data)
            .map_err(|e| ApiError::Internal(format!("Failed to write image data: {e}")))?;

        let path_string = file_path.to_string_lossy().to_string();

        TEMP_FILES
            .lock()
            .push((path_string.clone(), std::time::Instant::now()));

        Ok(path_string)
    } else if url.starts_with("http://") || url.starts_with("https://") {
        download_image(url).await
    } else {
        Ok(url.to_string())
    }
}

async fn download_image(url: &str) -> ApiResult<String> {
    use reqwest;
    use std::io::Write;

    let response = reqwest::get(url)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to download image: {e}")))?;

    if !response.status().is_success() {
        return Err(ApiError::BadRequest(format!(
            "Failed to download image: HTTP {}",
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to read image data: {e}")))?;

    let temp_dir = std::env::temp_dir();
    let file_name = format!("claude_image_{}.png", Uuid::new_v4());
    let file_path = temp_dir.join(&file_name);

    let mut file = std::fs::File::create(&file_path)
        .map_err(|e| ApiError::Internal(format!("Failed to create temp file: {e}")))?;

    file.write_all(&bytes)
        .map_err(|e| ApiError::Internal(format!("Failed to write image data: {e}")))?;

    let path_string = file_path.to_string_lossy().to_string();

    TEMP_FILES
        .lock()
        .push((path_string.clone(), std::time::Instant::now()));

    Ok(path_string)
}

async fn handle_streaming_response(
    model: String,
    rx: mpsc::Receiver<ClaudeCodeOutput>,
    session_manager: Arc<crate::core::interactive_session::InteractiveSessionManager>,
    conversation_id: String,
) -> ApiResult<impl IntoResponse> {
    // Use enhanced streaming with text chunking for better UX.
    // Pass session_manager + conversation_id so the disconnect guard
    // can auto-interrupt the CLI if the SSE client drops the connection.
    let stream = handle_enhanced_streaming_response(
        model,
        rx,
        Some(session_manager),
        Some(conversation_id),
    )
    .await;
    Ok(create_sse_stream(stream))
}

async fn handle_non_streaming_response(
    model: String,
    mut rx: mpsc::Receiver<ClaudeCodeOutput>,
    session_id: String,
    claude_manager: Arc<ClaudeManager>,
    timeout_seconds: u64,
    requested_tools: Option<Vec<crate::models::openai::Tool>>,
) -> ApiResult<Json<ChatCompletionResponse>> {
    use crate::models::openai::{FunctionCall, ToolCall};
    use tokio::time::{Duration, timeout};

    let mut full_content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut token_count = 0;

    info!(
        "Waiting for Claude response (timeout: {}s)...",
        timeout_seconds
    );

    let timeout_duration = Duration::from_secs(timeout_seconds);
    let start = std::time::Instant::now();

    loop {
        match timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(output)) => {
                // Skip messages from subagent sidechains (Task tool executions).
                // Only top-level messages (parent_tool_use_id == None) should be
                // accumulated into the response content.
                if output.is_sidechain() {
                    debug!(
                        "Skipping sidechain message (parent_tool_use_id: {:?})",
                        output.parent_tool_use_id()
                    );
                    continue;
                }

                info!("Received output from Claude (type: {})", output.r#type);

                match output.r#type.as_str() {
                    "assistant" => {
                        // Parse content blocks structurally from the assistant message
                        if let Some(message) = output.data.get("message")
                            && let Some(content_array) =
                                message.get("content").and_then(|c| c.as_array())
                        {
                            for content_block in content_array {
                                let block_type = content_block
                                    .get("type")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("");

                                match block_type {
                                    "text" => {
                                        if let Some(text) =
                                            content_block.get("text").and_then(|t| t.as_str())
                                        {
                                            full_content.push_str(text);
                                            token_count += text.split_whitespace().count() as i32;
                                        }
                                    },
                                    "tool_use" => {
                                        // Extract tool_use structurally → OpenAI ToolCall
                                        let tool_id = content_block
                                            .get("id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let tool_name = content_block
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let tool_input = content_block
                                            .get("input")
                                            .cloned()
                                            .unwrap_or(serde_json::json!({}));

                                        info!(
                                            "Extracted tool_use: id={}, name={}",
                                            tool_id, tool_name
                                        );

                                        tool_calls.push(ToolCall {
                                            id: if tool_id.is_empty() {
                                                format!("call_{}", uuid::Uuid::new_v4())
                                            } else {
                                                tool_id
                                            },
                                            tool_type: "function".to_string(),
                                            function: FunctionCall {
                                                name: tool_name,
                                                arguments: tool_input.to_string(),
                                            },
                                        });
                                    },
                                    "tool_result" => {
                                        // Tool results are informational — we log them but
                                        // they don't directly map to OpenAI response format
                                        // (OpenAI expects tool results as separate messages)
                                        debug!(
                                            "Received tool_result block (tool_use_id: {:?})",
                                            content_block.get("tool_use_id")
                                        );
                                    },
                                    _ => {
                                        debug!("Ignoring content block type: {}", block_type);
                                    },
                                }
                            }
                        }
                    },
                    "result" => {
                        // End of response
                        info!(
                            "Claude response complete, content_len={}, tool_calls={}",
                            full_content.len(),
                            tool_calls.len()
                        );
                    },
                    _ => {
                        debug!("Ignoring output type: {}", output.r#type);
                    },
                }
            },
            Ok(None) => {
                info!(
                    "Claude stream ended, total content length: {}, tool_calls: {}",
                    full_content.len(),
                    tool_calls.len()
                );
                break;
            },
            Err(_) => {
                if start.elapsed() > timeout_duration {
                    error!(
                        "Timeout waiting for Claude response after {:?}",
                        start.elapsed()
                    );
                    // Close the session to avoid EPIPE error
                    let _ = claude_manager.close_session(&session_id).await;
                    return Err(ApiError::ClaudeProcess(format!(
                        "Timeout waiting for response after {} seconds",
                        timeout_seconds
                    )));
                }
                info!(
                    "No data received in 5s, but still waiting... (elapsed: {:?})",
                    start.elapsed()
                );
            },
        }
    }

    let _ = claude_manager.close_session(&session_id).await;

    // Build the response message:
    // 1. If structural tool_calls were extracted, use them directly (preferred)
    // 2. Else, fall back to text heuristic detection (legacy path)
    // 3. Else, return plain text response
    let message = if !tool_calls.is_empty() {
        // Structural tool_calls extracted from content blocks — the reliable path
        info!("Returning {} structural tool call(s)", tool_calls.len());
        let finish = if full_content.is_empty() {
            "tool_calls"
        } else {
            "stop"
        };
        let content = if full_content.is_empty() {
            None
        } else {
            Some(MessageContent::Text(full_content))
        };
        (
            ChatMessage {
                role: "assistant".to_string(),
                content,
                name: None,
                tool_calls: Some(tool_calls),
            },
            finish,
        )
    } else if let Some(function_call) = crate::utils::function_calling::detect_and_convert_tool_call(
        &full_content,
        &requested_tools,
    ) {
        // Fallback: heuristic text detection for legacy tool call patterns
        info!("Fallback: detected tool call via text heuristic");
        let tool_call = ToolCall {
            id: format!("call_{}", uuid::Uuid::new_v4()),
            tool_type: "function".to_string(),
            function: function_call,
        };
        (
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                name: None,
                tool_calls: Some(vec![tool_call]),
            },
            "tool_calls",
        )
    } else {
        // Regular text response (no tool calls)
        (
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(MessageContent::Text(full_content)),
                name: None,
                tool_calls: None,
            },
            "stop",
        )
    };

    let response = ChatCompletionResponse {
        id: Uuid::new_v4().to_string(),
        object: "chat.completion".to_string(),
        created: Utc::now().timestamp(),
        model,
        choices: vec![ChatChoice {
            index: 0,
            message: message.0.clone(),
            finish_reason: Some(message.1.to_string()),
        }],
        usage: Usage {
            prompt_tokens: 0,
            completion_tokens: token_count,
            total_tokens: token_count,
        },
        conversation_id: None,
    };

    // Log the response for debugging
    info!(
        "Returning response with message: role={}, has_content={}, has_tool_calls={}, finish_reason={}",
        message.0.role,
        message.0.content.is_some(),
        message.0.tool_calls.is_some(),
        message.1
    );

    Ok(Json(response))
}
