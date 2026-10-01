use axum::{
    Json,
    extract::{Path, State},
    response::IntoResponse,
};
use chrono::Utc;
use futures::stream::{Stream, StreamExt};
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use uuid::Uuid;

use crate::{
    api::streaming_handler::handle_enhanced_streaming_response,
    core::{claude_manager::ClaudeManager, conversation::DefaultConversationManager},
    models::{
        claude::ClaudeCodeOutput,
        error::{ApiError, ApiResult},
        openai::{
            ChatChoice, ChatCompletionRequest, ChatCompletionResponse,
            ChatCompletionStreamResponse, ChatMessage, MessageContent, Usage,
        },
    },
    utils::streaming::create_sse_stream,
};
use once_cell::sync::Lazy;
use parking_lot::Mutex;

type TempFileEntry = (String, std::time::Instant);
type TempFileStore = Arc<Mutex<Vec<TempFileEntry>>>;

/// A tracked temp image is deleted once it is older than this, in seconds.
const TEMP_FILE_TTL_SECS: u64 = 900;
/// How often the sweeper wakes up, in seconds.
const TEMP_FILE_SWEEP_SECS: u64 = 300;

static TEMP_FILES: Lazy<TempFileStore> = Lazy::new(|| {
    let tracker = Arc::new(Mutex::new(Vec::new()));
    let tracker_clone = tracker.clone();
    tokio::spawn(async move {
        cleanup_temp_files(tracker_clone).await;
    });
    tracker
});

async fn cleanup_temp_files(tracker: TempFileStore) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(TEMP_FILE_SWEEP_SECS)).await; // 每5分钟检查一次
        prune_temp_files(&tracker, std::time::Instant::now());
    }
}

/// Delete every tracked file older than [`TEMP_FILE_TTL_SECS`] at `now` and stop
/// tracking it.
///
/// Split out of [`cleanup_temp_files`] so the retention policy can be exercised
/// without waiting on the five-minute timer. A file that cannot be removed is
/// logged and dropped from the tracker anyway, so a deleted-by-someone-else path
/// is not retried forever.
fn prune_temp_files(tracker: &TempFileStore, now: std::time::Instant) {
    let mut files = tracker.lock();

    files.retain(|(path, created)| {
        if now.duration_since(*created).as_secs() > TEMP_FILE_TTL_SECS {
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

    // A client-supplied conversation must be checked *here*: the CLI turn below
    // is billed, and an unknown id would otherwise only surface when
    // `add_message` fails afterwards — as a 500, after the money was spent.
    let conversation_id = if let Some(ref conv_id) = request.conversation_id {
        if state
            .conversation_manager
            .get_conversation(conv_id)
            .await
            .is_none()
        {
            return Err(ApiError::NotFound("Conversation not found".to_string()));
        }
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
        if let Some(mut cached_response) = state.cache.get(&cache_key) {
            info!("Returning cached response");
            // The cache key is (model, messages) only, so a hit can come from
            // another caller's turn. Replaying its `conversation_id` would hand
            // this client someone else's conversation to read and append to.
            cached_response.conversation_id = Some(conversation_id.clone());
            return Ok(axum::Json(cached_response).into_response());
        }
    }

    let formatted_message = format_messages_for_claude(&context_messages).await?;

    // 根据配置选择使用交互式会话管理器或进程池
    let (session_id, rx) = if state.use_interactive_sessions {
        // 使用交互式会话管理器复用进程.
        // The session key *must* be the conversation id handed back to the
        // client: it is what `POST /v1/sessions/:id/interrupt` and the SSE
        // disconnect guard look up. Passing `request.conversation_id` here let
        // `get_or_create_session_and_send` mint its own UUID whenever the client
        // sent none, and the session became unreachable.
        state
            .interactive_session_manager
            .get_or_create_session_and_send(
                Some(conversation_id.clone()),
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
        // Persist the client turn before handing the channel to the streamer:
        // the stream never comes back to this function, so this is the only
        // place where a streamed conversation can record what was asked.
        for msg in &request.messages {
            state
                .conversation_manager
                .add_message(&conversation_id, msg.clone())
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
        }

        Ok(handle_streaming_response(
            request.model,
            rx,
            state.interactive_session_manager.clone(),
            state.conversation_manager.clone(),
            conversation_id.clone(),
        )
        .await
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

/// What a client-supplied `image_url` is allowed to be.
///
/// Anything else is refused: see [`classify_image_url`].
#[derive(Debug, PartialEq, Eq)]
enum ImageSource {
    /// The base64 payload of a `data:image/...` URL.
    Inline(String),
    /// An `http(s)` URL the gateway may fetch itself.
    Remote(reqwest::Url),
}

/// Decide what to do with a client-supplied `image_url`, refusing everything the
/// gateway must not touch.
///
/// The gateway resolves these URLs **server side** and hands the resulting path
/// to the CLI, so the input is attacker-controlled in two dangerous ways:
///
/// * any fetchable scheme or host turns the gateway into a server-side request
///   forgery proxy — `http://169.254.169.254/latest/meta-data/` is the cloud
///   credential endpoint, and `http://127.0.0.1:…` is every unauthenticated
///   admin port on the box;
/// * anything that is not recognised must **not** be passed through as a
///   filesystem path, or `Image: /etc/shadow` ends up in the prompt and the CLI
///   reads it.
///
/// Only two shapes are therefore accepted: `data:image/…,<base64>` and an
/// `http(s)` URL whose host is a globally routable address or a public domain
/// name. Name resolution is *not* re-checked after this point, so a domain that
/// resolves to a private address (DNS rebinding) still gets through — that needs
/// a resolving connector on the HTTP client, which this crate does not build.
fn classify_image_url(url: &str) -> ApiResult<ImageSource> {
    if url.starts_with("data:image/") {
        let parts: Vec<&str> = url.split(',').collect();
        if parts.len() != 2 {
            return Err(ApiError::BadRequest("Invalid data URL format".to_string()));
        }
        return Ok(ImageSource::Inline(parts[1].to_string()));
    }

    let parsed = reqwest::Url::parse(url).map_err(|_| {
        ApiError::BadRequest("image_url must be a data:image/... URL or an http(s) URL".to_string())
    })?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::BadRequest(format!(
            "image_url scheme {:?} is not allowed; use data:image/... or http(s)",
            parsed.scheme()
        )));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| ApiError::BadRequest("image_url has no host to fetch from".to_string()))?;

    if is_blocked_image_host(host) {
        return Err(ApiError::BadRequest(
            "image_url host is not reachable from this gateway".to_string(),
        ));
    }

    Ok(ImageSource::Remote(parsed))
}

/// Hosts the gateway refuses to fetch from: loopback, link-local (cloud metadata
/// lives at `169.254.169.254`), private and otherwise non-routable addresses, and
/// the hostnames that resolve to them by convention.
fn is_blocked_image_host(host: &str) -> bool {
    let literal = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    if let Ok(ip) = literal.parse::<IpAddr>() {
        return !is_globally_routable(ip);
    }

    let name = host.trim_end_matches('.').to_ascii_lowercase();
    // A single-label host is never a public name: it resolves through the local
    // search domain, which is how `metadata` (GCP) and `instance-data` (AWS) reach
    // the credential endpoint.
    !name.contains('.')
        || name == "localhost"
        || name.ends_with(".localhost")
        || name.ends_with(".localdomain")
        || name.ends_with(".local")
        || name.ends_with(".internal")
}

/// Whether `ip` is an address that exists on the public internet.
///
/// Deliberately conservative: `Ipv4Addr::is_global` is still unstable, so the
/// reserved ranges that matter for SSRF are spelled out here.
fn is_globally_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && (b == 18 || b == 19))
                || a >= 240)
        },
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_globally_routable(IpAddr::V4(mapped));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xffc0) == 0xfe80
                || (first & 0xfe00) == 0xfc00)
        },
    }
}

async fn process_image_url(url: &str) -> ApiResult<String> {
    use base64::{Engine as _, engine::general_purpose};

    match classify_image_url(url)? {
        ImageSource::Inline(base64_data) => {
            let image_data = general_purpose::STANDARD
                .decode(base64_data)
                .map_err(|e| ApiError::BadRequest(format!("Invalid base64 data: {e}")))?;
            persist_temp_image(&std::env::temp_dir(), &image_data)
        },
        ImageSource::Remote(url) => download_image(url).await,
    }
}

/// Write `image_data` to a tracked file in `temp_dir` and return its path.
///
/// The path is registered in [`TEMP_FILES`] so the sweeper deletes it later; the
/// caller only ever sees a path it can hand to the CLI. `temp_dir` is a parameter
/// so the failure branch is reachable without touching the process environment.
fn persist_temp_image(temp_dir: &std::path::Path, image_data: &[u8]) -> ApiResult<String> {
    use std::io::Write;

    let file_name = format!("claude_image_{}.png", Uuid::new_v4());
    let file_path = temp_dir.join(&file_name);

    let mut file = std::fs::File::create(&file_path)
        .map_err(|e| ApiError::Internal(format!("Failed to create temp file: {e}")))?;

    file.write_all(image_data)
        .map_err(|e| ApiError::Internal(format!("Failed to write image data: {e}")))?;

    let path_string = file_path.to_string_lossy().to_string();

    TEMP_FILES
        .lock()
        .push((path_string.clone(), std::time::Instant::now()));

    Ok(path_string)
}

/// Fetch an image the gateway has already cleared through [`classify_image_url`].
///
/// Takes a parsed [`reqwest::Url`] rather than a string precisely so it cannot be
/// reached with an unvalidated host.
async fn download_image(url: reqwest::Url) -> ApiResult<String> {
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

    persist_temp_image(&std::env::temp_dir(), &bytes)
}

/// Build the SSE response for a streamed turn.
///
/// Infallible on purpose: the signature used to advertise `ApiResult` while every
/// path returned `Ok`, which made the `?` at the call site dead code.
async fn handle_streaming_response(
    model: String,
    rx: mpsc::Receiver<ClaudeCodeOutput>,
    session_manager: Arc<crate::core::interactive_session::InteractiveSessionManager>,
    conversation_manager: Arc<DefaultConversationManager>,
    conversation_id: String,
) -> impl IntoResponse {
    // Use enhanced streaming with text chunking for better UX.
    // Pass session_manager + conversation_id so the disconnect guard
    // can auto-interrupt the CLI if the SSE client drops the connection.
    let stream = handle_enhanced_streaming_response(
        model,
        rx,
        Some(session_manager),
        Some(conversation_id.clone()),
    )
    .await;

    create_sse_stream(record_streamed_turn(
        stream,
        conversation_manager,
        conversation_id,
    ))
}

/// Forward every chunk to the client, accumulating the assistant text, and append
/// the finished turn to the conversation once the stream ends.
///
/// Nothing else in the streaming path calls `add_message`: before this, a
/// conversation driven with `"stream": true` accumulated no history at all and
/// `get_context_messages` had nothing to replay on the next turn. A client that
/// disconnects mid-stream still records nothing — the generator is dropped before
/// it gets here — and a storage failure is logged without truncating the stream
/// the client is already reading.
fn record_streamed_turn(
    mut stream: Pin<Box<dyn Stream<Item = ChatCompletionStreamResponse> + Send>>,
    conversation_manager: Arc<DefaultConversationManager>,
    conversation_id: String,
) -> impl Stream<Item = ChatCompletionStreamResponse> + Send {
    async_stream::stream! {
        let mut assistant_text = String::new();

        while let Some(chunk) = stream.next().await {
            for choice in &chunk.choices {
                if let Some(content) = choice.delta.content.as_deref() {
                    assistant_text.push_str(content);
                }
            }
            yield chunk;
        }

        if !assistant_text.is_empty() {
            let message = ChatMessage {
                role: "assistant".to_string(),
                content: Some(MessageContent::Text(assistant_text)),
                name: None,
                tool_calls: None,
            };
            if let Err(e) = conversation_manager
                .add_message(&conversation_id, message)
                .await
            {
                error!(
                    "Failed to record streamed turn for conversation {}: {}",
                    conversation_id, e
                );
            }
        }
    }
}

/// Longest slice of the budget spent in a single `recv`, so a slow CLI still
/// produces a progress log every five seconds.
const RECV_POLL_SLICE: tokio::time::Duration = tokio::time::Duration::from_secs(5);

async fn handle_non_streaming_response(
    model: String,
    mut rx: mpsc::Receiver<ClaudeCodeOutput>,
    session_id: String,
    claude_manager: Arc<ClaudeManager>,
    timeout_seconds: u64,
    requested_tools: Option<Vec<crate::models::openai::Tool>>,
) -> ApiResult<Json<ChatCompletionResponse>> {
    use crate::models::openai::{FunctionCall, ToolCall};
    use tokio::time::{Duration, Instant, timeout};

    let mut full_content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut token_count = 0;

    info!(
        "Waiting for Claude response (timeout: {}s)...",
        timeout_seconds
    );

    // The deadline is read from the same clock as `timeout` below. Comparing a
    // `std::time::Instant` against 5-second slices made `timeout_seconds` a
    // multiple of 5 in practice: a 1-second budget held the connection for 5.
    //
    // The budget is clamped because `Instant + Duration` panics on overflow, and a
    // 136-year timeout means the same thing as the largest one we can represent.
    let start = Instant::now();
    let deadline = start + Duration::from_secs(timeout_seconds.min(u32::MAX as u64));

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match timeout(remaining.min(RECV_POLL_SLICE), rx.recv()).await {
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
                if Instant::now() >= deadline {
                    error!(
                        "Timeout waiting for Claude response after {:?}",
                        start.elapsed()
                    );
                    // Only reaches the processes owned by ClaudeManager, i.e. the
                    // ProcessPool path. An interactive session lives in
                    // InteractiveSessionManager.sessions and is left to the
                    // inactivity sweep.
                    let _ = claude_manager.close_session(&session_id).await;
                    return Err(ApiError::ClaudeProcess(format!(
                        "Timeout waiting for response after {} seconds",
                        timeout_seconds
                    )));
                }
                info!(
                    "No data received in {:?}, but still waiting... (elapsed: {:?})",
                    RECV_POLL_SLICE,
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
mod tests {
    use super::*;
    use crate::core::config::{FileAccessConfig, MCPConfig};
    use crate::core::conversation::{ConversationConfig, ConversationManager};
    use crate::core::storage::InMemoryConversationStore;
    use crate::models::openai::{ContentPart, FunctionDefinition, ImageUrl, Tool};
    use serde_json::json;
    use std::time::{Duration as StdDuration, Instant as StdInstant};

    // ── builders ───────────────────────────────────────────────────────────

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: Some(MessageContent::Text(text.to_string())),
            name: None,
            tool_calls: None,
        }
    }

    fn parts_msg(role: &str, parts: Vec<ContentPart>) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: Some(MessageContent::Array(parts)),
            name: None,
            tool_calls: None,
        }
    }

    fn image_part(url: &str) -> ContentPart {
        ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: url.to_string(),
                detail: None,
            },
        }
    }

    fn text_of(message: &ChatMessage) -> Option<&str> {
        match message.content.as_ref()? {
            MessageContent::Text(text) => Some(text.as_str()),
            MessageContent::Array(_) => None,
        }
    }

    /// A `ClaudeManager` whose command cannot be spawned: every test here feeds
    /// the channel directly, and `close_session` on an unknown id is a no-op.
    fn claude_manager() -> Arc<ClaudeManager> {
        Arc::new(ClaudeManager::new(
            "nexus-unit-test-no-such-cli".to_string(),
            FileAccessConfig {
                skip_permissions: false,
                additional_dirs: Vec::new(),
            },
            MCPConfig::default(),
        ))
    }

    fn conversation_manager() -> Arc<DefaultConversationManager> {
        Arc::new(ConversationManager::new(
            InMemoryConversationStore::default(),
            ConversationConfig::default(),
        ))
    }

    fn assistant_blocks(blocks: serde_json::Value) -> ClaudeCodeOutput {
        ClaudeCodeOutput {
            r#type: "assistant".to_string(),
            subtype: None,
            data: json!({"message": {"role": "assistant", "content": blocks}}),
        }
    }

    fn assistant_text(text: &str) -> ClaudeCodeOutput {
        assistant_blocks(json!([{"type": "text", "text": text}]))
    }

    fn result_success() -> ClaudeCodeOutput {
        ClaudeCodeOutput {
            r#type: "result".to_string(),
            subtype: Some("success".to_string()),
            data: json!({"is_error": false}),
        }
    }

    /// A closed receiver pre-loaded with `outputs` — the CLI stand-in.
    fn channel(outputs: Vec<ClaudeCodeOutput>) -> mpsc::Receiver<ClaudeCodeOutput> {
        let (tx, rx) = mpsc::channel(outputs.len().max(1));
        for output in outputs {
            tx.try_send(output).expect("preload the CLI channel");
        }
        rx
    }

    async fn complete(
        rx: mpsc::Receiver<ClaudeCodeOutput>,
        tools: Option<Vec<Tool>>,
    ) -> ApiResult<ChatCompletionResponse> {
        handle_non_streaming_response(
            "claude-sonnet-5".to_string(),
            rx,
            "session-under-test".to_string(),
            claude_manager(),
            5,
            tools,
        )
        .await
        .map(|json| json.0)
    }

    /// Make the `DEBUG` level live for the whole test binary.
    ///
    /// `tracing` compares the level against a *global* maximum before touching the
    /// callsite, so with no subscriber registered the arguments of `info!` and
    /// `debug!` are never evaluated: `output.parent_tool_use_id()`,
    /// `full_content.len()` and `start.elapsed()` are dead code in a test process,
    /// while they always run in production, where `main` installs a subscriber. The
    /// tests that walk those statements call this first so the whole statement is
    /// exercised — a panicking or expensive argument included.
    ///
    /// A thread-local `set_default` is not enough: it leaves the global maximum at
    /// `OFF`, which is exactly the short-circuit we are trying to defeat. And the
    /// global maximum is only recomputed when a callsite registers, so a test that
    /// logged before the subscriber existed latches it at `OFF` for the rest of the
    /// process — hence the explicit `rebuild_interest_cache`.
    fn enable_debug_logs() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = tracing::subscriber::set_global_default(
                tracing_subscriber::FmtSubscriber::builder()
                    .with_max_level(tracing::Level::DEBUG)
                    .with_test_writer()
                    .finish(),
            );
            tracing::callsite::rebuild_interest_cache();
        });
        assert!(
            tracing::level_filters::LevelFilter::current() >= tracing::Level::DEBUG,
            "the DEBUG level must be live, or the log statements under test are skipped"
        );
    }

    /// Proof that [`enable_debug_logs`] does what the rest of this module assumes,
    /// and that the argument lines of the multi-line `info!`/`debug!` statements
    /// above are an `llvm-cov` region artifact rather than dead code.
    ///
    /// `cargo llvm-cov` reports every *function call* used as an argument of a
    /// multi-line `tracing` macro as uncovered — `full_content.len()`,
    /// `start.elapsed()`, `message.0.content.is_some()` — while the same call on a
    /// single-line macro is counted. This test takes the same path and asserts the
    /// side effect: the argument is evaluated exactly once.
    #[test]
    fn log_arguments_really_are_evaluated_when_the_level_is_live() {
        static EVALUATIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        fn probe() -> usize {
            EVALUATIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
        }

        enable_debug_logs();
        debug!("probe for the coverage of log arguments: {}", probe());

        assert_eq!(
            EVALUATIONS.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a DEBUG argument must be evaluated once when a DEBUG subscriber is live"
        );
    }

    fn weather_tool() -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "get_weather".to_string(),
                description: None,
                parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
            },
        }
    }

    // ── image_url policy: the SSRF / local-file-read refusals ──────────────

    /// Every shape the gateway must refuse. The two dangerous families are the
    /// server-side fetch of an internal address and the "it is not a URL, so it
    /// must be a path" fallback that used to hand `/etc/passwd` to the CLI.
    #[test]
    fn classify_image_url_refuses_internal_hosts_schemes_and_paths() {
        let refused = [
            // Cloud metadata — the credential endpoint on every major cloud.
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            "http://metadata.google.internal/computeMetadata/v1/instance/",
            "http://metadata/computeMetadata/v1/",
            "http://instance-data/latest/user-data",
            // Loopback and the rest of the private estate.
            "http://127.0.0.1:8080/admin",
            "https://localhost/x.png",
            "http://LOCALHOST./x.png",
            "http://nexus.local/x.png",
            "http://db.internal/x.png",
            "http://[::1]/x.png",
            "http://[::ffff:127.0.0.1]/x.png",
            "http://10.0.0.5/x.png",
            "http://172.16.0.1/x.png",
            "http://192.168.1.1/x.png",
            "http://[fd00::1]/x.png",
            "http://[fe80::1]/x.png",
            "http://0.0.0.0/x.png",
            "http://100.64.0.1/x.png",
            "http://192.0.0.1/x.png",
            "http://198.18.0.1/x.png",
            "http://240.0.0.1/x.png",
            "http://255.255.255.255/x.png",
            "http://224.0.0.1/x.png",
            "http://192.0.2.1/x.png",
            // Other schemes, and plain filesystem paths.
            "file:///etc/passwd",
            "ftp://example.com/x.png",
            "data:text/html;base64,PHNjcmlwdD4=",
            "/etc/passwd",
            "../../../etc/shadow",
            "C:\\Windows\\win.ini",
            "pas une url",
            "",
        ];

        for url in refused {
            assert!(
                matches!(classify_image_url(url), Err(ApiError::BadRequest(_))),
                "{url:?} must be refused with a 400"
            );
        }
    }

    #[test]
    fn classify_image_url_accepts_public_http_and_data_images() {
        match classify_image_url("data:image/png;base64,bmV4dXM=") {
            Ok(ImageSource::Inline(payload)) => assert_eq!(payload, "bmV4dXM="),
            other => panic!("a data URL must stay inline, got {other:?}"),
        }

        for url in [
            "https://example.com/cat.png",
            "http://93.184.216.34/cat.png",
        ] {
            match classify_image_url(url) {
                Ok(ImageSource::Remote(parsed)) => assert_eq!(parsed.as_str(), url),
                other => panic!("{url} must be fetchable, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_data_url_must_have_exactly_one_comma() {
        let error = classify_image_url("data:image/png;base64,aa,bb")
            .expect_err("two commas is not a data URL");
        assert!(
            matches!(error, ApiError::BadRequest(ref m) if m == "Invalid data URL format"),
            "unexpected error: {error}"
        );
    }

    /// The whole point of the refusal: a path never becomes an `Image:` line in
    /// the prompt, so a caller cannot make the CLI read a local file.
    #[tokio::test]
    async fn a_local_path_is_never_turned_into_an_image() {
        let error = process_image_url("/etc/passwd")
            .await
            .expect_err("a filesystem path is not an image_url");
        match error {
            ApiError::BadRequest(message) => assert!(
                message.contains("data:image/"),
                "the message must say what is accepted, got {message:?}"
            ),
            other => panic!("expected a 400, got {other}"),
        }
    }

    #[tokio::test]
    async fn the_cloud_metadata_endpoint_is_not_fetched() {
        let error = process_image_url("http://169.254.169.254/latest/meta-data/")
            .await
            .expect_err("the metadata endpoint must never be fetched server side");
        assert!(matches!(error, ApiError::BadRequest(_)), "got {error}");
    }

    #[tokio::test]
    async fn a_data_url_image_is_decoded_into_a_tracked_temp_file() {
        let path = process_image_url("data:image/png;base64,bmV4dXM=")
            .await
            .expect("a well-formed data URL");

        assert_eq!(std::fs::read(&path).expect("the file exists"), b"nexus");
        assert!(
            TEMP_FILES.lock().iter().any(|(p, _)| *p == path),
            "the temp file must be tracked so the sweeper deletes it"
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn a_data_url_with_broken_base64_is_a_bad_request() {
        let error = process_image_url("data:image/png;base64,!!!not-base64!!!")
            .await
            .expect_err("invalid base64 must not reach the filesystem");
        match error {
            ApiError::BadRequest(message) => {
                assert!(message.starts_with("Invalid base64 data:"), "{message}")
            },
            other => panic!("expected a 400, got {other}"),
        }
    }

    #[test]
    fn persist_temp_image_reports_an_unwritable_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");

        let error =
            persist_temp_image(&missing, b"x").expect_err("a missing directory cannot be written");
        match error {
            ApiError::Internal(message) => {
                assert!(
                    message.starts_with("Failed to create temp file:"),
                    "{message}"
                )
            },
            other => panic!("expected a 500, got {other}"),
        }
    }

    // ── the temp-file sweeper ──────────────────────────────────────────────

    #[test]
    fn prune_temp_files_deletes_only_what_aged_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stale = dir.path().join("stale.png");
        let fresh = dir.path().join("fresh.png");
        std::fs::write(&stale, b"vieux").expect("write stale");
        std::fs::write(&fresh, b"neuf").expect("write fresh");
        let vanished = dir.path().join("deleted-by-someone-else.png");

        let born = StdInstant::now();
        let tracker: TempFileStore = Arc::new(Mutex::new(vec![
            (stale.to_string_lossy().into_owned(), born),
            (vanished.to_string_lossy().into_owned(), born),
            (
                fresh.to_string_lossy().into_owned(),
                born + StdDuration::from_secs(600),
            ),
        ]));

        // The age test is on whole seconds, so the TTL itself is still young.
        prune_temp_files(&tracker, born + StdDuration::from_secs(TEMP_FILE_TTL_SECS));
        assert_eq!(
            tracker.lock().len(),
            3,
            "exactly at the TTL nothing expires"
        );
        assert!(stale.exists());

        prune_temp_files(
            &tracker,
            born + StdDuration::from_secs(TEMP_FILE_TTL_SECS + 1),
        );
        assert!(!stale.exists(), "an aged-out file must be deleted");
        assert!(fresh.exists(), "a younger file must survive the sweep");
        assert_eq!(
            tracker.lock().len(),
            1,
            "an aged-out entry is dropped even when the file was already gone"
        );
    }

    /// The background sweeper wakes up every five minutes and must not delete a
    /// file that is still within its TTL. Driven on a paused clock: no real
    /// five-minute wait, and the task is aborted before the test returns.
    #[tokio::test(start_paused = true)]
    async fn the_sweeper_keeps_files_that_are_still_young() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = dir.path().join("live.png");
        std::fs::write(&live, b"encore utile").expect("write live");

        let tracker: TempFileStore = Arc::new(Mutex::new(vec![(
            live.to_string_lossy().into_owned(),
            StdInstant::now(),
        )]));

        let sweeper = tokio::spawn(cleanup_temp_files(tracker.clone()));
        // Let the task arm its timer first: advancing a clock nobody is waiting on
        // would make the assertion below vacuous.
        tokio::task::yield_now().await;
        tokio::time::advance(tokio::time::Duration::from_secs(TEMP_FILE_SWEEP_SECS + 1)).await;
        tokio::task::yield_now().await;

        assert_eq!(tracker.lock().len(), 1, "a young file survives a sweep");
        assert!(live.exists());
        sweeper.abort();
    }

    // ── download_image: reached only with a host the policy cleared ─────────

    /// `download_image` is driven directly here: `classify_image_url` refuses the
    /// loopback interface, so the only way to exercise the fetch itself is to hand
    /// it a URL the policy would never produce. That asymmetry *is* the fix.
    #[tokio::test]
    async fn download_image_writes_the_body_to_a_tracked_temp_file() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cat.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"PNG-OCTETS".to_vec()))
            .mount(&server)
            .await;

        let url = reqwest::Url::parse(&format!("{}/cat.png", server.uri())).expect("mock url");
        let saved = download_image(url).await.expect("the mock answers 200");

        assert_eq!(
            std::fs::read(&saved).expect("downloaded file"),
            b"PNG-OCTETS"
        );
        assert!(TEMP_FILES.lock().iter().any(|(p, _)| *p == saved));
        std::fs::remove_file(&saved).ok();
    }

    #[tokio::test]
    async fn a_non_success_status_is_a_bad_request_not_a_server_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let url = reqwest::Url::parse(&format!("{}/missing.png", server.uri())).expect("mock url");
        let error = download_image(url).await.expect_err("404 is not an image");
        match error {
            ApiError::BadRequest(message) => {
                assert_eq!(message, "Failed to download image: HTTP 404 Not Found")
            },
            other => panic!("a remote 404 must be the caller's fault, got {other}"),
        }
    }

    #[tokio::test]
    async fn an_unreachable_server_is_an_internal_error() {
        // Nothing listens on port 1 of the loopback interface: no DNS lookup, no
        // packet leaves the machine, and the connection is refused immediately.
        let url = reqwest::Url::parse("http://127.0.0.1:1/cat.png").expect("literal url");
        let error = download_image(url).await.expect_err("connection refused");
        match error {
            ApiError::Internal(message) => {
                assert!(
                    message.starts_with("Failed to download image:"),
                    "{message}"
                )
            },
            other => panic!("expected a 500, got {other}"),
        }
    }

    // ── prompt assembly ────────────────────────────────────────────────────

    #[tokio::test]
    async fn every_turn_but_the_last_is_prefixed_with_its_role() {
        let formatted = format_messages_for_claude(&[
            msg("user", "un"),
            msg("assistant", "deux"),
            msg("system", "trois"),
            msg("user", "quatre"),
        ])
        .await
        .expect("plain text messages");

        assert_eq!(
            formatted, "User: un\nAssistant: deux\nSystem: trois\nquatre",
            "the last turn is the actual prompt and carries no prefix"
        );
    }

    /// `role: "tool"` is what every OpenAI client sends back after a tool call.
    /// The `_ => {}` arm drops it: the gateway accepts the request and silently
    /// forgets the tool result instead of refusing it.
    #[tokio::test]
    async fn an_unknown_role_is_silently_dropped_from_the_prompt() {
        let formatted = format_messages_for_claude(&[
            msg("tool", "resultat de l outil"),
            msg("function", "autre resultat"),
            msg("user", "et donc ?"),
        ])
        .await
        .expect("unknown roles are accepted");

        assert_eq!(
            formatted, "et donc ?",
            "BUG: the content of a non user/assistant/system turn never reaches the CLI"
        );
    }

    #[tokio::test]
    async fn an_image_is_materialised_and_announced_after_the_text() {
        let formatted = format_messages_for_claude(&[
            parts_msg(
                "user",
                vec![
                    ContentPart::Text {
                        text: "regarde".to_string(),
                    },
                    ContentPart::Text {
                        text: "ceci".to_string(),
                    },
                    image_part("data:image/png;base64,bmV4dXM="),
                ],
            ),
            msg("user", "alors ?"),
        ])
        .await
        .expect("a data URL image");

        let path = formatted
            .lines()
            .find_map(|line| line.strip_prefix("Image: "))
            .expect("an Image: line naming the temp file");
        assert_eq!(
            formatted,
            format!("User: regarde ceci\n\nImage: {path}\n\nalors ?"),
            "text parts are joined with a space, then the image paths are appended"
        );
        assert_eq!(std::fs::read(path).expect("the image file"), b"nexus");
        std::fs::remove_file(path).ok();
    }

    #[tokio::test]
    async fn a_refused_image_fails_the_whole_prompt() {
        let error = format_messages_for_claude(&[parts_msg(
            "user",
            vec![image_part("file:///etc/passwd")],
        )])
        .await
        .expect_err("a refused image_url must abort the request");
        assert!(matches!(error, ApiError::BadRequest(_)), "got {error}");
    }

    #[tokio::test]
    async fn a_message_without_content_contributes_nothing() {
        let (text, images) = extract_content_and_images(&ChatMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_calls: None,
        })
        .await
        .expect("no content is legal for a tool-call turn");

        assert_eq!(text, "");
        assert!(images.is_empty());
    }

    // ── handle_non_streaming_response ──────────────────────────────────────

    #[tokio::test]
    async fn text_blocks_are_concatenated_and_words_counted() {
        enable_debug_logs();
        let response = complete(
            channel(vec![
                assistant_text("deux mots"),
                assistant_text(" encore"),
                result_success(),
            ]),
            None,
        )
        .await
        .expect("a complete transcript");

        let choice = &response.choices[0];
        assert_eq!(text_of(&choice.message), Some("deux mots encore"));
        assert_eq!(choice.finish_reason.as_deref(), Some("stop"));
        assert_eq!(choice.message.role, "assistant");
        assert!(choice.message.tool_calls.is_none());
        assert_eq!(response.usage.completion_tokens, 3);
        assert_eq!(response.usage.total_tokens, 3);
        assert_eq!(
            response.usage.prompt_tokens, 0,
            "the gateway never reports prompt tokens"
        );
        assert_eq!(response.object, "chat.completion");
        assert_eq!(response.model, "claude-sonnet-5");
        assert!(
            response.conversation_id.is_none(),
            "the conversation id is stamped by chat_completions, not here"
        );
    }

    #[tokio::test]
    async fn sidechains_tool_results_and_unknown_blocks_are_dropped() {
        enable_debug_logs();
        let sidechain = ClaudeCodeOutput {
            r#type: "assistant".to_string(),
            subtype: None,
            data: json!({
                "parent_tool_use_id": "toolu_sub",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "SECRET"}]},
            }),
        };

        let response = complete(
            channel(vec![
                sidechain,
                assistant_blocks(json!([
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "42"},
                    {"type": "thinking", "thinking": "reflexion interne"},
                    {"type": "text"},
                    {"type": "text", "text": "visible"},
                ])),
                ClaudeCodeOutput {
                    r#type: "system".to_string(),
                    subtype: None,
                    data: json!({"note": "ignored"}),
                },
                result_success(),
            ]),
            None,
        )
        .await
        .expect("a transcript full of noise");

        assert_eq!(text_of(&response.choices[0].message), Some("visible"));
        assert!(response.choices[0].message.tool_calls.is_none());
        assert_eq!(response.usage.completion_tokens, 1);
    }

    #[tokio::test]
    async fn an_assistant_message_without_content_blocks_is_ignored() {
        let response = complete(
            channel(vec![
                ClaudeCodeOutput {
                    r#type: "assistant".to_string(),
                    subtype: None,
                    data: json!({"message": {"role": "assistant"}}),
                },
                result_success(),
            ]),
            None,
        )
        .await
        .expect("a malformed assistant message must not abort the turn");

        assert_eq!(text_of(&response.choices[0].message), Some(""));
    }

    #[tokio::test]
    async fn a_tool_use_without_an_id_gets_a_generated_one() {
        let response = complete(
            channel(vec![
                assistant_blocks(json!([{"type": "tool_use", "name": "search"}])),
                result_success(),
            ]),
            None,
        )
        .await
        .expect("a tool_use block");

        let choice = &response.choices[0];
        assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
        assert!(
            choice.message.content.is_none(),
            "a bare tool call carries no content"
        );
        let calls = choice.message.tool_calls.as_ref().expect("tool_calls");
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].id.starts_with("call_"),
            "an id-less tool_use must get a synthetic id, got {:?}",
            calls[0].id
        );
        assert_eq!(calls[0].tool_type, "function");
        assert_eq!(calls[0].function.name, "search");
        assert_eq!(
            calls[0].function.arguments, "{}",
            "a missing input becomes an empty object"
        );
    }

    #[tokio::test]
    async fn text_next_to_a_tool_call_keeps_finish_reason_stop() {
        let response = complete(
            channel(vec![
                assistant_blocks(json!([
                    {"type": "text", "text": "je cherche"},
                    {"type": "tool_use", "id": "toolu_9", "name": "search", "input": {"q": "nexus"}},
                ])),
                result_success(),
            ]),
            None,
        )
        .await
        .expect("text and a tool call in one message");

        let choice = &response.choices[0];
        assert_eq!(
            choice.finish_reason.as_deref(),
            Some("stop"),
            "OpenAI clients read finish_reason to decide whether to run the tool"
        );
        assert_eq!(text_of(&choice.message), Some("je cherche"));
        let calls = choice.message.tool_calls.as_ref().expect("tool_calls");
        assert_eq!(calls[0].id, "toolu_9");
        assert_eq!(calls[0].function.arguments, r#"{"q":"nexus"}"#);
    }

    /// Legacy path: no structural `tool_use`, but the CLI answered with bare JSON
    /// and the client declared a tool. The heuristic converts it — and drops the
    /// text that produced it.
    #[tokio::test]
    async fn a_json_answer_is_converted_to_a_tool_call_when_tools_were_requested() {
        let response = complete(
            channel(vec![
                assistant_text(r#"{"city": "Lyon"}"#),
                result_success(),
            ]),
            Some(vec![weather_tool()]),
        )
        .await
        .expect("a JSON answer");

        let choice = &response.choices[0];
        assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
        assert!(
            choice.message.content.is_none(),
            "the raw JSON text is dropped"
        );
        let calls = choice.message.tool_calls.as_ref().expect("tool_calls");
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[0].function.arguments, r#"{"city":"Lyon"}"#);
        assert!(calls[0].id.starts_with("call_"));
    }

    #[tokio::test]
    async fn the_same_json_without_declared_tools_stays_text() {
        let response = complete(
            channel(vec![
                assistant_text(r#"{"city": "Lyon"}"#),
                result_success(),
            ]),
            None,
        )
        .await
        .expect("a JSON answer with no tools declared");

        assert_eq!(
            response.choices[0].finish_reason.as_deref(),
            Some("stop"),
            "without declared tools the heuristic must stay out of the way"
        );
        assert_eq!(
            text_of(&response.choices[0].message),
            Some(r#"{"city": "Lyon"}"#)
        );
    }

    /// Bug: the receive loop polled in fixed five-second slices and only then
    /// compared the elapsed wall clock against `claude.timeout_seconds`, so a
    /// one-second budget held the connection for five seconds. Replayed on a
    /// paused clock, the assertion below is exact — and fails with `5s` on the
    /// previous implementation.
    #[tokio::test(start_paused = true)]
    async fn the_configured_timeout_is_honoured_to_the_second() {
        enable_debug_logs();
        // The sender stays alive and silent: the CLI never answers.
        let (_tx, rx) = mpsc::channel(1);
        let started = tokio::time::Instant::now();

        let error = handle_non_streaming_response(
            "claude-sonnet-5".to_string(),
            rx,
            "session-under-test".to_string(),
            claude_manager(),
            1,
            None,
        )
        .await
        .expect_err("a silent CLI must time out");

        assert_eq!(
            error.to_string(),
            "Claude process error: Timeout waiting for response after 1 seconds"
        );
        assert_eq!(
            started.elapsed(),
            tokio::time::Duration::from_secs(1),
            "a 1 s budget must not cost a whole 5 s poll slice"
        );
    }

    /// The same loop with a budget longer than the poll slice: one full slice is
    /// spent logging progress, then the remainder.
    #[tokio::test(start_paused = true)]
    async fn a_budget_longer_than_the_poll_slice_still_ends_on_time() {
        enable_debug_logs();
        let (_tx, rx) = mpsc::channel(1);
        let started = tokio::time::Instant::now();

        let error = handle_non_streaming_response(
            "claude-sonnet-5".to_string(),
            rx,
            "session-under-test".to_string(),
            claude_manager(),
            7,
            None,
        )
        .await
        .expect_err("a silent CLI must time out");

        assert!(error.to_string().contains("after 7 seconds"), "{error}");
        assert_eq!(started.elapsed(), tokio::time::Duration::from_secs(7));
    }

    /// A budget nobody can wait for must not take the process down: the deadline
    /// is computed with `Instant + Duration`, which panics on overflow.
    #[tokio::test]
    async fn an_absurd_timeout_does_not_overflow_the_deadline() {
        let response = handle_non_streaming_response(
            "claude-sonnet-5".to_string(),
            channel(vec![assistant_text("toujours la"), result_success()]),
            "session-under-test".to_string(),
            claude_manager(),
            u64::MAX,
            None,
        )
        .await
        .expect("an unbounded budget still answers when the CLI does");

        assert_eq!(text_of(&response.0.choices[0].message), Some("toujours la"));
    }

    /// BUG (`format_messages_for_claude`): a role the gateway does not know loses
    /// its content entirely.
    ///
    /// Triggering input: a turn with `role: "tool"` — what every OpenAI client
    /// sends back after a tool call. The `match message.role.as_str()` has arms for
    /// `user`, `assistant` and `system` only, and its `_ => {}` drops the rest.
    /// Both transports go through this function, so the streaming path loses the
    /// same content.
    ///
    /// Expected: the tool result reaches the prompt in some shape.
    /// Actual: the CLI is asked the question without the answer it depends on.
    /// Not fixed here: deciding what prefix a `tool`/`function` turn deserves
    /// changes the prompt every tool-using client sends, which is a contract
    /// decision rather than a local repair.
    #[tokio::test]
    #[ignore = "documents a bug: an unknown role loses its content on the way to the CLI"]
    async fn a_tool_result_turn_should_reach_the_prompt() {
        let formatted = format_messages_for_claude(&[
            msg("tool", "la temperature est de 42"),
            msg("user", "et donc ?"),
        ])
        .await
        .expect("unknown roles are accepted");

        assert!(
            formatted.contains("42"),
            "the tool result must reach the CLI, got {formatted:?}"
        );
    }

    /// BUG (`handle_non_streaming_response`): a `result` message flagged
    /// `is_error` is handled exactly like a success.
    ///
    /// Triggering input: the synthetic event
    /// `core::interactive_session::build_process_died_event` emits when the CLI
    /// process dies — `{"type": "result", "subtype": "process_died", "is_error":
    /// true, ...}`. The `"result"` arm only logs, and the `_` arm ignores a
    /// top-level `"error"` output just as silently.
    ///
    /// Expected: `ApiError::ClaudeProcess` (500 claude_process_error) carrying the
    /// CLI's error text. Actual: `200 OK` with empty content, indistinguishable
    /// from a model that chose to say nothing.
    /// Not fixed here: the same silent handling sits in
    /// `api::streaming_handler::handle_enhanced_streaming_response`, which another
    /// owner holds, and both sites should change in one go — with one decision on
    /// the status code.
    #[tokio::test]
    #[ignore = "documents a bug: an errored result is served as a successful empty completion"]
    async fn an_errored_result_should_not_be_a_200() {
        let error = complete(
            channel(vec![ClaudeCodeOutput {
                r#type: "result".to_string(),
                subtype: Some("process_died".to_string()),
                data: json!({"is_error": true, "error": "CLI process terminated unexpectedly"}),
            }]),
            None,
        )
        .await
        .expect_err("a failed turn must not be a successful completion");

        assert!(
            error.to_string().contains("terminated unexpectedly"),
            "the CLI error text must reach the client, got {error}"
        );
    }

    // ── streaming: the turn is recorded on the way out ─────────────────────

    #[tokio::test]
    async fn a_streamed_turn_is_appended_to_the_conversation() {
        let manager = conversation_manager();
        let id = manager
            .create_conversation(Some("claude-sonnet-5".to_string()))
            .await
            .expect("in-memory store");

        let stream = handle_enhanced_streaming_response(
            "claude-sonnet-5".to_string(),
            channel(vec![assistant_text("bonjour Nexus"), result_success()]),
            None,
            None,
        )
        .await;

        let chunks: Vec<ChatCompletionStreamResponse> =
            Box::pin(record_streamed_turn(stream, manager.clone(), id.clone()))
                .collect()
                .await;
        let streamed: String = chunks
            .iter()
            .flat_map(|chunk| chunk.choices.iter())
            .filter_map(|choice| choice.delta.content.as_deref())
            .collect();
        assert_eq!(
            streamed, "bonjour Nexus",
            "the client still sees every chunk"
        );

        let conversation = manager
            .get_conversation(&id)
            .await
            .expect("the conversation");
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(conversation.messages[0].role, "assistant");
        assert_eq!(
            text_of(&conversation.messages[0]),
            Some("bonjour Nexus"),
            "the streamed answer must be replayable as context on the next turn"
        );
    }

    #[tokio::test]
    async fn a_stream_without_content_records_nothing() {
        let manager = conversation_manager();
        let id = manager.create_conversation(None).await.expect("store");

        let stream = handle_enhanced_streaming_response(
            "claude-sonnet-5".to_string(),
            channel(vec![result_success()]),
            None,
            None,
        )
        .await;
        let chunks: Vec<ChatCompletionStreamResponse> =
            Box::pin(record_streamed_turn(stream, manager.clone(), id.clone()))
                .collect()
                .await;

        assert!(!chunks.is_empty(), "the role chunk is always sent");
        assert!(
            manager
                .get_conversation(&id)
                .await
                .expect("the conversation")
                .messages
                .is_empty(),
            "an empty answer must not be stored as an empty turn"
        );
    }

    #[tokio::test]
    async fn a_storage_failure_never_truncates_the_stream() {
        let manager = conversation_manager();

        let stream = handle_enhanced_streaming_response(
            "claude-sonnet-5".to_string(),
            channel(vec![assistant_text("quand meme"), result_success()]),
            None,
            None,
        )
        .await;
        let chunks: Vec<ChatCompletionStreamResponse> = Box::pin(record_streamed_turn(
            stream,
            manager.clone(),
            "conversation-absente".to_string(),
        ))
        .collect()
        .await;

        let streamed: String = chunks
            .iter()
            .flat_map(|chunk| chunk.choices.iter())
            .filter_map(|choice| choice.delta.content.as_deref())
            .collect();
        assert_eq!(
            streamed, "quand meme",
            "a conversation that cannot be written must not cost the client its answer"
        );
        assert!(
            manager
                .get_conversation("conversation-absente")
                .await
                .is_none()
        );
    }
}
