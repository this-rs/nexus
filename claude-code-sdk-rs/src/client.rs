//! Interactive client for bidirectional communication with Claude
//!
//! This module provides the `ClaudeSDKClient` for interactive, stateful
//! conversations with Claude Code CLI.

use crate::{
    errors::{Result, SdkError},
    internal_query::Query,
    token_tracker::BudgetManager,
    transport::{InputMessage, SubprocessTransport, Transport},
    types::{ClaudeCodeOptions, ContentBlock, ControlRequest, ControlResponse, Message},
};
use futures::stream::{Stream, StreamExt};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info};

/// Client state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientState {
    /// Not connected
    Disconnected,
    /// Connected and ready
    Connected,
    /// Error state
    Error,
}

/// Interactive client for bidirectional communication with Claude
///
/// `ClaudeSDKClient` provides a stateful, interactive interface for communicating
/// with Claude Code CLI. Unlike the simple `query` function, this client supports:
///
/// - Bidirectional communication
/// - Multiple sessions
/// - Interrupt capabilities
/// - State management
/// - Follow-up messages based on responses
///
/// # Example
///
/// ```rust,no_run
/// use nexus_claude::{ClaudeSDKClient, ClaudeCodeOptions, Message, Result};
/// use futures::StreamExt;
///
/// #[tokio::main]
/// async fn main() -> Result<()> {
///     let options = ClaudeCodeOptions::builder()
///         .system_prompt("You are a helpful assistant")
///         .model("claude-3-opus-20240229")
///         .build();
///
///     let mut client = ClaudeSDKClient::new(options);
///
///     // Connect with initial prompt
///     client.connect(Some("Hello!".to_string())).await?;
///
///     // Receive initial response
///     let mut messages = client.receive_messages().await;
///     while let Some(msg) = messages.next().await {
///         match msg? {
///             Message::Result { .. } => break,
///             msg => println!("{:?}", msg),
///         }
///     }
///
///     // Send follow-up
///     client.send_request("What's 2 + 2?".to_string(), None).await?;
///
///     // Receive response
///     let mut messages = client.receive_messages().await;
///     while let Some(msg) = messages.next().await {
///         println!("{:?}", msg?);
///     }
///
///     // Disconnect
///     client.disconnect().await?;
///
///     Ok(())
/// }
/// ```
pub struct ClaudeSDKClient {
    /// Configuration options
    #[allow(dead_code)]
    options: ClaudeCodeOptions,
    /// Transport layer
    transport: Arc<Mutex<Box<dyn Transport + Send>>>,
    /// Internal query handler (when control protocol is enabled)
    query_handler: Option<Arc<Mutex<Query>>>,
    /// Client state
    state: Arc<RwLock<ClientState>>,
    /// Active sessions
    sessions: Arc<RwLock<HashMap<String, SessionData>>>,
    /// Message sender for current receiver
    message_tx: Arc<Mutex<Option<mpsc::Sender<Result<Message>>>>>,
    /// Message buffer for multiple receivers
    message_buffer: Arc<Mutex<Vec<Message>>>,
    /// Request counter
    request_counter: Arc<Mutex<u64>>,
    /// Budget manager for token tracking
    budget_manager: BudgetManager,
}

/// Session data
#[allow(dead_code)]
struct SessionData {
    /// Session ID
    id: String,
    /// Number of messages sent
    message_count: usize,
    /// Creation time
    created_at: std::time::Instant,
}

impl ClaudeSDKClient {
    /// Create a new client with the given options
    pub fn new(options: ClaudeCodeOptions) -> Self {
        // Set environment variable to indicate SDK usage
        unsafe {
            std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "sdk-rust");
        }

        let transport = match SubprocessTransport::new(options.clone()) {
            Ok(t) => t,
            Err(e) => {
                error!("Failed to create transport: {}", e);
                // Create with empty path, will fail on connect
                SubprocessTransport::with_cli_path(options.clone(), "")
            },
        };

        // Wrap transport in Arc for sharing
        let transport_arc: Arc<Mutex<Box<dyn Transport + Send>>> =
            Arc::new(Mutex::new(Box::new(transport)));

        Self::with_transport_internal(options, transport_arc)
    }

    /// Create a new client with a custom transport implementation
    ///
    /// This allows users to provide their own Transport implementation instead of
    /// using the default SubprocessTransport. Useful for testing, custom CLI paths,
    /// or alternative communication mechanisms.
    ///
    /// # Arguments
    ///
    /// * `options` - Configuration options for the client
    /// * `transport` - Custom transport implementation
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use nexus_claude::{ClaudeSDKClient, ClaudeCodeOptions, SubprocessTransport};
    /// # fn example() {
    /// let options = ClaudeCodeOptions::default();
    /// let transport = SubprocessTransport::with_cli_path(options.clone(), "/custom/path/claude-code");
    /// let client = ClaudeSDKClient::with_transport(options, Box::new(transport));
    /// # }
    /// ```
    pub fn with_transport(
        options: ClaudeCodeOptions,
        transport: Box<dyn Transport + Send>,
    ) -> Self {
        // Set environment variable to indicate SDK usage
        unsafe {
            std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "sdk-rust");
        }

        // Wrap transport in Arc for sharing
        let transport_arc: Arc<Mutex<Box<dyn Transport + Send>>> = Arc::new(Mutex::new(transport));

        Self::with_transport_internal(options, transport_arc)
    }

    /// Internal helper to construct client with pre-wrapped transport
    fn with_transport_internal(
        options: ClaudeCodeOptions,
        transport_arc: Arc<Mutex<Box<dyn Transport + Send>>>,
    ) -> Self {
        // Create query handler if control protocol features are enabled
        let query_handler = if options.can_use_tool.is_some()
            || options.hooks.is_some()
            || !options.mcp_servers.is_empty()
            || options.enable_file_checkpointing
        {
            // Extract SDK MCP server instances
            let sdk_mcp_servers: HashMap<String, Arc<dyn std::any::Any + Send + Sync>> = options
                .mcp_servers
                .iter()
                .filter_map(|(k, v)| {
                    // Only extract SDK type MCP servers
                    if let crate::types::McpServerConfig::Sdk { name: _, instance } = v {
                        Some((k.clone(), instance.clone()))
                    } else {
                        None
                    }
                })
                .collect();

            // Enable streaming mode when control protocol is active
            let is_streaming = options.can_use_tool.is_some()
                || options.hooks.is_some()
                || !sdk_mcp_servers.is_empty();

            let query = Query::new(
                transport_arc.clone(), // Share the same transport
                is_streaming,          // Enable streaming for control protocol
                options.can_use_tool.clone(),
                options.hooks.clone(),
                sdk_mcp_servers,
            );
            Some(Arc::new(Mutex::new(query)))
        } else {
            None
        };

        Self {
            options,
            transport: transport_arc,
            query_handler,
            state: Arc::new(RwLock::new(ClientState::Disconnected)),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            message_tx: Arc::new(Mutex::new(None)),
            message_buffer: Arc::new(Mutex::new(Vec::new())),
            request_counter: Arc::new(Mutex::new(0)),
            budget_manager: BudgetManager::new(),
        }
    }

    /// Connect to Claude CLI with an optional initial prompt
    pub async fn connect(&mut self, initial_prompt: Option<String>) -> Result<()> {
        // Check if already connected
        {
            let state = self.state.read().await;
            if *state == ClientState::Connected {
                return Ok(());
            }
        }

        // Connect transport
        {
            let mut transport = self.transport.lock().await;
            transport.connect().await?;
        }

        // Initialize query handler if present
        if let Some(ref query_handler) = self.query_handler {
            let mut handler = query_handler.lock().await;
            handler.start().await?;
            handler.initialize().await?;
            info!("Initialized SDK control protocol");
        }

        // Update state
        {
            let mut state = self.state.write().await;
            *state = ClientState::Connected;
        }

        info!("Connected to Claude CLI");

        // Start message receiver task (always needed for regular messages)
        self.start_message_receiver().await;

        // Send initial prompt if provided
        if let Some(prompt) = initial_prompt {
            self.send_request(prompt, None).await?;
        }

        Ok(())
    }

    /// Send a user message to Claude
    pub async fn send_user_message(&mut self, prompt: String) -> Result<()> {
        // Check connection
        {
            let state = self.state.read().await;
            if *state != ClientState::Connected {
                return Err(SdkError::InvalidState {
                    message: "Not connected".into(),
                });
            }
        }

        // Use default session ID
        let session_id = "default".to_string();

        // Update session data
        {
            let mut sessions = self.sessions.write().await;
            let session = sessions.entry(session_id.clone()).or_insert_with(|| {
                debug!("Creating new session: {}", session_id);
                SessionData {
                    id: session_id.clone(),
                    message_count: 0,
                    created_at: std::time::Instant::now(),
                }
            });
            session.message_count += 1;
        }

        // Create and send message
        let message = InputMessage::user(prompt, session_id.clone());

        {
            let mut transport = self.transport.lock().await;
            transport.send_message(message).await?;
        }

        debug!("Sent request to Claude");
        Ok(())
    }

    /// Send a request to Claude (alias for send_user_message with optional session_id)
    pub async fn send_request(
        &mut self,
        prompt: String,
        _session_id: Option<String>,
    ) -> Result<()> {
        // For now, ignore session_id and use send_user_message
        self.send_user_message(prompt).await
    }

    /// Receive messages from Claude
    ///
    /// Returns a stream of messages. The stream will end when a Result message
    /// is received or the connection is closed.
    pub async fn receive_messages(&mut self) -> impl Stream<Item = Result<Message>> + use<> {
        // Always use the regular message receiver
        // (Query handler shares the same transport and receives control messages separately)
        // Create a new channel for this receiver
        let (tx, rx) = mpsc::channel(100);

        // Get buffered messages and clear buffer
        let buffered_messages = {
            let mut buffer = self.message_buffer.lock().await;
            std::mem::take(&mut *buffer)
        };

        // Send buffered messages to the new receiver
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            for msg in buffered_messages {
                if tx_clone.send(Ok(msg)).await.is_err() {
                    break;
                }
            }
        });

        // Store the sender for the message receiver task
        {
            let mut message_tx = self.message_tx.lock().await;
            *message_tx = Some(tx);
        }

        ReceiverStream::new(rx)
    }

    /// Send an interrupt request
    pub async fn interrupt(&mut self) -> Result<()> {
        // Check connection
        {
            let state = self.state.read().await;
            if *state != ClientState::Connected {
                return Err(SdkError::InvalidState {
                    message: "Not connected".into(),
                });
            }
        }

        // If we have a query handler, use it
        if let Some(ref query_handler) = self.query_handler {
            let mut handler = query_handler.lock().await;
            return handler.interrupt().await;
        }

        // Otherwise use regular interrupt
        // Generate request ID
        let request_id = {
            let mut counter = self.request_counter.lock().await;
            *counter += 1;
            format!("interrupt_{}", *counter)
        };

        // Send interrupt request
        let request = ControlRequest::Interrupt {
            request_id: request_id.clone(),
        };

        {
            let mut transport = self.transport.lock().await;
            transport.send_control_request(request).await?;
        }

        info!("Sent interrupt request: {}", request_id);

        // Wait for acknowledgment (with timeout)
        let transport = self.transport.clone();
        let ack_task = tokio::spawn(async move {
            let mut transport = transport.lock().await;
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                transport.receive_control_response(),
            )
            .await
            {
                Ok(Ok(Some(ControlResponse::InterruptAck {
                    request_id: ack_id,
                    success,
                }))) => {
                    if ack_id == request_id && success {
                        Ok(())
                    } else {
                        Err(SdkError::ControlRequestError(
                            "Interrupt not acknowledged successfully".into(),
                        ))
                    }
                },
                Ok(Ok(None)) => Err(SdkError::ControlRequestError(
                    "No interrupt acknowledgment received".into(),
                )),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(SdkError::timeout(5)),
            }
        });

        ack_task
            .await
            .map_err(|_| SdkError::ControlRequestError("Interrupt task panicked".into()))?
    }

    /// Check if the client is connected
    pub async fn is_connected(&self) -> bool {
        let state = self.state.read().await;
        *state == ClientState::Connected
    }

    /// Get active session IDs
    pub async fn get_sessions(&self) -> Vec<String> {
        let sessions = self.sessions.read().await;
        sessions.keys().cloned().collect()
    }

    /// Receive messages until and including a ResultMessage
    ///
    /// This is a convenience method that collects all messages from a single response.
    /// It will automatically stop after receiving a ResultMessage.
    pub async fn receive_response(
        &mut self,
    ) -> Pin<Box<dyn Stream<Item = Result<Message>> + Send + '_>> {
        let mut messages = self.receive_messages().await;

        // Create a stream that stops after ResultMessage
        Box::pin(async_stream::stream! {
            while let Some(msg_result) = messages.next().await {
                match &msg_result {
                    Ok(Message::Result { .. }) => {
                        yield msg_result;
                        return;
                    }
                    _ => {
                        yield msg_result;
                    }
                }
            }
        })
    }

    /// Get server information
    ///
    /// Returns initialization information from the Claude Code server including:
    /// - Available commands
    /// - Current and available output styles
    /// - Server capabilities
    pub async fn get_server_info(&self) -> Option<serde_json::Value> {
        // If we have a query handler with control protocol, get from there
        if let Some(ref query_handler) = self.query_handler {
            let handler = query_handler.lock().await;
            if let Some(init_result) = handler.get_initialization_result() {
                return Some(init_result.clone());
            }
        }

        // Otherwise check message buffer for init message
        let buffer = self.message_buffer.lock().await;
        for msg in buffer.iter() {
            if let Message::System { subtype, data } = msg
                && subtype == "init"
            {
                return Some(data.clone());
            }
        }
        None
    }

    /// Get account information
    ///
    /// This method attempts to retrieve Claude account information through multiple methods:
    /// 1. From environment variable `ANTHROPIC_USER_EMAIL`
    /// 2. From Claude CLI config file (if accessible)
    /// 3. By querying the CLI with `/status` command (interactive mode)
    ///
    /// # Returns
    ///
    /// A string containing the account information, or an error if unavailable.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use nexus_claude::{ClaudeSDKClient, ClaudeCodeOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut client = ClaudeSDKClient::new(ClaudeCodeOptions::default());
    /// client.connect(None).await?;
    ///
    /// match client.get_account_info().await {
    ///     Ok(info) => println!("Account: {}", info),
    ///     Err(_) => println!("Account info not available"),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Note
    ///
    /// Account information may not always be available in SDK mode.
    /// Consider setting the `ANTHROPIC_USER_EMAIL` environment variable
    /// for reliable account identification.
    pub async fn get_account_info(&mut self) -> Result<String> {
        // Check connection
        {
            let state = self.state.read().await;
            if *state != ClientState::Connected {
                return Err(SdkError::InvalidState {
                    message: "Not connected. Call connect() first.".into(),
                });
            }
        }

        // Method 1: Check environment variable
        if let Ok(email) = std::env::var("ANTHROPIC_USER_EMAIL") {
            return Ok(format!("Email: {}", email));
        }

        // Method 2: Try reading from Claude config
        if let Some(config_info) = Self::read_claude_config().await {
            return Ok(config_info);
        }

        // Method 3: Try /status command (may not work in SDK mode)
        self.send_user_message("/status".to_string()).await?;

        let mut messages = self.receive_messages().await;
        let mut account_info = String::new();

        while let Some(msg_result) = messages.next().await {
            match msg_result? {
                Message::Assistant { message, .. } => {
                    for block in message.content {
                        if let ContentBlock::Text(text) = block {
                            account_info.push_str(&text.text);
                            account_info.push('\n');
                        }
                    }
                },
                Message::Result { .. } => break,
                _ => {},
            }
        }

        let trimmed = account_info.trim();

        // Check if we got actual status info or just a chat response
        if !trimmed.is_empty()
            && (trimmed.contains("account")
                || trimmed.contains("email")
                || trimmed.contains("subscription")
                || trimmed.contains("authenticated"))
        {
            return Ok(trimmed.to_string());
        }

        Err(SdkError::InvalidState {
            message: "Account information not available. Try setting ANTHROPIC_USER_EMAIL environment variable.".into(),
        })
    }

    /// Read Claude config file
    async fn read_claude_config() -> Option<String> {
        // Try common config locations
        let config_paths = vec![
            dirs::home_dir()?
                .join(".config")
                .join("claude")
                .join("config.json"),
            dirs::home_dir()?.join(".claude").join("config.json"),
        ];

        for path in config_paths {
            if let Ok(content) = tokio::fs::read_to_string(&path).await
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
            {
                if let Some(email) = json.get("email").and_then(|v| v.as_str()) {
                    return Some(format!("Email: {}", email));
                }
                if let Some(user) = json.get("user").and_then(|v| v.as_str()) {
                    return Some(format!("User: {}", user));
                }
            }
        }

        None
    }

    /// Set permission mode dynamically
    ///
    /// Changes the permission mode during an active session.
    /// Requires control protocol to be enabled (via can_use_tool, hooks, mcp_servers, or file checkpointing).
    ///
    /// # Arguments
    ///
    /// * `mode` - Permission mode: "default", "acceptEdits", "plan", or "bypassPermissions"
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use nexus_claude::{ClaudeSDKClient, ClaudeCodeOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut client = ClaudeSDKClient::new(ClaudeCodeOptions::default());
    /// client.connect(None).await?;
    ///
    /// // Switch to accept edits mode
    /// client.set_permission_mode("acceptEdits").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set_permission_mode(&mut self, mode: &str) -> Result<()> {
        if let Some(ref query_handler) = self.query_handler {
            let mut handler = query_handler.lock().await;
            handler.set_permission_mode(mode).await
        } else {
            Err(SdkError::InvalidState {
                message: "Query handler not initialized. Enable control protocol features (can_use_tool, hooks, mcp_servers, or enable_file_checkpointing).".to_string(),
            })
        }
    }

    /// Set model dynamically
    ///
    /// Changes the active model during an active session.
    /// Requires control protocol to be enabled (via can_use_tool, hooks, mcp_servers, or file checkpointing).
    ///
    /// # Arguments
    ///
    /// * `model` - Model identifier (e.g., "claude-3-5-sonnet-20241022") or None to use default
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use nexus_claude::{ClaudeSDKClient, ClaudeCodeOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut client = ClaudeSDKClient::new(ClaudeCodeOptions::default());
    /// client.connect(None).await?;
    ///
    /// // Switch to a different model
    /// client.set_model(Some("claude-3-5-sonnet-20241022".to_string())).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set_model(&mut self, model: Option<String>) -> Result<()> {
        if let Some(ref query_handler) = self.query_handler {
            let mut handler = query_handler.lock().await;
            handler.set_model(model).await
        } else {
            Err(SdkError::InvalidState {
                message: "Query handler not initialized. Enable control protocol features (can_use_tool, hooks, mcp_servers, or enable_file_checkpointing).".to_string(),
            })
        }
    }

    /// Send a query with optional session ID
    ///
    /// This method is similar to Python SDK's query method in ClaudeSDKClient
    pub async fn query(&mut self, prompt: String, session_id: Option<String>) -> Result<()> {
        let session_id = session_id.unwrap_or_else(|| "default".to_string());

        // Send the message
        let message = InputMessage::user(prompt, session_id);

        {
            let mut transport = self.transport.lock().await;
            transport.send_message(message).await?;
        }

        Ok(())
    }

    /// Rewind tracked files to their state at a specific user message
    ///
    /// Requires `enable_file_checkpointing` to be enabled in `ClaudeCodeOptions`.
    /// This method allows you to undo file changes made during the session by
    /// reverting them to their state at any previous user message checkpoint.
    ///
    /// # Arguments
    ///
    /// * `user_message_id` - UUID of the user message to rewind to. This should be
    ///   the `uuid` field from a message received during the conversation.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use nexus_claude::{ClaudeSDKClient, ClaudeCodeOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeCodeOptions::builder()
    ///     .enable_file_checkpointing(true)
    ///     .build();
    /// let mut client = ClaudeSDKClient::new(options);
    /// client.connect(None).await?;
    ///
    /// // Ask Claude to make some changes
    /// client.send_request("Make some changes to my files".to_string(), None).await?;
    ///
    /// // ... later, rewind to a checkpoint
    /// // client.rewind_files("user-message-uuid-here").await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The client is not connected
    /// - The query handler is not initialized (control protocol required)
    /// - File checkpointing is not enabled
    /// - The specified user_message_id is invalid
    pub async fn rewind_files(&mut self, user_message_id: &str) -> Result<()> {
        // Check connection
        {
            let state = self.state.read().await;
            if *state != ClientState::Connected {
                return Err(SdkError::InvalidState {
                    message: "Not connected. Call connect() first.".into(),
                });
            }
        }

        if !self.options.enable_file_checkpointing {
            return Err(SdkError::InvalidState {
                message: "File checkpointing is not enabled. Set ClaudeCodeOptions::builder().enable_file_checkpointing(true).".to_string(),
            });
        }

        // Require query handler for control protocol
        if let Some(ref query_handler) = self.query_handler {
            let mut handler = query_handler.lock().await;
            handler.rewind_files(user_message_id).await
        } else {
            Err(SdkError::InvalidState {
                message: "Query handler not initialized. Enable control protocol features (can_use_tool, hooks, mcp_servers, or enable_file_checkpointing).".to_string(),
            })
        }
    }

    /// Disconnect from Claude CLI
    pub async fn disconnect(&mut self) -> Result<()> {
        // Check if already disconnected
        {
            let state = self.state.read().await;
            if *state == ClientState::Disconnected {
                return Ok(());
            }
        }

        // Disconnect transport
        {
            let mut transport = self.transport.lock().await;
            transport.disconnect().await?;
        }

        // Update state
        {
            let mut state = self.state.write().await;
            *state = ClientState::Disconnected;
        }

        // Clear sessions
        {
            let mut sessions = self.sessions.write().await;
            sessions.clear();
        }

        info!("Disconnected from Claude CLI");
        Ok(())
    }

    /// Start the message receiver task
    async fn start_message_receiver(&mut self) {
        let transport = self.transport.clone();
        let message_tx = self.message_tx.clone();
        let message_buffer = self.message_buffer.clone();
        let state = self.state.clone();
        let budget_manager = self.budget_manager.clone();

        tokio::spawn(async move {
            // Subscribe to messages without holding the lock
            let mut stream = {
                let mut transport = transport.lock().await;
                transport.receive_messages()
            }; // Lock is released here immediately

            while let Some(result) = stream.next().await {
                match result {
                    Ok(message) => {
                        // Update token usage for Result messages
                        if let Message::Result { .. } = &message
                            && let Message::Result {
                                usage,
                                total_cost_usd,
                                ..
                            } = &message
                        {
                            let (input_tokens, output_tokens) = if let Some(usage_json) = usage {
                                let input = usage_json
                                    .get("input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let output = usage_json
                                    .get("output_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                (input, output)
                            } else {
                                (0, 0)
                            };
                            let cost = total_cost_usd.unwrap_or(0.0);
                            budget_manager
                                .update_usage(input_tokens, output_tokens, cost)
                                .await;
                        }

                        // Buffer init messages for get_server_info()
                        let buffered_as_init = if let Message::System { subtype, .. } = &message
                            && subtype == "init"
                        {
                            let mut buffer = message_buffer.lock().await;
                            buffer.push(message.clone());
                            true
                        } else {
                            false
                        };

                        // Try to send to current receiver
                        let sent = {
                            let mut tx_opt = message_tx.lock().await;
                            if let Some(tx) = tx_opt.as_mut() {
                                tx.send(Ok(message.clone())).await.is_ok()
                            } else {
                                false
                            }
                        };

                        // If no receiver or send failed, buffer the message — but not
                        // the init message already buffered just above, or a late
                        // subscriber would be replayed the handshake twice.
                        if !sent && !buffered_as_init {
                            let mut buffer = message_buffer.lock().await;
                            buffer.push(message);
                        }
                    },
                    Err(e) => {
                        error!("Error receiving message: {}", e);

                        // Send error to receiver if available
                        let mut tx_opt = message_tx.lock().await;
                        if let Some(tx) = tx_opt.as_mut() {
                            let _ = tx.send(Err(e)).await;
                        }

                        // Update state on error
                        let mut state = state.write().await;
                        *state = ClientState::Error;
                        break;
                    },
                }
            }

            debug!("Message receiver task ended");
        });
    }

    /// Get token usage statistics
    ///
    /// Returns the current token usage tracker with cumulative statistics
    /// for all queries executed by this client.
    pub async fn get_usage_stats(&self) -> crate::token_tracker::TokenUsageTracker {
        self.budget_manager.get_usage().await
    }

    /// Set budget limit with optional warning callback
    ///
    /// # Arguments
    ///
    /// * `limit` - Budget limit configuration (cost and/or token caps)
    /// * `on_warning` - Optional callback function triggered when usage exceeds warning threshold
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use nexus_claude::{ClaudeSDKClient, ClaudeCodeOptions};
    /// use nexus_claude::token_tracker::{BudgetLimit, BudgetWarningCallback};
    /// use std::sync::Arc;
    ///
    /// # async fn example() {
    /// let mut client = ClaudeSDKClient::new(ClaudeCodeOptions::default());
    ///
    /// // Set budget with callback
    /// let cb: BudgetWarningCallback = Arc::new(|msg: &str| println!("Budget warning: {}", msg));
    /// client.set_budget_limit(BudgetLimit::with_cost(5.0), Some(cb)).await;
    /// # }
    /// ```
    pub async fn set_budget_limit(
        &self,
        limit: crate::token_tracker::BudgetLimit,
        on_warning: Option<crate::token_tracker::BudgetWarningCallback>,
    ) {
        self.budget_manager.set_limit(limit).await;
        if let Some(callback) = on_warning {
            self.budget_manager.set_warning_callback(callback).await;
        }
    }

    /// Clear budget limit and reset warning state
    pub async fn clear_budget_limit(&self) {
        self.budget_manager.clear_limit().await;
    }

    /// Reset token usage statistics to zero
    ///
    /// Clears all accumulated token and cost statistics.
    /// Budget limits remain in effect.
    pub async fn reset_usage_stats(&self) {
        self.budget_manager.reset_usage().await;
    }

    /// Check if budget has been exceeded
    ///
    /// Returns true if current usage exceeds any configured limits
    pub async fn is_budget_exceeded(&self) -> bool {
        self.budget_manager.is_exceeded().await
    }

    // Removed unused helper; usage is updated inline in message receiver
}

impl Drop for ClaudeSDKClient {
    fn drop(&mut self) {
        // Try to disconnect gracefully
        let transport = self.transport.clone();
        let state = self.state.clone();

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let state = state.read().await;
                if *state == ClientState::Connected {
                    let mut transport = transport.lock().await;
                    if let Err(e) = transport.disconnect().await {
                        debug!("Error disconnecting in drop: {}", e);
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_tracker::{BudgetLimit, BudgetWarningCallback};
    use crate::types::{
        CanUseTool, HookMatcher, McpServerConfig, PermissionResult, PermissionResultDeny,
        ToolPermissionContext,
    };
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Await a condition that only becomes true once one of the client's
    /// background tasks has run. The bound is generous: it is reached only when
    /// the assertion was going to fail anyway.
    macro_rules! eventually {
        ($cond:expr, $($msg:tt)+) => {{
            let mut satisfied = false;
            for _ in 0..3_000u32 {
                if $cond {
                    satisfied = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert!(satisfied, $($msg)+);
        }};
    }

    // -----------------------------------------------------------------------
    // A scripted transport: the only seam `ClaudeSDKClient` offers
    // (`with_transport`) and therefore the only way to make the transport fail
    // on demand without a subprocess. The end-to-end behaviour against a real
    // child process lives in `tests/sdk_client_fake_cli.rs`.
    // -----------------------------------------------------------------------

    /// What the scripted transport answers to `receive_control_response()`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ack {
        /// Echo back the id the client sent, `success: true`.
        Echo,
        /// Echo back the id, but `success: false`.
        Refused,
        /// Acknowledge some *other* request id.
        WrongId,
        /// `Ok(None)`: the control channel is open but closed for business.
        Closed,
        /// `Err(..)`: the transport itself failed.
        Failed,
        /// Never resolves, so the client's own 5 s deadline decides.
        Hang,
        /// Panic, so the client observes a `JoinError` on its ack task.
        Panic,
    }

    /// Everything the test keeps after handing the transport over to the client.
    struct Handle {
        /// Push what the CLI would print; the client's receiver task reads it.
        inbound: mpsc::Sender<Result<Message>>,
        /// Every `InputMessage` the client sent, in order.
        sent: Arc<StdMutex<Vec<InputMessage>>>,
        /// Every legacy `ControlRequest` the client sent, in order.
        control: Arc<StdMutex<Vec<ControlRequest>>>,
        /// How many times `disconnect()` was called, successfully or not.
        disconnects: Arc<AtomicUsize>,
    }

    impl Handle {
        fn sent(&self) -> Vec<serde_json::Value> {
            self.sent
                .lock()
                .expect("sent lock")
                .iter()
                .map(|m| serde_json::to_value(m).expect("InputMessage serialises"))
                .collect()
        }

        fn interrupt_ids(&self) -> Vec<String> {
            self.control
                .lock()
                .expect("control lock")
                .iter()
                .map(|r| match r {
                    ControlRequest::Interrupt { request_id } => request_id.clone(),
                })
                .collect()
        }

        fn disconnects(&self) -> usize {
            self.disconnects.load(Ordering::SeqCst)
        }
    }

    struct ScriptedTransport {
        inbound: Option<mpsc::Receiver<Result<Message>>>,
        ack: Ack,
        fail_connect: bool,
        fail_send: bool,
        fail_disconnect: bool,
        fail_sdk_control: bool,
        connected: bool,
        last_interrupt_id: Option<String>,
        sent: Arc<StdMutex<Vec<InputMessage>>>,
        control: Arc<StdMutex<Vec<ControlRequest>>>,
        disconnects: Arc<AtomicUsize>,
    }

    impl ScriptedTransport {
        fn new() -> (Self, Handle) {
            let (tx, rx) = mpsc::channel(64);
            let sent = Arc::new(StdMutex::new(Vec::new()));
            let control = Arc::new(StdMutex::new(Vec::new()));
            let disconnects = Arc::new(AtomicUsize::new(0));
            let transport = Self {
                inbound: Some(rx),
                ack: Ack::Echo,
                fail_connect: false,
                fail_send: false,
                fail_disconnect: false,
                fail_sdk_control: false,
                connected: false,
                last_interrupt_id: None,
                sent: sent.clone(),
                control: control.clone(),
                disconnects: disconnects.clone(),
            };
            let handle = Handle {
                inbound: tx,
                sent,
                control,
                disconnects,
            };
            (transport, handle)
        }

        fn with_ack(mut self, ack: Ack) -> Self {
            self.ack = ack;
            self
        }

        fn failing_connect(mut self) -> Self {
            self.fail_connect = true;
            self
        }

        fn failing_send(mut self) -> Self {
            self.fail_send = true;
            self
        }

        fn failing_disconnect(mut self) -> Self {
            self.fail_disconnect = true;
            self
        }

        /// Makes the control-protocol handshake fail without waiting for the
        /// query handler's 60 s response timeout.
        fn failing_sdk_control(mut self) -> Self {
            self.fail_sdk_control = true;
            self
        }
    }

    #[async_trait]
    impl Transport for ScriptedTransport {
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        async fn connect(&mut self) -> Result<()> {
            if self.fail_connect {
                return Err(SdkError::ConnectionError("scripted connect failure".into()));
            }
            self.connected = true;
            Ok(())
        }

        async fn send_message(&mut self, message: InputMessage) -> Result<()> {
            if self.fail_send {
                return Err(SdkError::TransportError("scripted send failure".into()));
            }
            self.sent.lock().expect("sent lock").push(message);
            Ok(())
        }

        fn receive_messages(
            &mut self,
        ) -> Pin<Box<dyn Stream<Item = Result<Message>> + Send + 'static>> {
            match self.inbound.take() {
                Some(rx) => Box::pin(ReceiverStream::new(rx)),
                // The client subscribes exactly once; a second subscriber would
                // silently starve, so make that visible as an empty stream.
                None => Box::pin(futures::stream::empty()),
            }
        }

        async fn send_control_request(&mut self, request: ControlRequest) -> Result<()> {
            match &request {
                ControlRequest::Interrupt { request_id } => {
                    self.last_interrupt_id = Some(request_id.clone());
                },
            }
            self.control.lock().expect("control lock").push(request);
            Ok(())
        }

        async fn receive_control_response(&mut self) -> Result<Option<ControlResponse>> {
            let echoed = self.last_interrupt_id.clone().unwrap_or_default();
            match self.ack {
                Ack::Echo => Ok(Some(ControlResponse::InterruptAck {
                    request_id: echoed,
                    success: true,
                })),
                Ack::Refused => Ok(Some(ControlResponse::InterruptAck {
                    request_id: echoed,
                    success: false,
                })),
                Ack::WrongId => Ok(Some(ControlResponse::InterruptAck {
                    request_id: format!("{echoed}-stale"),
                    success: true,
                })),
                Ack::Closed => Ok(None),
                Ack::Failed => Err(SdkError::TransportError("scripted control failure".into())),
                Ack::Hang => std::future::pending().await,
                Ack::Panic => panic!("scripted transport panics inside receive_control_response"),
            }
        }

        async fn send_sdk_control_request(&mut self, _request: serde_json::Value) -> Result<()> {
            if self.fail_sdk_control {
                return Err(SdkError::TransportError(
                    "scripted control-protocol failure".into(),
                ));
            }
            Ok(())
        }

        async fn send_sdk_control_response(&mut self, _response: serde_json::Value) -> Result<()> {
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        async fn disconnect(&mut self) -> Result<()> {
            self.disconnects.fetch_add(1, Ordering::SeqCst);
            if self.fail_disconnect {
                return Err(SdkError::TransportError(
                    "scripted disconnect failure".into(),
                ));
            }
            self.connected = false;
            Ok(())
        }
    }

    /// A client on a scripted transport, with default (control-protocol-free)
    /// options so `query_handler` stays `None`.
    fn scripted(transport: ScriptedTransport) -> ClaudeSDKClient {
        ClaudeSDKClient::with_transport(ClaudeCodeOptions::default(), Box::new(transport))
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant {
            message: crate::types::AssistantMessage {
                content: vec![ContentBlock::Text(crate::types::TextContent {
                    text: text.to_string(),
                })],
            },
            parent_tool_use_id: None,
        }
    }

    /// The terminal message, with whatever usage/cost the test wants to feed the
    /// budget manager.
    fn result_with(usage: Option<serde_json::Value>, cost: Option<f64>) -> Message {
        Message::Result {
            subtype: "success".to_string(),
            duration_ms: 1,
            duration_api_ms: 1,
            is_error: false,
            num_turns: 1,
            session_id: "scripted".to_string(),
            total_cost_usd: cost,
            usage,
            result: Some("done".to_string()),
            structured_output: None,
        }
    }

    /// The text of the first text block of an assistant message.
    fn assistant_text_of(message: &Message) -> Option<String> {
        match message {
            Message::Assistant { message, .. } => message.content.iter().find_map(|b| match b {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            }),
            _ => None,
        }
    }

    fn init_message(session_id: &str) -> Message {
        Message::System {
            subtype: "init".to_string(),
            data: serde_json::json!({"session_id": session_id, "tools": ["Bash"]}),
        }
    }

    /// Restores an environment variable when the test ends, pass or fail.
    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    // =======================================================================
    // Construction: which options switch the control protocol on
    // =======================================================================

    #[tokio::test]
    async fn a_fresh_client_is_disconnected_with_no_sessions() {
        let client = scripted(ScriptedTransport::new().0);

        assert!(!client.is_connected().await);
        assert!(client.get_sessions().await.is_empty());
        assert_eq!(*client.state.read().await, ClientState::Disconnected);
        assert!(
            client.query_handler.is_none(),
            "default options must not switch the control protocol on"
        );
    }

    struct DenyEverything;

    #[async_trait]
    impl CanUseTool for DenyEverything {
        async fn can_use_tool(
            &self,
            _tool_name: &str,
            _input: &serde_json::Value,
            _context: &ToolPermissionContext,
        ) -> PermissionResult {
            PermissionResult::Deny(PermissionResultDeny {
                message: "no".to_string(),
                interrupt: false,
            })
        }
    }

    /// Four independent switches create the query handler; each is spelled out
    /// here because `with_transport_internal` is the only place that decides it
    /// and the four conditions are OR-ed in one expression.
    #[tokio::test]
    async fn each_control_protocol_feature_creates_the_query_handler() {
        let plain = scripted(ScriptedTransport::new().0);
        assert!(plain.query_handler.is_none());

        let with_permission = ClaudeCodeOptions {
            can_use_tool: Some(Arc::new(DenyEverything)),
            ..Default::default()
        };
        let client =
            ClaudeSDKClient::with_transport(with_permission, Box::new(ScriptedTransport::new().0));
        assert!(
            client.query_handler.is_some(),
            "can_use_tool requires the control protocol"
        );

        let mut hooks = HashMap::new();
        hooks.insert(
            "PreToolUse".to_string(),
            vec![HookMatcher {
                matcher: None,
                hooks: vec![],
            }],
        );
        let client = ClaudeSDKClient::with_transport(
            ClaudeCodeOptions::builder().hooks(hooks).build(),
            Box::new(ScriptedTransport::new().0),
        );
        assert!(
            client.query_handler.is_some(),
            "hooks require the control protocol"
        );

        let client = ClaudeSDKClient::with_transport(
            ClaudeCodeOptions::builder()
                .enable_file_checkpointing(true)
                .build(),
            Box::new(ScriptedTransport::new().0),
        );
        assert!(
            client.query_handler.is_some(),
            "enable_file_checkpointing requires the control protocol"
        );
    }

    /// Only `McpServerConfig::Sdk` entries are handed to the query handler as
    /// in-process servers; every other variant is filtered out, yet a non-empty
    /// `mcp_servers` map still switches the control protocol on.
    #[tokio::test]
    async fn only_sdk_mcp_servers_are_extracted_but_any_server_enables_the_protocol() {
        let options = ClaudeCodeOptions::builder()
            .add_mcp_server(
                "in_process",
                McpServerConfig::Sdk {
                    name: "in_process".to_string(),
                    instance: Arc::new(7u32),
                },
            )
            .add_mcp_server(
                "external",
                McpServerConfig::Stdio {
                    command: "does-not-run".to_string(),
                    args: None,
                    env: None,
                },
            )
            .build();

        let client = ClaudeSDKClient::with_transport(options, Box::new(ScriptedTransport::new().0));
        assert!(client.query_handler.is_some());

        // A map holding only a non-SDK server still creates the handler, which is
        // the branch where `sdk_mcp_servers` comes out empty.
        let options = ClaudeCodeOptions::builder()
            .add_mcp_server(
                "external",
                McpServerConfig::Http {
                    url: "http://127.0.0.1:1/mcp".to_string(),
                    headers: None,
                },
            )
            .build();
        let client = ClaudeSDKClient::with_transport(options, Box::new(ScriptedTransport::new().0));
        assert!(
            client.query_handler.is_some(),
            "a non-empty mcp_servers map enables the protocol even with no SDK server"
        );
    }

    // =======================================================================
    // connect()
    // =======================================================================

    #[tokio::test]
    async fn a_failing_transport_connect_leaves_the_client_disconnected() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport.failing_connect());

        let error = client
            .connect(Some("salut".to_string()))
            .await
            .expect_err("the transport refused to connect");
        assert!(
            matches!(error, SdkError::ConnectionError(ref m) if m == "scripted connect failure"),
            "the transport's error must reach the caller unchanged, got {error:?}"
        );
        assert!(!client.is_connected().await);
        assert!(
            handle.sent().is_empty(),
            "the initial prompt must not be sent when connect failed"
        );
    }

    #[tokio::test]
    async fn connect_with_an_initial_prompt_registers_the_default_session() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);

        client
            .connect(Some("salut".to_string()))
            .await
            .expect("scripted connect");

        assert!(client.is_connected().await);
        assert_eq!(client.get_sessions().await, vec!["default".to_string()]);
        let sent = handle.sent();
        assert_eq!(sent.len(), 1, "exactly the initial prompt, got {sent:?}");
        assert_eq!(sent[0]["type"], "user");
        assert_eq!(sent[0]["message"]["content"], "salut");
        assert_eq!(sent[0]["session_id"], "default");

        client.disconnect().await.expect("scripted disconnect");
    }

    /// The early return when already connected also swallows the prompt it was
    /// given: a caller that reconnects with a new prompt sends nothing at all.
    #[tokio::test]
    async fn a_second_connect_is_a_no_op_and_drops_its_prompt() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);

        client.connect(None).await.expect("first connect");
        client
            .connect(Some("this prompt is lost".to_string()))
            .await
            .expect("second connect is Ok");

        assert!(
            handle.sent().is_empty(),
            "the already-connected early return never reaches send_request"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    // =======================================================================
    // send_user_message / send_request / query
    // =======================================================================

    #[tokio::test]
    async fn send_user_message_is_refused_before_connect() {
        let mut client = scripted(ScriptedTransport::new().0);

        let error = client
            .send_user_message("salut".to_string())
            .await
            .expect_err("sending without a connection must fail");
        assert!(
            matches!(error, SdkError::InvalidState { ref message } if message == "Not connected"),
            "got {error:?}"
        );
        assert!(client.get_sessions().await.is_empty());
    }

    /// The session is created and its counter bumped *before* the transport is
    /// asked to send, so a failed send still leaves a session behind.
    #[tokio::test]
    async fn a_failed_send_still_registers_the_session() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport.failing_send());

        client.connect(None).await.expect("scripted connect");
        let error = client
            .send_user_message("salut".to_string())
            .await
            .expect_err("the transport refused the message");
        assert!(
            matches!(error, SdkError::TransportError(ref m) if m == "scripted send failure"),
            "got {error:?}"
        );
        assert_eq!(
            client.get_sessions().await,
            vec!["default".to_string()],
            "session bookkeeping happens before the send and is not rolled back"
        );
        assert!(handle.sent().is_empty());
    }

    #[tokio::test]
    async fn repeated_sends_reuse_the_single_default_session() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);

        client.connect(None).await.expect("scripted connect");
        client
            .send_user_message("un".to_string())
            .await
            .expect("first send");
        client
            .send_user_message("deux".to_string())
            .await
            .expect("second send");

        assert_eq!(
            client.get_sessions().await,
            vec!["default".to_string()],
            "the session id is hard-coded to \"default\""
        );
        let sent = handle.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1]["message"]["content"], "deux");
        client.disconnect().await.expect("scripted disconnect");
    }

    /// `send_request`'s second parameter is documented as an "optional
    /// session_id" but is bound to `_session_id` and never read: the message
    /// still goes out on `default`.
    #[tokio::test]
    async fn send_request_silently_discards_the_session_id_it_is_given() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);

        client.connect(None).await.expect("scripted connect");
        client
            .send_request("salut".to_string(), Some("my-session".to_string()))
            .await
            .expect("send_request");

        assert_eq!(
            handle.sent()[0]["session_id"],
            "default",
            "the caller's session id is dropped on the floor"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    /// `query()` honours the session id `send_request` ignores, but skips both
    /// the connection check and the session registry that `send_user_message`
    /// performs — so it happily writes to a transport the client never connected.
    #[tokio::test]
    async fn query_honours_its_session_id_but_checks_no_state() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);

        client
            .query("avant connexion".to_string(), None)
            .await
            .expect("query does not check the client state");
        assert!(
            !client.is_connected().await,
            "the client is still disconnected, yet the message went out"
        );
        assert_eq!(handle.sent()[0]["session_id"], "default");

        client.connect(None).await.expect("scripted connect");
        client
            .query("avec session".to_string(), Some("my-session".to_string()))
            .await
            .expect("query");
        let sent = handle.sent();
        assert_eq!(sent[1]["session_id"], "my-session");
        assert!(
            client.get_sessions().await.is_empty(),
            "query() never records a session, unlike send_user_message()"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    // =======================================================================
    // receive_messages / receive_response / the receiver task
    // =======================================================================

    #[tokio::test]
    async fn a_subscriber_receives_live_messages_in_order() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        let mut stream = Box::pin(client.receive_messages().await);
        handle
            .inbound
            .send(Ok(init_message("sess-live")))
            .await
            .expect("push init");
        handle
            .inbound
            .send(Ok(assistant("bonjour")))
            .await
            .expect("push assistant");
        handle
            .inbound
            .send(Ok(result_with(None, None)))
            .await
            .expect("push result");

        let first = stream.next().await.expect("init").expect("ok");
        assert!(matches!(first, Message::System { ref subtype, .. } if subtype == "init"));
        let second = stream.next().await.expect("assistant").expect("ok");
        assert!(matches!(second, Message::Assistant { .. }));
        let third = stream.next().await.expect("result").expect("ok");
        assert!(matches!(
            third,
            Message::Result {
                is_error: false,
                ..
            }
        ));

        client.disconnect().await.expect("scripted disconnect");
    }

    /// Without a subscriber the receiver task buffers everything, and the init
    /// message is additionally kept for `get_server_info()`. Subscribing later
    /// replays the buffer — and empties it, so `get_server_info()` then forgets
    /// the handshake it had already seen.
    #[tokio::test]
    async fn the_buffer_is_replayed_to_a_late_subscriber_and_cleared() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        handle
            .inbound
            .send(Ok(init_message("sess-buffered")))
            .await
            .expect("push init");
        handle
            .inbound
            .send(Ok(assistant("bonjour")))
            .await
            .expect("push assistant");

        eventually!(
            client.get_server_info().await.is_some(),
            "the init message must be buffered for get_server_info()"
        );
        let info = client.get_server_info().await.expect("buffered init");
        assert_eq!(info["session_id"], "sess-buffered");

        let mut stream = Box::pin(client.receive_messages().await);
        let replayed = stream.next().await.expect("replayed init").expect("ok");
        assert!(matches!(replayed, Message::System { ref subtype, .. } if subtype == "init"));
        // Regression guard: `start_message_receiver` used to push the init
        // message into `message_buffer` twice — once for `get_server_info()` and
        // once through the generic "nobody is listening" path — so the next item
        // replayed here was a second copy of the handshake instead of the
        // assistant turn. Revert the `buffered_as_init` guard and this fails.
        let replayed = stream
            .next()
            .await
            .expect("replayed assistant")
            .expect("ok");
        assert!(
            matches!(replayed, Message::Assistant { .. }),
            "the init message must be replayed exactly once, got {replayed:?}"
        );

        assert!(
            client.get_server_info().await.is_none(),
            "receive_messages() drains the buffer, so the handshake is lost for good"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    #[tokio::test]
    async fn get_server_info_ignores_system_messages_that_are_not_init() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        assert!(
            client.get_server_info().await.is_none(),
            "nothing has arrived yet"
        );

        handle
            .inbound
            .send(Ok(Message::System {
                subtype: "compact_boundary".to_string(),
                data: serde_json::json!({"trigger": "auto"}),
            }))
            .await
            .expect("push system");
        handle
            .inbound
            .send(Ok(assistant("bonjour")))
            .await
            .expect("push assistant");

        // Ask while both sit in the buffer: the scan walks a non-empty buffer and
        // still finds no handshake.
        eventually!(
            client.message_buffer.lock().await.len() == 2,
            "both messages must be buffered before the scan is meaningful"
        );
        assert!(
            client.get_server_info().await.is_none(),
            "a buffer full of non-init messages yields no server information"
        );

        // Drain both through a subscriber so there is no doubt they were seen,
        // then ask again: plenty arrived, none of it was an `init`.
        {
            let mut stream = Box::pin(client.receive_messages().await);
            let first = stream.next().await.expect("system").expect("ok");
            assert!(matches!(first, Message::System { ref subtype, .. }
                if subtype == "compact_boundary"));
            let second = stream.next().await.expect("assistant").expect("ok");
            assert!(matches!(second, Message::Assistant { .. }));
        }
        assert!(
            client.get_server_info().await.is_none(),
            "only subtype == \"init\" counts as server information"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    /// A client built with control-protocol options but never connected has a
    /// query handler with no initialization result: `get_server_info` must fall
    /// through to the message buffer instead of unwrapping it.
    #[tokio::test]
    async fn get_server_info_falls_back_to_the_buffer_before_initialization() {
        let client = ClaudeSDKClient::with_transport(
            ClaudeCodeOptions::builder()
                .enable_file_checkpointing(true)
                .build(),
            Box::new(ScriptedTransport::new().0),
        );

        assert!(client.query_handler.is_some());
        assert!(
            client.get_server_info().await.is_none(),
            "no handshake has happened, so there is nothing to report"
        );
    }

    #[tokio::test]
    async fn a_transport_error_reaches_the_subscriber_and_poisons_the_state() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        let mut stream = Box::pin(client.receive_messages().await);
        handle
            .inbound
            .send(Err(SdkError::TransportError("flux rompu".into())))
            .await
            .expect("push error");

        let item = stream.next().await.expect("the error is forwarded");
        let error = item.expect_err("an Err item");
        assert!(
            matches!(error, SdkError::TransportError(ref m) if m == "flux rompu"),
            "got {error:?}"
        );
        eventually!(
            *client.state.read().await == ClientState::Error,
            "a stream error must move the client to the Error state"
        );
        assert!(!client.is_connected().await);
        drop(stream);
    }

    /// Same error, no subscriber: the state still flips, and the error is simply
    /// dropped — it is never buffered, so nobody ever learns about it.
    #[tokio::test]
    async fn a_transport_error_without_a_subscriber_is_swallowed() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        handle
            .inbound
            .send(Err(SdkError::TransportError("flux rompu".into())))
            .await
            .expect("push error");

        eventually!(
            *client.state.read().await == ClientState::Error,
            "the state must flip even with nobody listening"
        );
        assert!(
            client.message_buffer.lock().await.is_empty(),
            "errors are not buffered, unlike messages"
        );
    }

    #[tokio::test]
    async fn receive_response_stops_at_the_first_result_message() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        handle
            .inbound
            .send(Ok(assistant("un")))
            .await
            .expect("push assistant");
        handle
            .inbound
            .send(Ok(result_with(None, None)))
            .await
            .expect("push result");
        handle
            .inbound
            .send(Ok(assistant("après le résultat")))
            .await
            .expect("push trailing assistant");

        let mut collected = Vec::new();
        {
            let mut stream = client.receive_response().await;
            while let Some(item) = stream.next().await {
                collected.push(item.expect("no error scripted"));
            }
        }

        assert_eq!(
            collected.len(),
            2,
            "the stream must end on the result, got {collected:?}"
        );
        assert!(matches!(collected[0], Message::Assistant { .. }));
        assert!(matches!(collected[1], Message::Result { .. }));
    }

    // =======================================================================
    // Token accounting
    // =======================================================================

    #[tokio::test]
    async fn result_usage_feeds_the_budget_manager() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        handle
            .inbound
            .send(Ok(result_with(
                Some(serde_json::json!({"input_tokens": 11, "output_tokens": 7})),
                Some(0.25),
            )))
            .await
            .expect("push result");

        eventually!(
            client.get_usage_stats().await.session_count == 1,
            "the receiver task must account the result message"
        );
        let usage = client.get_usage_stats().await;
        assert_eq!(usage.total_input_tokens, 11);
        assert_eq!(usage.total_output_tokens, 7);
        assert!((usage.total_cost_usd - 0.25).abs() < 1e-12);
        assert!(!client.is_budget_exceeded().await, "no limit is set");

        client.reset_usage_stats().await;
        let usage = client.get_usage_stats().await;
        assert_eq!(usage.total_tokens(), 0);
        assert_eq!(usage.session_count, 0);
    }

    /// `usage` and `total_cost_usd` are both optional on the wire, and a usage
    /// object whose fields are not numbers is treated the same as a missing one.
    #[tokio::test]
    async fn a_result_without_usable_usage_counts_as_zero_but_still_as_a_session() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        handle
            .inbound
            .send(Ok(result_with(None, None)))
            .await
            .expect("push bare result");
        handle
            .inbound
            .send(Ok(result_with(
                Some(serde_json::json!({"input_tokens": "beaucoup"})),
                None,
            )))
            .await
            .expect("push result with non-numeric usage");

        eventually!(
            client.get_usage_stats().await.session_count == 2,
            "both results must be accounted"
        );
        let usage = client.get_usage_stats().await;
        assert_eq!(usage.total_tokens(), 0, "nothing numeric to add");
        assert_eq!(usage.total_cost_usd, 0.0);
    }

    #[tokio::test]
    async fn crossing_the_budget_cap_invokes_the_warning_callback() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);

        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink = seen.clone();
        let callback: BudgetWarningCallback = Arc::new(move |message: &str| {
            sink.lock().expect("sink lock").push(message.to_string());
        });
        client
            .set_budget_limit(BudgetLimit::with_tokens(4), Some(callback))
            .await;

        client.connect(None).await.expect("scripted connect");
        handle
            .inbound
            .send(Ok(result_with(
                Some(serde_json::json!({"input_tokens": 3, "output_tokens": 5})),
                Some(0.0),
            )))
            .await
            .expect("push result");

        eventually!(
            !seen.lock().expect("sink lock").is_empty(),
            "8 tokens against a 4-token cap must warn"
        );
        assert_eq!(
            seen.lock().expect("sink lock").as_slice(),
            ["Budget limit exceeded".to_string()]
        );
        assert!(client.is_budget_exceeded().await);

        client.clear_budget_limit().await;
        assert!(
            !client.is_budget_exceeded().await,
            "without a limit nothing can be exceeded"
        );
    }

    /// `set_budget_limit` with no callback must still arm the limit; the cap is
    /// then enforced by `is_budget_exceeded()` with nothing to call back into.
    #[tokio::test]
    async fn a_budget_limit_without_a_callback_is_still_enforced() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client
            .set_budget_limit(BudgetLimit::with_tokens(1), None)
            .await;
        assert!(
            !client.is_budget_exceeded().await,
            "nothing has been spent yet"
        );

        client.connect(None).await.expect("scripted connect");
        handle
            .inbound
            .send(Ok(result_with(
                Some(serde_json::json!({"input_tokens": 2, "output_tokens": 1})),
                None,
            )))
            .await
            .expect("push result");

        eventually!(
            client.is_budget_exceeded().await,
            "3 tokens against a 1-token cap must count as exceeded"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    // =======================================================================
    // interrupt()
    // =======================================================================

    #[tokio::test]
    async fn interrupt_is_refused_before_connect() {
        let mut client = scripted(ScriptedTransport::new().0);
        let error = client
            .interrupt()
            .await
            .expect_err("interrupting a disconnected client must fail");
        assert!(
            matches!(error, SdkError::InvalidState { ref message } if message == "Not connected"),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn interrupt_numbers_its_requests_and_accepts_a_matching_ack() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport.with_ack(Ack::Echo));
        client.connect(None).await.expect("scripted connect");

        client.interrupt().await.expect("first interrupt");
        client.interrupt().await.expect("second interrupt");

        assert_eq!(
            handle.interrupt_ids(),
            vec!["interrupt_1".to_string(), "interrupt_2".to_string()],
            "the request counter must advance for every interrupt"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    #[tokio::test]
    async fn an_unsuccessful_ack_is_an_error() {
        let (transport, _handle) = ScriptedTransport::new();
        let mut client = scripted(transport.with_ack(Ack::Refused));
        client.connect(None).await.expect("scripted connect");

        let error = client.interrupt().await.expect_err("success: false");
        assert!(
            matches!(error, SdkError::ControlRequestError(ref m)
                if m == "Interrupt not acknowledged successfully"),
            "got {error:?}"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    #[tokio::test]
    async fn an_ack_for_another_request_id_is_an_error() {
        let (transport, _handle) = ScriptedTransport::new();
        let mut client = scripted(transport.with_ack(Ack::WrongId));
        client.connect(None).await.expect("scripted connect");

        let error = client.interrupt().await.expect_err("mismatched request id");
        assert!(
            matches!(error, SdkError::ControlRequestError(ref m)
                if m == "Interrupt not acknowledged successfully"),
            "got {error:?}"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    #[tokio::test]
    async fn a_closed_control_channel_is_reported_as_a_missing_ack() {
        let (transport, _handle) = ScriptedTransport::new();
        let mut client = scripted(transport.with_ack(Ack::Closed));
        client.connect(None).await.expect("scripted connect");

        let error = client.interrupt().await.expect_err("Ok(None)");
        assert!(
            matches!(error, SdkError::ControlRequestError(ref m)
                if m == "No interrupt acknowledgment received"),
            "got {error:?}"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    #[tokio::test]
    async fn a_control_channel_failure_is_propagated_verbatim() {
        let (transport, _handle) = ScriptedTransport::new();
        let mut client = scripted(transport.with_ack(Ack::Failed));
        client.connect(None).await.expect("scripted connect");

        let error = client
            .interrupt()
            .await
            .expect_err("Err from the transport");
        assert!(
            matches!(error, SdkError::TransportError(ref m) if m == "scripted control failure"),
            "the transport's own error must not be rewritten, got {error:?}"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    /// The ack wait is bounded at five seconds. The Tokio clock is paused, so the
    /// deadline is reached without the test taking five seconds.
    #[tokio::test(start_paused = true)]
    async fn interrupt_gives_up_after_five_seconds() {
        let (transport, _handle) = ScriptedTransport::new();
        let mut client = scripted(transport.with_ack(Ack::Hang));
        client.connect(None).await.expect("scripted connect");

        let error = client.interrupt().await.expect_err("the ack never comes");
        assert!(
            matches!(error, SdkError::Timeout { seconds: 5 }),
            "got {error:?}"
        );
    }

    /// A transport that panics must surface as an error, not as a lost future.
    #[tokio::test]
    async fn a_panicking_transport_is_reported_as_a_failed_ack_task() {
        let (transport, _handle) = ScriptedTransport::new();
        let mut client = scripted(transport.with_ack(Ack::Panic));
        client.connect(None).await.expect("scripted connect");

        let error = client.interrupt().await.expect_err("the ack task panicked");
        assert!(
            matches!(error, SdkError::ControlRequestError(ref m) if m == "Interrupt task panicked"),
            "got {error:?}"
        );
    }

    // =======================================================================
    // disconnect() and Drop
    // =======================================================================

    #[tokio::test]
    async fn disconnect_clears_the_sessions_and_is_idempotent() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);

        client
            .connect(Some("salut".to_string()))
            .await
            .expect("scripted connect");
        assert_eq!(client.get_sessions().await.len(), 1);

        client.disconnect().await.expect("first disconnect");
        assert!(!client.is_connected().await);
        assert!(
            client.get_sessions().await.is_empty(),
            "disconnect must forget the sessions"
        );
        assert_eq!(handle.disconnects(), 1);

        client.disconnect().await.expect("second disconnect is Ok");
        assert_eq!(
            handle.disconnects(),
            1,
            "the already-disconnected early return must not reach the transport"
        );
    }

    /// When the transport refuses to shut down, the error propagates but the
    /// client keeps reporting itself as connected and keeps its sessions.
    #[tokio::test]
    async fn a_failing_disconnect_leaves_the_client_marked_connected() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport.failing_disconnect());

        client
            .connect(Some("salut".to_string()))
            .await
            .expect("scripted connect");
        let error = client.disconnect().await.expect_err("scripted failure");
        assert!(
            matches!(error, SdkError::TransportError(ref m) if m == "scripted disconnect failure"),
            "got {error:?}"
        );
        assert!(
            client.is_connected().await,
            "the state is only updated after a successful transport disconnect"
        );
        assert_eq!(client.get_sessions().await.len(), 1);
        assert_eq!(handle.disconnects(), 1);
    }

    #[tokio::test]
    async fn dropping_a_connected_client_disconnects_it_on_the_runtime() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        drop(client);

        eventually!(
            handle.disconnects() == 1,
            "Drop must spawn the transport shutdown on the current runtime"
        );
    }

    #[tokio::test]
    async fn dropping_a_client_that_never_connected_touches_nothing() {
        let (transport, handle) = ScriptedTransport::new();
        let client = scripted(transport);
        drop(client);

        // Give the spawned task every chance to run before concluding.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            handle.disconnects(),
            0,
            "Drop only disconnects a client whose state is Connected"
        );
    }

    /// Outside a Tokio runtime there is nothing to spawn on, so `Drop` is a
    /// silent no-op — a connected client dropped from a blocking context leaks
    /// its subprocess.
    #[test]
    fn dropping_a_client_outside_a_runtime_cannot_disconnect() {
        let (transport, handle) = ScriptedTransport::new();
        let client = scripted(transport);
        drop(client);
        assert_eq!(handle.disconnects(), 0);
    }

    // =======================================================================
    // Control-protocol-only entry points, without a query handler
    // =======================================================================

    const NO_HANDLER: &str = "Query handler not initialized. Enable control protocol features (can_use_tool, hooks, mcp_servers, or enable_file_checkpointing).";

    #[tokio::test]
    async fn set_permission_mode_needs_the_control_protocol() {
        let mut client = scripted(ScriptedTransport::new().0);
        let error = client
            .set_permission_mode("acceptEdits")
            .await
            .expect_err("no query handler");
        assert!(
            matches!(error, SdkError::InvalidState { ref message } if message == NO_HANDLER),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn set_model_needs_the_control_protocol() {
        let mut client = scripted(ScriptedTransport::new().0);
        let error = client
            .set_model(Some("claude-opus-5".to_string()))
            .await
            .expect_err("no query handler");
        assert!(
            matches!(error, SdkError::InvalidState { ref message } if message == NO_HANDLER),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn rewind_files_checks_the_connection_then_the_feature_flag() {
        let (transport, _handle) = ScriptedTransport::new();
        let mut client = scripted(transport);

        let error = client
            .rewind_files("uuid-1")
            .await
            .expect_err("not connected");
        assert!(
            matches!(error, SdkError::InvalidState { ref message }
                if message == "Not connected. Call connect() first."),
            "got {error:?}"
        );

        client.connect(None).await.expect("scripted connect");
        let error = client
            .rewind_files("uuid-1")
            .await
            .expect_err("checkpointing is off");
        assert!(
            matches!(error, SdkError::InvalidState { ref message }
                if message.starts_with("File checkpointing is not enabled.")),
            "got {error:?}"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    // =======================================================================
    // get_account_info() and its three sources
    // =======================================================================

    #[tokio::test]
    async fn get_account_info_needs_a_connection() {
        let mut client = scripted(ScriptedTransport::new().0);
        let error = client.get_account_info().await.expect_err("not connected");
        assert!(
            matches!(error, SdkError::InvalidState { ref message }
                if message == "Not connected. Call connect() first."),
            "got {error:?}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(claude_home_env)]
    async fn get_account_info_prefers_the_environment_variable() {
        let _email = EnvGuard::set("ANTHROPIC_USER_EMAIL", "someone@example.invalid");
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        let info = client.get_account_info().await.expect("env var wins");
        assert_eq!(info, "Email: someone@example.invalid");
        assert!(
            handle.sent().is_empty(),
            "method 1 must short-circuit before the /status prompt"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    /// With no environment variable and no readable config, the last resort is a
    /// `/status` prompt whose answer is accepted only if it mentions the account.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(claude_home_env)]
    async fn get_account_info_accepts_a_status_reply_that_mentions_the_account() {
        let home = tempfile::tempdir().expect("temp home");
        let _email = EnvGuard::unset("ANTHROPIC_USER_EMAIL");
        let _home = EnvGuard::set("HOME", &home.path().display().to_string());

        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        handle
            .inbound
            .send(Ok(Message::Assistant {
                message: crate::types::AssistantMessage {
                    content: vec![
                        // A non-text block must be skipped, not concatenated.
                        ContentBlock::Thinking(crate::types::ThinkingContent {
                            thinking: "le /status arrive".to_string(),
                            signature: "sig".to_string(),
                        }),
                        ContentBlock::Text(crate::types::TextContent {
                            text: "Logged in, account: someone@example.invalid".to_string(),
                        }),
                    ],
                },
                parent_tool_use_id: None,
            }))
            .await
            .expect("push status answer");
        handle
            .inbound
            .send(Ok(result_with(None, None)))
            .await
            .expect("push result");

        let info = client
            .get_account_info()
            .await
            .expect("the reply mentions the account");
        assert_eq!(info, "Logged in, account: someone@example.invalid");
        let sent = handle.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0]["message"]["content"], "/status",
            "method 3 drives the CLI with a /status prompt"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(claude_home_env)]
    async fn get_account_info_rejects_a_status_reply_about_anything_else() {
        let home = tempfile::tempdir().expect("temp home");
        let _email = EnvGuard::unset("ANTHROPIC_USER_EMAIL");
        let _home = EnvGuard::set("HOME", &home.path().display().to_string());

        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        handle
            .inbound
            .send(Ok(Message::System {
                subtype: "compact_boundary".to_string(),
                data: serde_json::json!({}),
            }))
            .await
            .expect("push ignored message");
        handle
            .inbound
            .send(Ok(assistant("Bonjour, comment puis-je aider ?")))
            .await
            .expect("push unrelated answer");
        handle
            .inbound
            .send(Ok(result_with(None, None)))
            .await
            .expect("push result");

        let error = client
            .get_account_info()
            .await
            .expect_err("the reply says nothing about the account");
        assert!(
            matches!(error, SdkError::InvalidState { ref message }
                if message.starts_with("Account information not available.")),
            "got {error:?}"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(claude_home_env)]
    async fn get_account_info_rejects_a_turn_with_no_assistant_text_at_all() {
        let home = tempfile::tempdir().expect("temp home");
        let _email = EnvGuard::unset("ANTHROPIC_USER_EMAIL");
        let _home = EnvGuard::set("HOME", &home.path().display().to_string());

        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        handle
            .inbound
            .send(Ok(result_with(None, None)))
            .await
            .expect("push bare result");

        let error = client
            .get_account_info()
            .await
            .expect_err("nothing to read");
        assert!(
            matches!(error, SdkError::InvalidState { ref message }
                if message.starts_with("Account information not available.")),
            "got {error:?}"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    /// `get_account_info` surfaces an error item from the message stream instead
    /// of looping forever on it.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(claude_home_env)]
    async fn get_account_info_propagates_a_stream_error() {
        let home = tempfile::tempdir().expect("temp home");
        let _email = EnvGuard::unset("ANTHROPIC_USER_EMAIL");
        let _home = EnvGuard::set("HOME", &home.path().display().to_string());

        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        // The error must land *while* get_account_info is reading the stream: if
        // it arrives first, the receiver task flips the state to Error and the
        // connection check rejects the call before method 3 is ever reached.
        let inbound = handle.inbound.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = inbound
                .send(Err(SdkError::TransportError("flux rompu".into())))
                .await;
        });

        let error = client
            .get_account_info()
            .await
            .expect_err("the stream error wins");
        assert!(
            matches!(error, SdkError::TransportError(ref m) if m == "flux rompu"),
            "got {error:?}"
        );
    }

    // =======================================================================
    // A handshake that fails after the transport is already up
    // =======================================================================

    /// `connect()` brings the transport up first and only then performs the
    /// control-protocol handshake. When the handshake fails, the error propagates
    /// but the *connected* transport is never shut down and the client's state
    /// stays `Disconnected` — so the follow-up `disconnect()` takes its
    /// already-disconnected early return and the child process is leaked.
    #[tokio::test]
    async fn a_failed_handshake_leaves_the_transport_connected_and_unreachable() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = ClaudeSDKClient::with_transport(
            ClaudeCodeOptions::builder()
                .enable_file_checkpointing(true)
                .build(),
            Box::new(transport.failing_sdk_control()),
        );

        let error = client
            .connect(None)
            .await
            .expect_err("the handshake failed");
        assert!(
            matches!(error, SdkError::TransportError(ref m)
                if m == "scripted control-protocol failure"),
            "got {error:?}"
        );
        assert!(
            !client.is_connected().await,
            "the state was never advanced past Disconnected"
        );
        assert_eq!(
            handle.disconnects(),
            0,
            "the transport was connected and is never told to shut down"
        );

        client
            .disconnect()
            .await
            .expect("disconnect reports success");
        assert_eq!(
            handle.disconnects(),
            0,
            "disconnect() takes the already-disconnected early return, so the \
             live transport is leaked"
        );
    }

    /// Method 2 in situ: with no environment variable but a readable config, the
    /// config answers and the `/status` prompt is never sent.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(claude_home_env)]
    async fn get_account_info_falls_back_to_the_claude_config() {
        let home = tempfile::tempdir().expect("temp home");
        let _email = EnvGuard::unset("ANTHROPIC_USER_EMAIL");
        let _home = EnvGuard::set("HOME", &home.path().display().to_string());
        let dot = home.path().join(".claude");
        std::fs::create_dir_all(&dot).expect("create .claude");
        std::fs::write(
            dot.join("config.json"),
            r#"{"email": "config@example.invalid"}"#,
        )
        .expect("write config");

        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        let info = client.get_account_info().await.expect("the config answers");
        assert_eq!(info, "Email: config@example.invalid");
        assert!(
            handle.sent().is_empty(),
            "method 2 must short-circuit before the /status prompt"
        );
        client.disconnect().await.expect("scripted disconnect");
    }

    /// A replay abandoned mid-flight must stop quietly and leave the client
    /// usable. The buffer is deliberately larger than the 100-slot channel
    /// `receive_messages()` creates, so the replay task is still blocked on a
    /// full channel when the subscriber walks away.
    #[tokio::test]
    async fn abandoning_a_replay_mid_flight_leaves_the_client_usable() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport);
        client.connect(None).await.expect("scripted connect");

        for i in 0..150u32 {
            handle
                .inbound
                .send(Ok(assistant(&format!("m{i}"))))
                .await
                .expect("push");
        }
        eventually!(
            client.message_buffer.lock().await.len() == 150,
            "every message must reach the buffer before the replay starts"
        );

        // Take the whole buffer, then walk away before the replay can drain it.
        drop(client.receive_messages().await);

        let mut stream = Box::pin(client.receive_messages().await);
        handle
            .inbound
            .send(Ok(assistant("après")))
            .await
            .expect("push after the abandoned replay");
        let live = stream.next().await.expect("a live message").expect("ok");
        assert_eq!(
            assistant_text_of(&live).as_deref(),
            Some("après"),
            "the abandoned replay must not leak into the next subscription"
        );

        client.disconnect().await.expect("scripted disconnect");
    }

    /// A transport that fails during `Drop` is logged and swallowed: `Drop`
    /// cannot return an error, so the failure must not escape.
    #[tokio::test]
    async fn a_failing_disconnect_during_drop_is_swallowed() {
        let (transport, handle) = ScriptedTransport::new();
        let mut client = scripted(transport.failing_disconnect());
        client.connect(None).await.expect("scripted connect");

        drop(client);

        eventually!(
            handle.disconnects() == 1,
            "Drop must still attempt the shutdown"
        );
        // Nothing to propagate the error to: the only effect is a debug log, and
        // the failure must not take the runtime down.
        tokio::task::yield_now().await;
    }

    // -----------------------------------------------------------------------
    // read_claude_config(): the private helper behind method 2
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(claude_home_env)]
    async fn read_claude_config_returns_none_when_no_config_exists() {
        let home = tempfile::tempdir().expect("temp home");
        let _home = EnvGuard::set("HOME", &home.path().display().to_string());

        assert!(ClaudeSDKClient::read_claude_config().await.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(claude_home_env)]
    async fn read_claude_config_prefers_the_xdg_location_and_the_email_field() {
        let home = tempfile::tempdir().expect("temp home");
        let _home = EnvGuard::set("HOME", &home.path().display().to_string());

        let xdg = home.path().join(".config").join("claude");
        std::fs::create_dir_all(&xdg).expect("create .config/claude");
        std::fs::write(
            xdg.join("config.json"),
            r#"{"email": "xdg@example.invalid", "user": "ignored"}"#,
        )
        .expect("write config");

        let dot = home.path().join(".claude");
        std::fs::create_dir_all(&dot).expect("create .claude");
        std::fs::write(
            dot.join("config.json"),
            r#"{"email": "dotdir@example.invalid"}"#,
        )
        .expect("write config");

        assert_eq!(
            ClaudeSDKClient::read_claude_config().await,
            Some("Email: xdg@example.invalid".to_string()),
            "~/.config/claude/config.json is tried first, and email wins over user"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(claude_home_env)]
    async fn read_claude_config_falls_back_to_the_user_field_in_the_dot_directory() {
        let home = tempfile::tempdir().expect("temp home");
        let _home = EnvGuard::set("HOME", &home.path().display().to_string());

        let dot = home.path().join(".claude");
        std::fs::create_dir_all(&dot).expect("create .claude");
        std::fs::write(dot.join("config.json"), r#"{"user": "triviere"}"#).expect("write config");

        assert_eq!(
            ClaudeSDKClient::read_claude_config().await,
            Some("User: triviere".to_string())
        );
    }

    /// A config file that is unreadable as JSON, or that holds neither field, is
    /// skipped silently rather than reported as an error.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(claude_home_env)]
    async fn read_claude_config_skips_unusable_files() {
        let home = tempfile::tempdir().expect("temp home");
        let _home = EnvGuard::set("HOME", &home.path().display().to_string());

        let xdg = home.path().join(".config").join("claude");
        std::fs::create_dir_all(&xdg).expect("create .config/claude");
        std::fs::write(xdg.join("config.json"), "{ not json").expect("write broken config");

        let dot = home.path().join(".claude");
        std::fs::create_dir_all(&dot).expect("create .claude");
        std::fs::write(dot.join("config.json"), r#"{"organization": "acme"}"#)
            .expect("write config without email or user");

        assert!(
            ClaudeSDKClient::read_claude_config().await.is_none(),
            "neither a parse failure nor a missing field may be reported as an account"
        );
    }
}
