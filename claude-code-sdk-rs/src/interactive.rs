//! Working interactive client implementation

use crate::{
    errors::{Result, SdkError},
    transport::{InputMessage, SubprocessTransport, Transport},
    types::{
        ClaudeCodeOptions, ControlRequest, HookCallback, HookContext, HookInput, HookJSONOutput,
        HookMatcher, Message, SDKControlInitializeRequest, SDKControlRequest,
        SDKHookCallbackRequest,
    },
};
use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};

/// Interactive client for stateful conversations with Claude
///
/// This is the recommended client for interactive use. It provides a clean API
/// that matches the Python SDK's functionality.
pub struct InteractiveClient {
    transport: Arc<Mutex<Box<dyn Transport + Send>>>,
    connected: bool,
    /// Hook configurations from ClaudeCodeOptions (used by initialize_hooks)
    hooks: Option<HashMap<String, Vec<HookMatcher>>>,
    /// Registered hook callbacks keyed by callback_id (populated by initialize_hooks)
    hook_callbacks: Arc<RwLock<HashMap<String, Arc<dyn HookCallback>>>>,
    /// Counter for generating unique callback IDs
    callback_counter: Arc<Mutex<u64>>,
}

impl InteractiveClient {
    /// Create a client from a pre-built transport (for testing or custom transports)
    pub fn from_transport(transport: Box<dyn Transport + Send>) -> Self {
        Self {
            transport: Arc::new(Mutex::new(transport)),
            connected: false,
            hooks: None,
            hook_callbacks: Arc::new(RwLock::new(HashMap::new())),
            callback_counter: Arc::new(Mutex::new(0)),
        }
    }

    /// Create a client from a pre-built transport with hooks (for testing)
    pub fn from_transport_with_hooks(
        transport: Box<dyn Transport + Send>,
        hooks: HashMap<String, Vec<HookMatcher>>,
    ) -> Self {
        Self {
            transport: Arc::new(Mutex::new(transport)),
            connected: false,
            hooks: Some(hooks),
            hook_callbacks: Arc::new(RwLock::new(HashMap::new())),
            callback_counter: Arc::new(Mutex::new(0)),
        }
    }

    /// Create a new client
    pub fn new(options: ClaudeCodeOptions) -> Result<Self> {
        unsafe {
            std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "sdk-rust");
        }
        let hooks = options.hooks.clone();
        let transport: Box<dyn Transport + Send> = Box::new(SubprocessTransport::new(options)?);
        Ok(Self {
            transport: Arc::new(Mutex::new(transport)),
            connected: false,
            hooks,
            hook_callbacks: Arc::new(RwLock::new(HashMap::new())),
            callback_counter: Arc::new(Mutex::new(0)),
        })
    }

    /// Take the SDK control receiver for handling inbound control requests
    /// (e.g., `can_use_tool` permission requests) from the Claude CLI subprocess.
    ///
    /// This can only be called once — subsequent calls return `None`.
    /// The receiver yields raw JSON values representing SDK control protocol messages.
    ///
    /// Use this to listen for permission requests when running in non-BypassPermissions
    /// modes and handle them via `send_control_response()`.
    pub async fn take_sdk_control_receiver(
        &self,
    ) -> Option<tokio::sync::mpsc::Receiver<serde_json::Value>> {
        let mut transport = self.transport.lock().await;
        transport.take_sdk_control_receiver()
    }

    /// Get a clone of the hook callbacks registry.
    ///
    /// This allows the caller (e.g., PO backend `stream_response`) to dispatch
    /// hook callbacks **without** holding the client lock. The returned Arc can
    /// be used with the standalone `dispatch_hook_callback_from_registry()` helper.
    pub fn hook_callbacks(&self) -> Arc<RwLock<HashMap<String, Arc<dyn HookCallback>>>> {
        self.hook_callbacks.clone()
    }

    /// Clone the stdin sender for writing control responses without holding
    /// the client lock. This allows `send_permission_response` to write to
    /// the CLI subprocess while `stream_response` holds the client lock.
    pub async fn clone_stdin_sender(&self) -> Option<tokio::sync::mpsc::Sender<String>> {
        let transport = self.transport.lock().await;
        transport.clone_stdin_sender()
    }

    /// Connect to Claude
    pub async fn connect(&mut self) -> Result<()> {
        if self.connected {
            return Ok(());
        }

        let mut transport = self.transport.lock().await;
        transport.connect().await?;
        drop(transport); // Release lock immediately

        self.connected = true;
        info!("Connected to Claude CLI");
        Ok(())
    }

    /// Send a message and receive all messages until Result message
    pub async fn send_and_receive(&mut self, prompt: String) -> Result<Vec<Message>> {
        if !self.connected {
            return Err(SdkError::InvalidState {
                message: "Not connected".into(),
            });
        }

        // Send message
        {
            let mut transport = self.transport.lock().await;
            let message = InputMessage::user(prompt, "default".to_string());
            transport.send_message(message).await?;
        } // Lock released here

        debug!("Message sent, waiting for response");

        // Receive messages
        let mut messages = Vec::new();
        loop {
            // Try to get a message
            let msg_result = {
                let mut transport = self.transport.lock().await;
                let mut stream = transport.receive_messages();
                stream.next().await
            }; // Lock released here

            // Process the message
            if let Some(result) = msg_result {
                match result {
                    Ok(msg) => {
                        debug!("Received: {:?}", msg);
                        let is_result = matches!(msg, Message::Result { .. });
                        messages.push(msg);
                        if is_result {
                            break;
                        }
                    },
                    Err(e) => return Err(e),
                }
            } else {
                // No more messages, wait a bit
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        }

        Ok(messages)
    }

    /// Send a message without waiting for response
    pub async fn send_message(&mut self, prompt: String) -> Result<()> {
        if !self.connected {
            return Err(SdkError::InvalidState {
                message: "Not connected".into(),
            });
        }

        let mut transport = self.transport.lock().await;
        let message = InputMessage::user(prompt, "default".to_string());
        transport.send_message(message).await?;
        drop(transport);

        debug!("Message sent");
        Ok(())
    }

    /// Send a raw SDK control response to the Claude CLI subprocess.
    ///
    /// This is used to respond to control protocol requests (e.g., `can_use_tool`
    /// permission requests) that arrive when running in non-BypassPermissions mode.
    /// The response is written directly to stdin as a JSON control_response message.
    ///
    /// # Arguments
    /// * `response` - The control response payload, e.g. `{"allow": true}` or
    ///   `{"allow": false, "reason": "User denied"}`. The transport wraps this in
    ///   `{"type": "control_response", "response": <payload>}` automatically.
    pub async fn send_control_response(&mut self, response: serde_json::Value) -> Result<()> {
        if !self.connected {
            return Err(SdkError::InvalidState {
                message: "Not connected".into(),
            });
        }

        let mut transport = self.transport.lock().await;
        transport.send_sdk_control_response(response).await?;
        drop(transport);

        debug!("Control response sent");
        Ok(())
    }

    /// Send a message and receive response as a stream (atomic operation)
    ///
    /// This method subscribes to the message stream BEFORE sending the message,
    /// ensuring no messages are lost due to race conditions. This is the recommended
    /// way to send messages when you need streaming responses.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use nexus_claude::{InteractiveClient, ClaudeCodeOptions, Message};
    /// use futures::StreamExt;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let mut client = InteractiveClient::new(ClaudeCodeOptions::default())?;
    ///     client.connect().await?;
    ///
    ///     // Send and receive atomically - no race condition
    ///     let mut stream = std::pin::pin!(client.send_and_receive_stream("Hello!".to_string()).await?);
    ///     while let Some(msg) = stream.next().await {
    ///         match msg? {
    ///             Message::Assistant { message, .. } => println!("{:?}", message),
    ///             Message::Result { .. } => break,
    ///             _ => {}
    ///         }
    ///     }
    ///
    ///     Ok(())
    /// }
    /// ```
    pub async fn send_and_receive_stream(
        &mut self,
        prompt: String,
    ) -> Result<impl Stream<Item = Result<Message>> + '_> {
        if !self.connected {
            return Err(SdkError::InvalidState {
                message: "Not connected".into(),
            });
        }

        // Create channel for forwarding messages
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        // CRITICAL: Subscribe and send within the SAME lock acquisition
        // This guarantees the subscription happens BEFORE any response arrives
        {
            let mut transport = self.transport.lock().await;

            // 1. Subscribe to the broadcast FIRST
            let mut stream = transport.receive_messages();

            // 2. THEN send the message
            let message = InputMessage::user(prompt, "default".to_string());
            transport.send_message(message).await?;

            debug!("Message sent, subscription active");

            // 3. Spawn task to forward messages (stream is already subscribed)
            let tx_clone = tx;
            tokio::spawn(async move {
                while let Some(result) = stream.next().await {
                    if tx_clone.send(result).await.is_err() {
                        // Receiver dropped
                        break;
                    }
                }
            });
        } // Lock released here, after subscription and send

        // Return stream that stops at Result message
        Ok(async_stream::stream! {
            let mut rx_stream = ReceiverStream::new(rx);

            while let Some(result) = rx_stream.next().await {
                match &result {
                    Ok(msg) => {
                        let is_result = matches!(msg, Message::Result { .. });
                        yield result;
                        if is_result {
                            break;
                        }
                    }
                    Err(_) => {
                        yield result;
                        break;
                    }
                }
            }
        })
    }

    /// Receive messages until Result message (convenience method like Python SDK)
    pub async fn receive_response(&mut self) -> Result<Vec<Message>> {
        if !self.connected {
            return Err(SdkError::InvalidState {
                message: "Not connected".into(),
            });
        }

        let mut messages = Vec::new();
        loop {
            // Try to get a message
            let msg_result = {
                let mut transport = self.transport.lock().await;
                let mut stream = transport.receive_messages();
                stream.next().await
            }; // Lock released here

            // Process the message
            if let Some(result) = msg_result {
                match result {
                    Ok(msg) => {
                        debug!("Received: {:?}", msg);
                        let is_result = matches!(msg, Message::Result { .. });
                        messages.push(msg);
                        if is_result {
                            break;
                        }
                    },
                    Err(e) => return Err(e),
                }
            } else {
                // No more messages, wait a bit
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        }

        Ok(messages)
    }

    /// Receive messages as a stream (streaming output support)
    ///
    /// Returns a stream of messages that can be iterated over asynchronously.
    /// This is similar to Python SDK's `receive_messages()` method.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use nexus_claude::{InteractiveClient, ClaudeCodeOptions};
    /// use futures::StreamExt;
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let mut client = InteractiveClient::new(ClaudeCodeOptions::default())?;
    ///     client.connect().await?;
    ///     
    ///     // Send a message
    ///     client.send_message("Hello!".to_string()).await?;
    ///     
    ///     // Receive messages as a stream
    ///     let mut stream = client.receive_messages_stream().await;
    ///     while let Some(msg) = stream.next().await {
    ///         match msg {
    ///             Ok(message) => println!("Received: {:?}", message),
    ///             Err(e) => eprintln!("Error: {}", e),
    ///         }
    ///     }
    ///     
    ///     Ok(())
    /// }
    /// ```
    pub async fn receive_messages_stream(&mut self) -> impl Stream<Item = Result<Message>> + '_ {
        // Create a channel for messages
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let transport = self.transport.clone();

        // Spawn a task to receive messages from transport
        tokio::spawn(async move {
            let mut transport = transport.lock().await;
            let mut stream = transport.receive_messages();

            while let Some(result) = stream.next().await {
                // Send each message through the channel
                if tx.send(result).await.is_err() {
                    // Receiver dropped, stop sending
                    break;
                }
            }
        });

        // Return the receiver as a stream
        ReceiverStream::new(rx)
    }

    /// Receive messages as an async iterator until a Result message
    ///
    /// This is a convenience method that collects messages until a Result message
    /// is received, similar to Python SDK's `receive_response()`.
    pub async fn receive_response_stream(&mut self) -> impl Stream<Item = Result<Message>> + '_ {
        // Create a stream that stops after Result message
        async_stream::stream! {
            let mut stream = self.receive_messages_stream().await;

            while let Some(result) = stream.next().await {
                match &result {
                    Ok(msg) => {
                        let is_result = matches!(msg, Message::Result { .. });
                        yield result;
                        if is_result {
                            break;
                        }
                    }
                    Err(_) => {
                        yield result;
                        break;
                    }
                }
            }
        }
    }

    /// Change the permission mode of the active CLI subprocess session.
    ///
    /// Sends a `set_permission_mode` control request to the Claude CLI process,
    /// which takes effect on the next tool use. The mode change does NOT interrupt
    /// any ongoing streaming response.
    ///
    /// # Valid modes
    /// - `"default"` — prompts for dangerous tools (Bash, Edit, Write)
    /// - `"acceptEdits"` — auto-approves file edits, prompts for Bash
    /// - `"bypassPermissions"` — auto-approves all tools
    /// - `"plan"` — read-only, blocks all write operations
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use nexus_claude::{InteractiveClient, ClaudeCodeOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut client = InteractiveClient::new(ClaudeCodeOptions::default())?;
    /// client.connect().await?;
    /// // Switch to bypass mode mid-session
    /// client.set_permission_mode("bypassPermissions").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set_permission_mode(&mut self, mode: &str) -> Result<()> {
        if !self.connected {
            return Err(SdkError::InvalidState {
                message: "Not connected".into(),
            });
        }

        // Validate mode
        const VALID_MODES: &[&str] = &["default", "acceptEdits", "bypassPermissions", "plan"];
        if !VALID_MODES.contains(&mode) {
            return Err(SdkError::InvalidState {
                message: format!(
                    "Invalid permission mode '{}'. Valid modes: {}",
                    mode,
                    VALID_MODES.join(", ")
                ),
            });
        }

        let request = serde_json::json!({
            "type": "control_request",
            "request_id": uuid::Uuid::new_v4().to_string(),
            "request": {
                "subtype": "set_permission_mode",
                "mode": mode
            }
        });

        let mut transport = self.transport.lock().await;
        transport.send_sdk_control_request(request).await?;
        drop(transport);

        info!(mode = %mode, "Permission mode change request sent");
        Ok(())
    }

    // ========================================================================
    // Hook lifecycle — initialize, dispatch, respond
    // ========================================================================

    /// Initialize the control protocol and register hooks with the CLI.
    ///
    /// This reproduces the logic from `Query::initialize()`: it generates unique
    /// callback IDs for each `HookCallback`, stores them locally, and sends an
    /// `SDKControlRequest::Initialize` message to the CLI subprocess so it knows
    /// which hooks to trigger.
    ///
    /// **Must be called after `connect()` and before `take_sdk_control_receiver()`.**
    /// The init message expects a response on the SDK control channel. If the
    /// receiver has already been taken, the response will be lost.
    ///
    /// No-op if no hooks were configured in `ClaudeCodeOptions`.
    pub async fn initialize_hooks(&self) -> Result<()> {
        let hooks = match &self.hooks {
            Some(h) if !h.is_empty() => h,
            _ => {
                debug!("No hooks configured — skipping initialize_hooks");
                return Ok(());
            },
        };

        // Generate callback IDs and register callbacks (mirrors Query::initialize)
        let mut counter = self.callback_counter.lock().await;
        let mut callbacks_map = self.hook_callbacks.write().await;

        let hooks_json: HashMap<String, serde_json::Value> = hooks
            .iter()
            .map(|(event_name, matchers)| {
                let matchers_with_ids: Vec<serde_json::Value> = matchers
                    .iter()
                    .map(|matcher| {
                        let callback_ids: Vec<String> = matcher
                            .hooks
                            .iter()
                            .map(|hook_cb| {
                                *counter += 1;
                                let callback_id =
                                    format!("hook_{}_{}", *counter, uuid::Uuid::new_v4().simple());
                                callbacks_map.insert(callback_id.clone(), hook_cb.clone());
                                callback_id
                            })
                            .collect();

                        serde_json::json!({
                            "matcher": matcher.matcher.clone(),
                            "hookCallbackIds": callback_ids
                        })
                    })
                    .collect();

                (event_name.clone(), serde_json::json!(matchers_with_ids))
            })
            .collect();

        drop(callbacks_map);
        drop(counter);

        // Build the initialize control request
        let init_request = SDKControlRequest::Initialize(SDKControlInitializeRequest {
            subtype: "initialize".to_string(),
            hooks: Some(hooks_json),
        });

        let request_id = uuid::Uuid::new_v4().to_string();
        let control_msg = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": init_request
        });

        // Send via transport stdin
        {
            let mut transport = self.transport.lock().await;
            transport.send_sdk_control_request(control_msg).await?;
        }

        info!("initialize_hooks: sent init with hook callback IDs to CLI");
        Ok(())
    }

    /// Dispatch an inbound `hook_callback` control message to the registered callback.
    ///
    /// This is the counterpart of `Query::start_control_handler()` for the hook_callback
    /// subtype. The caller (PO backend's `stream_response`) reads raw JSON from
    /// `sdk_control_rx`, detects `subtype: "hook_callback"`, and calls this method.
    ///
    /// Returns `Some(Ok(output))` if the callback was found and executed successfully,
    /// `Some(Err(..))` if the callback failed, or `None` if the message is not a
    /// hook_callback or the callback_id is unknown.
    ///
    /// **Lock-free**: does NOT acquire the transport mutex. Safe to call while
    /// `stream_response` holds the client lock.
    pub async fn dispatch_hook_callback(
        &self,
        control_msg: &serde_json::Value,
    ) -> Option<std::result::Result<HookJSONOutput, SdkError>> {
        // Try to extract hook_callback fields — support both formats:
        // 1. Top-level: { "subtype": "hook_callback", "callback_id": ..., "input": ... }
        // 2. Nested:    { "request": { "subtype": "hook_callback", ... } }
        let request_data = control_msg.get("request").unwrap_or(control_msg);

        let subtype = request_data
            .get("subtype")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if subtype != "hook_callback" {
            return None;
        }

        // Try structured deserialization first, then fallback to manual field access
        let (callback_id, input, tool_use_id) = if let Ok(req) =
            serde_json::from_value::<SDKHookCallbackRequest>(request_data.clone())
        {
            (req.callback_id, req.input, req.tool_use_id)
        } else {
            let cb_id = request_data
                .get("callback_id")
                .or_else(|| request_data.get("callbackId"))
                .and_then(|v| v.as_str())?;
            let input = request_data
                .get("input")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let tool_use_id = request_data
                .get("tool_use_id")
                .or_else(|| request_data.get("toolUseId"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            (cb_id.to_string(), input, tool_use_id)
        };

        // Look up the callback
        let callbacks = self.hook_callbacks.read().await;
        let callback = match callbacks.get(&callback_id) {
            Some(cb) => cb.clone(),
            None => {
                warn!("No hook callback found for ID: {}", callback_id);
                return None;
            },
        };
        drop(callbacks);

        // Parse HookInput and execute
        let context = HookContext { signal: None };
        let result = match serde_json::from_value::<HookInput>(input.clone()) {
            Ok(hook_input) => {
                callback
                    .execute(&hook_input, tool_use_id.as_deref(), &context)
                    .await
            },
            Err(parse_err) => {
                error!("Failed to parse hook input: {}", parse_err);
                Err(SdkError::MessageParseError {
                    error: format!("Invalid hook input: {parse_err}"),
                    raw: input.to_string(),
                })
            },
        };

        Some(result)
    }

    /// Send the result of a hook callback back to the CLI subprocess.
    ///
    /// Writes a `control_response` JSON message to stdin with the serialized
    /// `HookJSONOutput`. Uses `clone_stdin_sender()` internally so it does NOT
    /// acquire the transport mutex — safe to call while streaming.
    ///
    /// # Arguments
    /// * `request_id` - The request_id from the original hook_callback control message
    /// * `output` - The result from `dispatch_hook_callback`
    pub async fn send_hook_response(
        &self,
        request_id: &str,
        output: &std::result::Result<HookJSONOutput, SdkError>,
    ) -> Result<()> {
        let response_json = match output {
            Ok(hook_output) => {
                let output_value = serde_json::to_value(hook_output).unwrap_or_else(|e| {
                    error!("Failed to serialize hook output: {}", e);
                    serde_json::json!({})
                });
                serde_json::json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": request_id,
                        "response": output_value
                    }
                })
            },
            Err(e) => {
                serde_json::json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "error",
                        "request_id": request_id,
                        "error": e.to_string()
                    }
                })
            },
        };

        // Use stdin_tx directly (lock-free path) if available
        let stdin_tx = {
            let transport = self.transport.lock().await;
            transport.clone_stdin_sender()
        };

        if let Some(tx) = stdin_tx {
            let json = serde_json::to_string(&response_json)?;
            tx.send(json).await.map_err(|e| {
                SdkError::ConnectionError(format!("Failed to send hook response: {}", e))
            })?;
            debug!("Hook response sent for request_id={}", request_id);
            Ok(())
        } else {
            // Fallback: send via transport (takes lock, but only briefly)
            let mut transport = self.transport.lock().await;
            transport
                .send_sdk_control_response(
                    response_json
                        .get("response")
                        .cloned()
                        .unwrap_or(serde_json::json!({})),
                )
                .await
        }
    }

    /// Get the PID of the Claude CLI child process.
    ///
    /// Returns `Some(pid)` when the subprocess is running, `None` otherwise.
    /// Useful for sending signals directly to the process group (e.g.,
    /// cascading SIGINT to all descendant processes).
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # async fn example(client: &nexus_claude::InteractiveClient) {
    /// if let Some(pid) = client.child_pid().await {
    ///     // Send SIGINT to the process group
    ///     #[cfg(unix)]
    ///     unsafe { libc::kill(-(pid as i32), libc::SIGINT); }
    /// }
    /// # }
    /// ```
    pub async fn child_pid(&self) -> Option<u32> {
        let transport = self.transport.lock().await;
        transport.child_pid()
    }

    /// Send interrupt signal to cancel current operation
    pub async fn interrupt(&mut self) -> Result<()> {
        if !self.connected {
            return Err(SdkError::InvalidState {
                message: "Not connected".into(),
            });
        }

        let mut transport = self.transport.lock().await;
        let request = ControlRequest::Interrupt {
            request_id: uuid::Uuid::new_v4().to_string(),
        };
        transport.send_control_request(request).await?;
        drop(transport);

        info!("Interrupt sent");
        Ok(())
    }

    /// Build the JSON string for an interrupt control request.
    ///
    /// This produces the exact same wire format as
    /// [`SubprocessTransport::send_control_request`] for the `Interrupt` variant:
    ///
    /// ```json
    /// {"type":"control_request","request":{"type":"interrupt","request_id":"<uuid>"}}
    /// ```
    ///
    /// **Use case**: The PO Backend can send interrupts via a cloned `stdin_tx`
    /// (obtained from [`Transport::clone_stdin_sender`]) without acquiring the
    /// client Mutex lock. This avoids duplicating the wire format outside of
    /// the SDK.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nexus_claude::InteractiveClient;
    ///
    /// let json = InteractiveClient::build_interrupt_json();
    /// // Send directly via stdin_tx.try_send(json) — no client lock needed
    /// ```
    pub fn build_interrupt_json() -> String {
        let request_id = uuid::Uuid::new_v4().to_string();
        serde_json::to_string(&serde_json::json!({
            "type": "control_request",
            "request": {
                "type": "interrupt",
                "request_id": request_id
            }
        }))
        .expect("interrupt JSON serialization cannot fail")
    }

    /// Disconnect
    pub async fn disconnect(&mut self) -> Result<()> {
        if !self.connected {
            return Ok(());
        }

        let mut transport = self.transport.lock().await;
        transport.disconnect().await?;
        drop(transport);

        self.connected = false;
        info!("Disconnected from Claude CLI");
        Ok(())
    }
}

// ============================================================================
// Standalone hook helpers (for use without client lock)
// ============================================================================

/// Check if a raw SDK control JSON message is a `hook_callback`.
///
/// Inspects the `subtype` field (supports both top-level and nested `request`).
/// This is a cheap check that can be done before dispatching.
pub fn is_hook_callback(control_msg: &serde_json::Value) -> bool {
    let request_data = control_msg.get("request").unwrap_or(control_msg);
    request_data.get("subtype").and_then(|v| v.as_str()) == Some("hook_callback")
}

/// Dispatch a `hook_callback` control message using a pre-cloned callbacks registry.
///
/// This is the lock-free counterpart of `InteractiveClient::dispatch_hook_callback`.
/// Use this when the client mutex is held (e.g., during `stream_response`) and you
/// already have a cloned `hook_callbacks` Arc from `InteractiveClient::hook_callbacks()`.
///
/// Returns `Some(Ok(output))` if the callback executed, `Some(Err(..))` on error,
/// or `None` if the callback_id is unknown.
pub async fn dispatch_hook_from_registry(
    control_msg: &serde_json::Value,
    hook_callbacks: &RwLock<HashMap<String, Arc<dyn HookCallback>>>,
) -> Option<std::result::Result<HookJSONOutput, SdkError>> {
    let request_data = control_msg.get("request").unwrap_or(control_msg);

    let subtype = request_data
        .get("subtype")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if subtype != "hook_callback" {
        return None;
    }

    // Extract fields
    let (callback_id, input, tool_use_id) =
        if let Ok(req) = serde_json::from_value::<SDKHookCallbackRequest>(request_data.clone()) {
            (req.callback_id, req.input, req.tool_use_id)
        } else {
            let cb_id = request_data
                .get("callback_id")
                .or_else(|| request_data.get("callbackId"))
                .and_then(|v| v.as_str())?;
            let input = request_data
                .get("input")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let tool_use_id = request_data
                .get("tool_use_id")
                .or_else(|| request_data.get("toolUseId"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            (cb_id.to_string(), input, tool_use_id)
        };

    // Look up
    let callbacks = hook_callbacks.read().await;
    let callback = match callbacks.get(&callback_id) {
        Some(cb) => cb.clone(),
        None => {
            warn!("No hook callback found for ID: {}", callback_id);
            return None;
        },
    };
    drop(callbacks);

    // Execute
    let context = HookContext { signal: None };
    let result = match serde_json::from_value::<HookInput>(input.clone()) {
        Ok(hook_input) => {
            callback
                .execute(&hook_input, tool_use_id.as_deref(), &context)
                .await
        },
        Err(parse_err) => {
            error!("Failed to parse hook input: {}", parse_err);
            Err(SdkError::MessageParseError {
                error: format!("Invalid hook input: {parse_err}"),
                raw: input.to_string(),
            })
        },
    };

    Some(result)
}

/// Build the JSON control_response for a hook callback result.
///
/// Returns the serialized JSON string ready to be sent via `stdin_tx`.
/// This avoids needing access to the client or transport.
pub fn build_hook_response_json(
    request_id: &str,
    output: &std::result::Result<HookJSONOutput, SdkError>,
) -> String {
    let response_json = match output {
        Ok(hook_output) => {
            let output_value = serde_json::to_value(hook_output).unwrap_or_else(|e| {
                error!("Failed to serialize hook output: {}", e);
                serde_json::json!({})
            });
            serde_json::json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": output_value
                }
            })
        },
        Err(e) => {
            serde_json::json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": request_id,
                    "error": e.to_string()
                }
            })
        },
    };
    serde_json::to_string(&response_json).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::mock::MockTransport;
    use crate::types::{
        HookCallback, HookContext, HookInput, HookJSONOutput, HookMatcher, SyncHookJSONOutput,
    };
    use std::sync::Arc;

    /// A simple test callback that records calls and returns continue: true
    #[derive(Clone)]
    struct TestHookCallback {
        call_count: Arc<Mutex<u32>>,
    }

    impl TestHookCallback {
        fn new() -> Self {
            Self {
                call_count: Arc::new(Mutex::new(0)),
            }
        }

        async fn calls(&self) -> u32 {
            *self.call_count.lock().await
        }
    }

    #[async_trait::async_trait]
    impl HookCallback for TestHookCallback {
        async fn execute(
            &self,
            _input: &HookInput,
            _tool_use_id: Option<&str>,
            _context: &HookContext,
        ) -> std::result::Result<HookJSONOutput, SdkError> {
            let mut count = self.call_count.lock().await;
            *count += 1;
            Ok(HookJSONOutput::Sync(SyncHookJSONOutput {
                continue_: Some(true),
                suppress_output: None,
                stop_reason: None,
                decision: None,
                system_message: None,
                reason: None,
                hook_specific_output: None,
            }))
        }
    }

    fn make_hooks_with_callback(
        event: &str,
        callback: Arc<dyn HookCallback>,
    ) -> HashMap<String, Vec<HookMatcher>> {
        let mut hooks = HashMap::new();
        hooks.insert(
            event.to_string(),
            vec![HookMatcher {
                matcher: None,
                hooks: vec![callback],
            }],
        );
        hooks
    }

    #[tokio::test]
    async fn test_initialize_hooks_sends_init_message() {
        let (transport, mut handle) = MockTransport::pair();
        let callback = Arc::new(TestHookCallback::new());
        let hooks = make_hooks_with_callback("PreCompact", callback);

        let client = InteractiveClient::from_transport_with_hooks(transport, hooks);

        // initialize_hooks should send a control_request via the transport
        client.initialize_hooks().await.unwrap();

        // The init message should be observable via outbound_control_request_rx
        let msg = handle
            .outbound_control_request_rx
            .recv()
            .await
            .expect("Should have received init message");

        // Verify structure
        assert_eq!(msg["type"], "control_request");
        let request = &msg["request"];
        assert_eq!(request["subtype"], "initialize");
        // hooks should be present with PreCompact key
        let hooks_json = request["hooks"]
            .as_object()
            .expect("hooks should be object");
        assert!(
            hooks_json.contains_key("PreCompact"),
            "Should contain PreCompact key"
        );
        // Should contain a matcher with hookCallbackIds
        let matchers = hooks_json["PreCompact"]
            .as_array()
            .expect("PreCompact should be array");
        assert_eq!(matchers.len(), 1);
        let callback_ids = matchers[0]["hookCallbackIds"]
            .as_array()
            .expect("hookCallbackIds should be array");
        assert_eq!(callback_ids.len(), 1);
        // Callback ID should start with "hook_"
        let cb_id = callback_ids[0].as_str().unwrap();
        assert!(
            cb_id.starts_with("hook_"),
            "Callback ID should start with hook_"
        );
    }

    #[tokio::test]
    async fn test_initialize_hooks_noop_when_no_hooks() {
        let (transport, mut handle) = MockTransport::pair();
        // No hooks configured
        let client = InteractiveClient::from_transport(transport);

        client.initialize_hooks().await.unwrap();

        // No message should have been sent
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            handle.outbound_control_request_rx.recv(),
        )
        .await;
        assert!(result.is_err(), "Should timeout — no message sent");
    }

    #[tokio::test]
    async fn test_dispatch_hook_callback_executes_callback() {
        let (transport, _handle) = MockTransport::pair();
        let callback = Arc::new(TestHookCallback::new());
        let hooks = make_hooks_with_callback("PreCompact", callback.clone());

        let client = InteractiveClient::from_transport_with_hooks(transport, hooks);

        // First, initialize to populate callback IDs
        client.initialize_hooks().await.unwrap();

        // Get the registered callback ID
        let callbacks = client.hook_callbacks.read().await;
        let (cb_id, _) = callbacks.iter().next().expect("Should have one callback");
        let cb_id = cb_id.clone();
        drop(callbacks);

        // Simulate a hook_callback control message from CLI
        // HookInput uses internally-tagged enum: { "hook_event_name": "PreCompact", ...fields }
        let control_msg = serde_json::json!({
            "type": "control_request",
            "request_id": "req-123",
            "request": {
                "subtype": "hook_callback",
                "callback_id": cb_id,
                "input": {
                    "hook_event_name": "PreCompact",
                    "session_id": "sess-1",
                    "transcript_path": "/tmp/transcript.json",
                    "cwd": "/home/user",
                    "trigger": "auto"
                }
            }
        });

        let result = client.dispatch_hook_callback(&control_msg).await;
        assert!(result.is_some(), "Should dispatch successfully");
        let output = result.unwrap();
        assert!(output.is_ok(), "Callback should succeed");

        // Verify callback was actually executed
        assert_eq!(callback.calls().await, 1);

        // Verify output is Sync with continue: true
        match output.unwrap() {
            HookJSONOutput::Sync(sync_out) => {
                assert_eq!(sync_out.continue_, Some(true));
            },
            _ => panic!("Expected Sync output"),
        }
    }

    #[tokio::test]
    async fn test_dispatch_unknown_callback_returns_none() {
        let (transport, _handle) = MockTransport::pair();
        let callback = Arc::new(TestHookCallback::new());
        let hooks = make_hooks_with_callback("PreCompact", callback.clone());

        let client = InteractiveClient::from_transport_with_hooks(transport, hooks);
        client.initialize_hooks().await.unwrap();

        // Send a hook_callback with an unknown callback_id
        let control_msg = serde_json::json!({
            "request": {
                "subtype": "hook_callback",
                "callback_id": "unknown_callback_id",
                "input": {
                    "hook_event_name": "PreCompact",
                    "session_id": "sess-1",
                    "transcript_path": "/tmp/t.json",
                    "cwd": "/home",
                    "trigger": "auto"
                }
            }
        });

        let result = client.dispatch_hook_callback(&control_msg).await;
        assert!(result.is_none(), "Unknown callback should return None");

        // Original callback should NOT have been called
        assert_eq!(callback.calls().await, 0);
    }

    #[tokio::test]
    async fn test_dispatch_non_hook_message_returns_none() {
        let (transport, _handle) = MockTransport::pair();
        let client = InteractiveClient::from_transport(transport);

        // Send a non-hook control message
        let control_msg = serde_json::json!({
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "input": {}
            }
        });

        let result = client.dispatch_hook_callback(&control_msg).await;
        assert!(result.is_none(), "Non-hook message should return None");
    }

    #[tokio::test]
    async fn test_send_hook_response_success_format() {
        let (transport, mut handle) = MockTransport::pair();
        let client = InteractiveClient::from_transport(transport);

        let output = Ok(HookJSONOutput::Sync(SyncHookJSONOutput {
            continue_: Some(true),
            suppress_output: None,
            stop_reason: None,
            decision: None,
            system_message: None,
            reason: None,
            hook_specific_output: None,
        }));

        // send_hook_response falls back to send_sdk_control_response (no stdin_tx in mock)
        client.send_hook_response("req-456", &output).await.unwrap();

        // The response should be observable via outbound_control_rx
        let msg = handle
            .outbound_control_rx
            .recv()
            .await
            .expect("Should have received response");

        // MockTransport wraps in {"type": "control_response", "response": ...}
        assert_eq!(msg["type"], "control_response");
        let response = &msg["response"];
        assert_eq!(response["subtype"], "success");
        assert_eq!(response["request_id"], "req-456");
        // The inner response should have the hook output
        let inner = &response["response"];
        assert_eq!(inner["continue"], true);
    }

    #[tokio::test]
    async fn test_send_hook_response_error_format() {
        let (transport, mut handle) = MockTransport::pair();
        let client = InteractiveClient::from_transport(transport);

        let output: std::result::Result<HookJSONOutput, SdkError> =
            Err(SdkError::ConnectionError("Hook failed".to_string()));

        client.send_hook_response("req-789", &output).await.unwrap();

        let msg = handle
            .outbound_control_rx
            .recv()
            .await
            .expect("Should have received error response");

        assert_eq!(msg["type"], "control_response");
        let response = &msg["response"];
        assert_eq!(response["subtype"], "error");
        assert_eq!(response["request_id"], "req-789");
        // Error string should be present
        let error_str = response["error"].as_str().unwrap();
        assert!(
            error_str.contains("Hook failed"),
            "Error should contain message"
        );
    }

    #[tokio::test]
    async fn test_initialize_hooks_multiple_events_and_matchers() {
        let (transport, mut handle) = MockTransport::pair();

        let cb1 = Arc::new(TestHookCallback::new()) as Arc<dyn HookCallback>;
        let cb2 = Arc::new(TestHookCallback::new()) as Arc<dyn HookCallback>;
        let cb3 = Arc::new(TestHookCallback::new()) as Arc<dyn HookCallback>;

        let mut hooks: HashMap<String, Vec<HookMatcher>> = HashMap::new();
        hooks.insert(
            "PreCompact".to_string(),
            vec![HookMatcher {
                matcher: None,
                hooks: vec![cb1],
            }],
        );
        hooks.insert(
            "PreToolUse".to_string(),
            vec![
                HookMatcher {
                    matcher: Some(serde_json::json!({"tool_name": "Bash"})),
                    hooks: vec![cb2],
                },
                HookMatcher {
                    matcher: None,
                    hooks: vec![cb3],
                },
            ],
        );

        let client = InteractiveClient::from_transport_with_hooks(transport, hooks);
        client.initialize_hooks().await.unwrap();

        let msg = handle.outbound_control_request_rx.recv().await.unwrap();

        let hooks_json = msg["request"]["hooks"].as_object().unwrap();
        assert!(hooks_json.contains_key("PreCompact"));
        assert!(hooks_json.contains_key("PreToolUse"));

        // PreCompact: 1 matcher, 1 callback
        let pc = hooks_json["PreCompact"].as_array().unwrap();
        assert_eq!(pc.len(), 1);
        assert_eq!(pc[0]["hookCallbackIds"].as_array().unwrap().len(), 1);

        // PreToolUse: 2 matchers, 1 callback each
        let ptu = hooks_json["PreToolUse"].as_array().unwrap();
        assert_eq!(ptu.len(), 2);
        assert_eq!(ptu[0]["hookCallbackIds"].as_array().unwrap().len(), 1);
        assert_eq!(ptu[1]["hookCallbackIds"].as_array().unwrap().len(), 1);
        // First matcher should have the tool_name filter
        assert_eq!(ptu[0]["matcher"]["tool_name"], "Bash");
        // Second matcher should be null
        assert!(ptu[1]["matcher"].is_null());

        // Total callbacks registered: 3
        let callbacks = client.hook_callbacks.read().await;
        assert_eq!(callbacks.len(), 3);
    }

    // ================================================================
    // Tests for build_interrupt_json()
    // ================================================================

    #[test]
    fn test_build_interrupt_json_has_correct_structure() {
        let json_str = InteractiveClient::build_interrupt_json();
        let parsed: serde_json::Value =
            serde_json::from_str(&json_str).expect("should be valid JSON");

        // Top-level type must be "control_request"
        assert_eq!(parsed["type"], "control_request");

        // Must have a "request" object
        let request = parsed.get("request").expect("should have 'request' field");
        assert!(request.is_object(), "'request' should be an object");

        // request.type must be "interrupt"
        assert_eq!(request["type"], "interrupt");

        // request.request_id must be a non-empty string (UUID)
        let request_id = request["request_id"]
            .as_str()
            .expect("request_id should be a string");
        assert!(!request_id.is_empty(), "request_id should not be empty");

        // request_id should be a valid UUID
        uuid::Uuid::parse_str(request_id).expect("request_id should be a valid UUID");
    }

    #[test]
    fn test_build_interrupt_json_matches_transport_format() {
        // The wire format produced by build_interrupt_json() must be identical
        // to what SubprocessTransport::send_control_request() produces for
        // ControlRequest::Interrupt.
        //
        // The transport builds:
        // {
        //   "type": "control_request",
        //   "request": {
        //     "type": "interrupt",
        //     "request_id": "<uuid>"
        //   }
        // }
        let json_str = InteractiveClient::build_interrupt_json();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        // Verify exact key set at top level: only "type" and "request"
        let obj = parsed.as_object().unwrap();
        assert_eq!(obj.len(), 2, "top-level should have exactly 2 keys");
        assert!(obj.contains_key("type"));
        assert!(obj.contains_key("request"));

        // Verify exact key set in request: only "type" and "request_id"
        let request = parsed["request"].as_object().unwrap();
        assert_eq!(request.len(), 2, "request should have exactly 2 keys");
        assert!(request.contains_key("type"));
        assert!(request.contains_key("request_id"));
    }

    #[test]
    fn test_build_interrupt_json_generates_unique_ids() {
        let json1 = InteractiveClient::build_interrupt_json();
        let json2 = InteractiveClient::build_interrupt_json();

        let parsed1: serde_json::Value = serde_json::from_str(&json1).unwrap();
        let parsed2: serde_json::Value = serde_json::from_str(&json2).unwrap();

        let id1 = parsed1["request"]["request_id"].as_str().unwrap();
        let id2 = parsed2["request"]["request_id"].as_str().unwrap();

        assert_ne!(id1, id2, "Each call should produce a unique request_id");
    }

    #[test]
    fn test_build_interrupt_json_is_sendable_via_stdin() {
        // Verify the output is a single-line JSON string (no newlines)
        // that can be sent directly via stdin_tx
        let json_str = InteractiveClient::build_interrupt_json();
        assert!(
            !json_str.contains('\n'),
            "JSON should be a single line for stdin transport"
        );
        assert!(!json_str.is_empty(), "JSON should not be empty");
    }
}
