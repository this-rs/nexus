use axum::{
    Json,
    extract::{Path, State},
    response::IntoResponse,
};
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
                Json(
                    serde_json::json!({"status": "interrupted", "conversation_id": conversation_id}),
                ),
            )
        },
        Ok(false) => {
            info!("Session not found for interrupt: {}", conversation_id);
            (
                axum::http::StatusCode::NOT_FOUND,
                Json(
                    serde_json::json!({"error": "session not found", "conversation_id": conversation_id}),
                ),
            )
        },
        Err(e) => {
            error!("Failed to interrupt session {}: {}", conversation_id, e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::json!({"error": e.to_string(), "conversation_id": conversation_id}),
                ),
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
        // Anything else used to be returned verbatim and handed to the CLI as
        // `Image: <path>`, which let a caller name any file the CLI could read.
        // A client string is never a path.
        Err(ApiError::BadRequest(
            "image_url must be a data:image/ URL or an http(s) URL".to_string(),
        ))
    }
}

/// Whether an address may be fetched on behalf of a caller.
///
/// The gateway fetches `image_url` itself, so every address the server can
/// reach but the caller cannot is a confused-deputy hazard: loopback, the
/// private ranges, and above all the link-local block that carries cloud
/// instance metadata at `169.254.169.254`. Only globally routable addresses
/// are allowed through.
///
/// `IpAddr::is_global` is still unstable, so the ranges are spelled out here
/// rather than waiting for it.
fn is_publicly_routable(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                // 100.64.0.0/10, carrier-grade NAT.
                || (a == 100 && (64..128).contains(&b))
                // 198.18.0.0/15, benchmarking.
                || (a == 198 && (18..20).contains(&b))
                // 240.0.0.0/4, reserved.
                || a >= 240)
        },
        IpAddr::V6(v6) => {
            // An IPv4-mapped address is an IPv4 address wearing a hat; judge
            // the address it actually reaches.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_publicly_routable(IpAddr::V4(mapped));
            }
            let first = v6.segments()[0];
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                // fc00::/7, unique local.
                || (first & 0xfe00) == 0xfc00
                // fe80::/10, link local.
                || (first & 0xffc0) == 0xfe80)
        },
    }
}

/// Resolve `url` and refuse it unless every address it reaches is public.
///
/// Every resolved address is checked, not just the first: a name that returns
/// one public and one loopback address must not be fetchable.
///
/// This narrows the hole rather than sealing it. The name is resolved here and
/// resolved again by the HTTP client, so a DNS entry that changes between the
/// two still slips through (DNS rebinding). Closing that needs the connection
/// pinned to the address checked here, which is a `reqwest` connector change
/// and a larger piece of work than this fix; it is recorded as such in
/// `docs/BUGS.md` rather than left implied.
async fn refuse_unless_public(url: &str) -> ApiResult<()> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| ApiError::BadRequest(format!("Invalid image URL: {e}")))?;

    match parsed.scheme() {
        "http" | "https" => {},
        other => {
            return Err(ApiError::BadRequest(format!(
                "image_url scheme {other:?} is not allowed"
            )));
        },
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| ApiError::BadRequest("image_url has no host".to_string()))?;
    let port = parsed.port_or_known_default().unwrap_or(80);

    // A literal address resolves without touching DNS, which is what keeps the
    // tests for this guard offline.
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| ApiError::BadRequest(format!("Cannot resolve image host: {e}")))?;

    let mut saw_one = false;
    for addr in addrs {
        saw_one = true;
        if !is_publicly_routable(addr.ip()) {
            return Err(ApiError::BadRequest(
                "image_url resolves to a non-public address".to_string(),
            ));
        }
    }
    if !saw_one {
        return Err(ApiError::BadRequest(
            "image_url host resolves to no address".to_string(),
        ));
    }
    Ok(())
}

async fn download_image(url: &str) -> ApiResult<String> {
    use reqwest;
    use std::io::Write;

    // Before any request leaves the process.
    refuse_unless_public(url).await?;

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
    let stream =
        handle_enhanced_streaming_response(model, rx, Some(session_manager), Some(conversation_id))
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

#[cfg(test)]
mod image_url_guard_tests {
    use super::*;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test address parses")
    }

    // -- the address classifier -------------------------------------------

    #[test]
    fn cloud_metadata_and_the_private_ranges_are_not_publicly_routable() {
        // 169.254.169.254 is the address this guard exists for: it serves
        // instance credentials on every major cloud.
        for blocked in [
            "169.254.169.254",
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1", // carrier-grade NAT
            "198.18.0.1", // benchmarking
            "240.0.0.1",  // reserved
            "255.255.255.255",
            "224.0.0.1", // multicast
        ] {
            assert!(
                !is_publicly_routable(ip(blocked)),
                "{blocked} must not be publicly routable"
            );
        }
    }

    #[test]
    fn ipv6_loopback_unique_local_and_link_local_are_not_publicly_routable() {
        for blocked in ["::", "::1", "fc00::1", "fd12:3456::1", "fe80::1", "ff02::1"] {
            assert!(
                !is_publicly_routable(ip(blocked)),
                "{blocked} must not be publicly routable"
            );
        }
    }

    #[test]
    fn an_ipv4_mapped_ipv6_address_is_judged_by_the_address_it_reaches() {
        // ::ffff:169.254.169.254 reaches the metadata service just as well as
        // the bare v4 address; checking only the v6 shape would wave it past.
        assert!(!is_publicly_routable(ip("::ffff:169.254.169.254")));
        assert!(!is_publicly_routable(ip("::ffff:127.0.0.1")));
        assert!(is_publicly_routable(ip("::ffff:93.184.216.34")));
    }

    #[test]
    fn ordinary_public_addresses_still_pass() {
        for allowed in [
            "93.184.216.34",
            "8.8.8.8",
            "172.32.0.1",
            "2606:2800:220:1::1",
        ] {
            assert!(
                is_publicly_routable(ip(allowed)),
                "{allowed} must remain fetchable"
            );
        }
    }

    // -- the guard, end to end, without a network -------------------------
    //
    // Every URL below uses a literal address, so `lookup_host` answers from
    // the string and no DNS query or HTTP request is made.

    #[tokio::test]
    async fn the_metadata_address_is_refused_before_any_request() {
        let err = refuse_unless_public("http://169.254.169.254/latest/meta-data/")
            .await
            .expect_err("the metadata service must be refused");
        assert!(
            matches!(err, ApiError::BadRequest(ref m) if m.contains("non-public")),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn loopback_is_refused_whatever_the_port() {
        for url in [
            "http://127.0.0.1/admin",
            "http://127.0.0.1:9200/_cluster/health",
            "https://[::1]:8080/",
        ] {
            assert!(
                refuse_unless_public(url).await.is_err(),
                "{url} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn a_non_http_scheme_is_refused() {
        let err = refuse_unless_public("file:///etc/passwd")
            .await
            .expect_err("file:// must be refused");
        assert!(
            matches!(err, ApiError::BadRequest(ref m) if m.contains("not allowed")),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn a_public_literal_address_passes_the_guard() {
        // Proves the guard is not simply refusing everything, which is the way
        // a check like this silently stops being a check.
        refuse_unless_public("http://93.184.216.34/image.png")
            .await
            .expect("a public address must pass the guard");
    }

    #[tokio::test]
    async fn download_image_consults_the_guard_before_fetching() {
        // The tests above prove the guard is correct; this one proves it is
        // WIRED. Without the call in `download_image`, reqwest attempts the
        // connection and the error becomes `Internal("Failed to download
        // image: ...")` instead of a refusal, so the distinction is asserted
        // rather than just `is_err()`.
        //
        // Port 1 on loopback is refused instantly by the OS, so the unfixed
        // path fails fast rather than hanging this test.
        let err = process_image_url("http://127.0.0.1:1/image.png")
            .await
            .expect_err("a loopback target must never be fetched");
        match err {
            ApiError::BadRequest(ref m) if m.contains("non-public") => {},
            other => panic!(
                "expected a refusal from the guard, got {other:?} — \
                 the guard is not wired into download_image"
            ),
        }
    }

    // -- the branch that treated a client string as a path ----------------

    #[tokio::test]
    async fn an_unrecognised_image_url_is_refused_rather_than_read_as_a_path() {
        // This is the local-file-read bug: these used to be returned verbatim
        // and injected into the prompt as `Image: <path>`.
        for hostile in [
            "/etc/passwd",
            "../../../../etc/shadow",
            "file:///etc/passwd",
            "~/.ssh/id_rsa",
            "C:\\Windows\\win.ini",
        ] {
            let result = process_image_url(hostile).await;
            assert!(result.is_err(), "{hostile} must be refused, got {result:?}");
        }
    }

    #[tokio::test]
    async fn a_data_url_still_works() {
        // 1x1 transparent PNG; the fix must not break the supported path.
        let url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
        let path = process_image_url(url)
            .await
            .expect("data URLs stay supported");
        assert!(
            std::path::Path::new(&path).exists(),
            "the decoded image should be on disk at {path}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
