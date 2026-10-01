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
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};

/// The turn cannot complete: the CLI's message stream ended before a
/// [`Message::Result`].
///
/// `send_and_receive` and `receive_response` used to answer this case with an
/// endless `sleep(10 ms)` loop, so a CLI that died mid-turn made the caller
/// wait forever.
fn stream_ended_before_result() -> SdkError {
    SdkError::TransportError(
        "the CLI message stream ended before a Result message: the turn cannot complete".into(),
    )
}

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
    /// The CLI's stdin writer, cloned once at `connect()` and cleared at
    /// `disconnect()`.
    ///
    /// `send_hook_response` writes through this clone, so it never has to take
    /// the transport mutex just to ask for it. It MUST be cleared on
    /// `disconnect`, or hook answers would go on being queued on a channel the
    /// CLI no longer reads.
    stdin_tx: Option<tokio::sync::mpsc::Sender<String>>,
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
            stdin_tx: None,
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
            stdin_tx: None,
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
            stdin_tx: None,
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

    /// Subscribe to the message broadcast for **out-of-band** consumption.
    ///
    /// Returns a `'static` stream that can be held by a long-lived task
    /// running in parallel with `send_and_receive`/`send_and_receive_stream`
    /// calls. Each subscriber sees every `Message` produced by the CLI
    /// subprocess after `connect()`, including those emitted spontaneously
    /// between turns (e.g. background tool notifications, system events).
    ///
    /// Unlike `receive_messages_stream` (which borrows `&mut self` for the
    /// stream's lifetime), this method only borrows briefly while taking the
    /// transport lock to call `subscribe_messages`. The returned stream is
    /// independent of the transport lock and can survive arbitrary other
    /// operations on the client.
    ///
    /// Returns `None` if the underlying transport doesn't expose a broadcast
    /// (e.g. mock transport, or `connect()` was not called yet).
    pub async fn subscribe_messages(
        &self,
    ) -> Option<Pin<Box<dyn Stream<Item = Result<Message>> + Send + 'static>>> {
        let transport = self.transport.lock().await;
        transport.subscribe_messages()
    }

    /// Connect to Claude
    pub async fn connect(&mut self) -> Result<()> {
        if self.connected {
            return Ok(());
        }

        let stdin_tx = {
            let mut transport = self.transport.lock().await;
            transport.connect().await?;
            // Clone the stdin writer while we already hold the mutex, so
            // `send_hook_response` never needs it again.
            transport.clone_stdin_sender()
        }; // Lock released immediately

        self.stdin_tx = stdin_tx;
        self.connected = true;
        info!("Connected to Claude CLI");
        Ok(())
    }

    /// Send a message and collect every message of the turn, up to and
    /// including the terminal [`Message::Result`].
    ///
    /// Like [`Self::send_and_receive_stream`], this subscribes to the message
    /// stream **before** sending and keeps that one subscription for the whole
    /// turn: a `broadcast` replays nothing, so a subscription dropped between
    /// two messages loses whatever the CLI printed in between.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError::InvalidState`] before `connect()`, whatever the
    /// transport reports while writing or reading, and
    /// [`SdkError::TransportError`] if the message stream ends before a
    /// `Result` message — which is what a dead CLI looks like from here.
    pub async fn send_and_receive(&mut self, prompt: String) -> Result<Vec<Message>> {
        if !self.connected {
            return Err(SdkError::InvalidState {
                message: "Not connected".into(),
            });
        }

        // Subscribe and send under the SAME lock acquisition, so the response
        // cannot land before the subscription exists.
        let mut stream = {
            let mut transport = self.transport.lock().await;
            let stream = transport.receive_messages();
            let message = InputMessage::user(prompt, "default".to_string());
            transport.send_message(message).await?;
            stream
        }; // Lock released here, after subscription and send

        debug!("Message sent, subscription active");

        let mut messages = Vec::new();
        while let Some(result) = stream.next().await {
            match result {
                Ok(msg) => {
                    debug!("Received: {:?}", msg);
                    let is_result = matches!(msg, Message::Result { .. });
                    messages.push(msg);
                    if is_result {
                        return Ok(messages);
                    }
                },
                Err(e) => return Err(e),
            }
        }

        Err(stream_ended_before_result())
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

    /// Collect the messages of the current turn without sending anything,
    /// up to and including the terminal [`Message::Result`].
    ///
    /// Subscribes once and keeps that subscription until the turn ends, for
    /// the same reason as [`Self::send_and_receive`].
    ///
    /// # Errors
    ///
    /// Returns [`SdkError::InvalidState`] before `connect()`, whatever the
    /// transport reports, and [`SdkError::TransportError`] if the stream ends
    /// before a `Result` message.
    pub async fn receive_response(&mut self) -> Result<Vec<Message>> {
        if !self.connected {
            return Err(SdkError::InvalidState {
                message: "Not connected".into(),
            });
        }

        // One subscription for the whole turn: see `send_and_receive`.
        let mut stream = {
            let mut transport = self.transport.lock().await;
            transport.receive_messages()
        };

        let mut messages = Vec::new();
        while let Some(result) = stream.next().await {
            match result {
                Ok(msg) => {
                    debug!("Received: {:?}", msg);
                    let is_result = matches!(msg, Message::Result { .. });
                    messages.push(msg);
                    if is_result {
                        return Ok(messages);
                    }
                },
                Err(e) => return Err(e),
            }
        }

        Err(stream_ended_before_result())
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

        // Subscribe here, under a short-lived guard. The relay below must NOT
        // hold the transport mutex: it only exits when `tx.send` fails, i.e. at
        // the message *after* the caller dropped the stream, so a guard held
        // inside the task starves every other method of the client until one
        // more message happens to arrive.
        let mut stream = {
            let mut transport = self.transport.lock().await;
            transport.receive_messages()
        };

        // Spawn a task to forward the already-subscribed stream
        tokio::spawn(async move {
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
            let tool_use_id = match manual_tool_use_id(request_data) {
                Ok(id) => id,
                // Refuse rather than pretend there is no tool at all.
                Err(e) => return Some(Err(e)),
            };
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
    /// `HookJSONOutput`.
    ///
    /// **Lock-free on a connected client**: it writes through the stdin sender
    /// cloned once by `connect()`, so it neither holds nor takes the transport
    /// mutex. A task that keeps that mutex — a caller streaming a turn, say —
    /// cannot delay a hook answer.
    ///
    /// On a client that was never connected, or one already disconnected, there
    /// is no cached sender and the method falls back to
    /// `Transport::send_sdk_control_response`, which does take the mutex
    /// briefly.
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

        // Use the sender cached at connect() — no mutex on this path at all.
        if let Some(tx) = &self.stdin_tx {
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

        // The CLI's stdin is closed: a cached sender would now be a channel
        // nobody reads.
        self.stdin_tx = None;
        self.connected = false;
        info!("Disconnected from Claude CLI");
        Ok(())
    }
}

// ============================================================================
// Standalone hook helpers (for use without client lock)
// ============================================================================

/// Read `tool_use_id` (or `toolUseId`) off a `hook_callback` request, refusing a
/// value that is present but is not a string.
///
/// This is the manual fallback both dispatchers take when
/// `SDKHookCallbackRequest` fails to deserialize — and an ill-typed
/// `tool_use_id` is one of the few things that makes it fail. The fallback used
/// to call `.as_str()` and hand the callback `None`, i.e. tell a `PreToolUse`
/// hook that there is no tool at all while the CLI was asking about one. The
/// request is malformed, whichever callback it names, so it is answered with an
/// `error` control response instead: the CLI gets a definite answer, and no hook
/// ever decides about a tool it could not identify.
fn manual_tool_use_id(request_data: &serde_json::Value) -> Result<Option<String>> {
    match request_data
        .get("tool_use_id")
        .or_else(|| request_data.get("toolUseId"))
    {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(id)) => Ok(Some(id.clone())),
        Some(other) => Err(SdkError::MessageParseError {
            error: format!("hook_callback tool_use_id must be a string, got {other}"),
            raw: other.to_string(),
        }),
    }
}

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
            let tool_use_id = match manual_tool_use_id(request_data) {
                Ok(id) => id,
                // Refuse rather than pretend there is no tool at all.
                Err(e) => return Some(Err(e)),
            };
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
        /// The `tool_use_id` the dispatcher handed to the last call.
        last_tool_use_id: Arc<Mutex<Option<String>>>,
    }

    impl TestHookCallback {
        fn new() -> Self {
            Self {
                call_count: Arc::new(Mutex::new(0)),
                last_tool_use_id: Arc::new(Mutex::new(None)),
            }
        }

        async fn calls(&self) -> u32 {
            *self.call_count.lock().await
        }

        /// What the dispatcher extracted as `tool_use_id` on the last call.
        async fn last_tool_use_id(&self) -> Option<String> {
            self.last_tool_use_id.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl HookCallback for TestHookCallback {
        async fn execute(
            &self,
            _input: &HookInput,
            tool_use_id: Option<&str>,
            _context: &HookContext,
        ) -> std::result::Result<HookJSONOutput, SdkError> {
            *self.last_tool_use_id.lock().await = tool_use_id.map(str::to_string);
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

    // ========================================================================
    // A scripted transport for the seams no shipped transport can produce
    // ========================================================================

    /// One item of a scripted `receive_messages()` stream.
    #[derive(Clone)]
    enum Scripted {
        /// A message handed to the caller.
        Msg(Message),
        /// A transport-level failure.
        ///
        /// Neither shipped transport can produce one: `SubprocessTransport`
        /// broadcasts a `broadcast::Sender<Message>` (parse failures are logged
        /// and dropped, never forwarded) and `MockTransport` filters
        /// `BroadcastStreamRecvError` away. The `Err(e)` arms of
        /// `send_and_receive`/`receive_response`/the two streams are therefore
        /// only reachable through a transport written for the purpose.
        Fail(String),
    }

    /// Everything a test needs to observe a [`ScriptedTransport`] after the
    /// client has taken ownership of it.
    struct ScriptedHandle {
        /// Items not yet pulled by `receive_messages()`.
        queue: Arc<std::sync::Mutex<std::collections::VecDeque<Scripted>>>,
        /// Prompts the client pushed through `send_message`.
        sent: Arc<std::sync::Mutex<Vec<InputMessage>>>,
        /// How many times `connect()` reached the transport.
        connects: Arc<std::sync::atomic::AtomicUsize>,
        /// Lines written through `clone_stdin_sender`, when one was wired.
        stdin_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    }

    /// A transport with every seam under the test's control: a stream that can
    /// fail or end, a `send_message` that can refuse, a real stdin channel and
    /// a pid.
    struct ScriptedTransport {
        queue: Arc<std::sync::Mutex<std::collections::VecDeque<Scripted>>>,
        sent: Arc<std::sync::Mutex<Vec<InputMessage>>>,
        connects: Arc<std::sync::atomic::AtomicUsize>,
        send_failure: Option<String>,
        stdin_tx: Option<tokio::sync::mpsc::Sender<String>>,
        pid: Option<u32>,
        connected: bool,
    }

    /// Builder for [`ScriptedTransport`].
    struct ScriptedBuilder {
        items: Vec<Scripted>,
        send_failure: Option<String>,
        stdin: bool,
        pid: Option<u32>,
    }

    impl ScriptedBuilder {
        /// An empty script: `receive_messages()` ends immediately.
        fn new() -> Self {
            Self {
                items: Vec::new(),
                send_failure: None,
                stdin: false,
                pid: None,
            }
        }

        /// Queue one message.
        fn msg(mut self, message: Message) -> Self {
            self.items.push(Scripted::Msg(message));
            self
        }

        /// Queue one transport failure.
        fn fail(mut self, why: &str) -> Self {
            self.items.push(Scripted::Fail(why.to_string()));
            self
        }

        /// Make `send_message` refuse with a `ConnectionError`.
        fn send_fails(mut self, why: &str) -> Self {
            self.send_failure = Some(why.to_string());
            self
        }

        /// Expose a stdin sender, like a connected `SubprocessTransport`.
        fn with_stdin(mut self) -> Self {
            self.stdin = true;
            self
        }

        /// Expose a child pid, like a connected `SubprocessTransport`.
        fn with_pid(mut self, pid: u32) -> Self {
            self.pid = Some(pid);
            self
        }

        fn build(self) -> (Box<dyn Transport + Send>, ScriptedHandle) {
            let queue = Arc::new(std::sync::Mutex::new(
                self.items
                    .into_iter()
                    .collect::<std::collections::VecDeque<_>>(),
            ));
            let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
            let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (stdin_tx, stdin_rx) = if self.stdin {
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
            let transport = ScriptedTransport {
                queue: queue.clone(),
                sent: sent.clone(),
                connects: connects.clone(),
                send_failure: self.send_failure,
                stdin_tx,
                pid: self.pid,
                connected: false,
            };
            (
                Box::new(transport),
                ScriptedHandle {
                    queue,
                    sent,
                    connects,
                    stdin_rx,
                },
            )
        }
    }

    #[async_trait::async_trait]
    impl Transport for ScriptedTransport {
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        async fn connect(&mut self) -> Result<()> {
            self.connects
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.connected = true;
            Ok(())
        }

        async fn send_message(&mut self, message: InputMessage) -> Result<()> {
            if let Some(why) = &self.send_failure {
                return Err(SdkError::ConnectionError(why.clone()));
            }
            self.sent.lock().expect("sent lock").push(message);
            Ok(())
        }

        fn receive_messages(
            &mut self,
        ) -> Pin<Box<dyn Stream<Item = Result<Message>> + Send + 'static>> {
            let queue = self.queue.clone();
            Box::pin(futures::stream::unfold(queue, |queue| async move {
                let next = queue.lock().expect("queue lock").pop_front();
                match next {
                    Some(Scripted::Msg(message)) => Some((Ok(message), queue)),
                    Some(Scripted::Fail(why)) => Some((Err(SdkError::ConnectionError(why)), queue)),
                    None => None,
                }
            }))
        }

        async fn send_control_request(&mut self, _request: ControlRequest) -> Result<()> {
            Ok(())
        }

        async fn receive_control_response(
            &mut self,
        ) -> Result<Option<crate::types::ControlResponse>> {
            Ok(None)
        }

        async fn send_sdk_control_request(&mut self, _request: serde_json::Value) -> Result<()> {
            Ok(())
        }

        async fn send_sdk_control_response(&mut self, _response: serde_json::Value) -> Result<()> {
            Ok(())
        }

        fn clone_stdin_sender(&self) -> Option<tokio::sync::mpsc::Sender<String>> {
            self.stdin_tx.clone()
        }

        fn child_pid(&self) -> Option<u32> {
            self.pid
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        async fn disconnect(&mut self) -> Result<()> {
            self.connected = false;
            Ok(())
        }
    }

    /// A `system` message, the cheapest non-terminal message.
    fn system_message(subtype: &str) -> Message {
        Message::System {
            subtype: subtype.to_string(),
            data: serde_json::json!({"session_id": "scripted"}),
        }
    }

    /// The terminal `result` message every receive loop stops on.
    fn result_message(text: &str) -> Message {
        Message::Result {
            subtype: "success".to_string(),
            duration_ms: 1,
            duration_api_ms: 1,
            is_error: false,
            num_turns: 1,
            session_id: "scripted".to_string(),
            total_cost_usd: None,
            usage: None,
            result: Some(text.to_string()),
            structured_output: None,
        }
    }

    /// The `subtype` of a `system` message, for assertions.
    fn system_subtypes(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|m| match m {
                Message::System { subtype, .. } => Some(subtype.clone()),
                _ => None,
            })
            .collect()
    }

    /// Assert the error is the `Not connected` guard, not some other failure.
    fn assert_not_connected(error: Option<SdkError>) {
        match error {
            Some(SdkError::InvalidState { message }) => assert_eq!(message, "Not connected"),
            other => panic!("expected InvalidState {{ Not connected }}, got {other:?}"),
        }
    }

    /// Wait until the broadcast behind a `MockTransport` has `expected`
    /// subscribers, so a test never races the task that subscribes.
    async fn await_subscribers(tx: &tokio::sync::broadcast::Sender<Message>, expected: usize) {
        for _ in 0..500 {
            if tx.receiver_count() == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        panic!(
            "broadcast still has {} subscribers, expected {expected}",
            tx.receiver_count()
        );
    }

    // ========================================================================
    // Connection guards
    // ========================================================================

    #[tokio::test]
    async fn every_turn_operation_is_refused_before_connect() {
        let (transport, _handle) = MockTransport::pair();
        let mut client = InteractiveClient::from_transport(transport);

        assert_not_connected(client.send_and_receive("hi".to_string()).await.err());
        assert_not_connected(client.send_message("hi".to_string()).await.err());
        assert_not_connected(
            client
                .send_control_response(serde_json::json!({"allow": true}))
                .await
                .err(),
        );
        assert_not_connected(client.receive_response().await.err());
        assert_not_connected(client.set_permission_mode("plan").await.err());
        assert_not_connected(client.interrupt().await.err());
        {
            // The stream borrows the client, so scope it.
            let stream = client.send_and_receive_stream("hi".to_string()).await;
            assert_not_connected(stream.err());
        }

        // `disconnect` is the one method that answers Ok when there is nothing
        // to disconnect — it must stay idempotent for Drop-style cleanup.
        client.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn connect_and_disconnect_are_idempotent_and_reach_the_transport_once() {
        let (transport, handle) = ScriptedBuilder::new().build();
        let mut client = InteractiveClient::from_transport(transport);

        client.connect().await.unwrap();
        client.connect().await.unwrap();
        assert_eq!(
            handle.connects.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the second connect must short-circuit on self.connected"
        );
        assert!(client.connected);

        client.disconnect().await.unwrap();
        assert!(!client.connected);
        // Second disconnect short-circuits too.
        client.disconnect().await.unwrap();
        assert!(!client.connected);

        // And a reconnect after a disconnect does reach the transport again.
        client.connect().await.unwrap();
        assert_eq!(handle.connects.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    // ========================================================================
    // send_message / send_control_response
    // ========================================================================

    #[tokio::test]
    async fn send_message_wraps_the_prompt_in_a_default_session_user_message() {
        let (transport, mut handle) = MockTransport::pair();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        client.send_message("bonjour".to_string()).await.unwrap();

        let sent = handle
            .sent_input_rx
            .recv()
            .await
            .expect("one input message");
        assert_eq!(sent.r#type, "user");
        assert_eq!(sent.message["role"], "user");
        assert_eq!(sent.message["content"], "bonjour");
        assert_eq!(
            sent.session_id, "default",
            "the client hardcodes the session id instead of using the CLI's"
        );
        assert!(sent.parent_tool_use_id.is_none());
    }

    #[tokio::test]
    async fn send_message_propagates_a_transport_write_failure() {
        let (transport, _handle) = ScriptedBuilder::new().send_fails("broken pipe").build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let error = client.send_message("hi".to_string()).await.unwrap_err();
        assert!(
            matches!(&error, SdkError::ConnectionError(why) if why == "broken pipe"),
            "got {error:?}"
        );
        // The failure does not flip the client back to disconnected.
        assert!(client.connected);
    }

    #[tokio::test]
    async fn send_and_receive_propagates_a_transport_write_failure_before_waiting() {
        let (transport, handle) = ScriptedBuilder::new()
            .send_fails("stdin closed")
            .msg(result_message("never read"))
            .build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let error = client.send_and_receive("hi".to_string()).await.unwrap_err();
        assert!(
            matches!(&error, SdkError::ConnectionError(why) if why == "stdin closed"),
            "got {error:?}"
        );
        assert_eq!(
            handle.queue.lock().expect("queue").len(),
            1,
            "a failed send must not consume the response stream"
        );
    }

    #[tokio::test]
    async fn send_control_response_is_delegated_verbatim_to_the_transport() {
        let (transport, mut handle) = MockTransport::pair();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        client
            .send_control_response(serde_json::json!({"allow": false, "reason": "nope"}))
            .await
            .unwrap();

        let sent = handle.outbound_control_rx.recv().await.expect("a response");
        assert_eq!(sent["type"], "control_response");
        assert_eq!(sent["response"]["allow"], false);
        assert_eq!(sent["response"]["reason"], "nope");
    }

    // ========================================================================
    // send_and_receive
    // ========================================================================

    #[tokio::test]
    async fn send_and_receive_collects_until_the_result_and_leaves_the_rest() {
        let (transport, handle) = ScriptedBuilder::new()
            .msg(system_message("init"))
            .msg(result_message("done"))
            .msg(system_message("after-the-turn"))
            .build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let messages = client.send_and_receive("hi".to_string()).await.unwrap();

        assert_eq!(system_subtypes(&messages), vec!["init".to_string()]);
        assert!(matches!(messages.last(), Some(Message::Result { .. })));
        assert_eq!(messages.len(), 2);
        assert_eq!(
            handle.queue.lock().expect("queue").len(),
            1,
            "the message after the result stays in the stream for the next turn"
        );
        let sent = handle.sent.lock().expect("sent");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].message["content"], "hi");
    }

    #[tokio::test]
    async fn send_and_receive_drops_the_messages_it_already_collected_when_the_stream_fails() {
        let (transport, _handle) = ScriptedBuilder::new()
            .msg(system_message("init"))
            .fail("cli died mid-turn")
            .build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let error = client.send_and_receive("hi".to_string()).await.unwrap_err();
        assert!(
            matches!(&error, SdkError::ConnectionError(why) if why == "cli died mid-turn"),
            "got {error:?}"
        );
        // Documented consequence of `Err(e) => return Err(e)`: the `init`
        // message collected before the failure is thrown away with the Vec.
    }

    #[tokio::test]
    async fn send_and_receive_reports_a_stream_that_ends_without_a_result() {
        // This used to be the infinite `else { sleep 10 ms }` branch: the loop
        // consumed the script, then polled an exhausted stream forever.
        let (transport, handle) = ScriptedBuilder::new().msg(system_message("init")).build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.send_and_receive("hi".to_string()),
        )
        .await
        .expect("an exhausted stream must end the turn, not spin on a 10 ms sleep")
        .expect_err("there was no Result message to return");

        assert!(
            matches!(&error, SdkError::TransportError(why) if why.contains("ended before a Result")),
            "got {error:?}"
        );
        assert!(
            handle.queue.lock().expect("queue").is_empty(),
            "the whole script was consumed before the stream ended"
        );
    }

    #[tokio::test]
    async fn send_and_receive_cannot_see_what_was_broadcast_before_the_call() {
        // The subscription now opens inside the call, before the prompt is
        // written, and stays open for the whole turn — but a tokio broadcast
        // still replays nothing, so what the CLI printed *before* the call is
        // gone for good. A caller who needs that history must hold a
        // `subscribe_messages()` stream across turns instead.
        let (transport, handle) = MockTransport::pair();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        handle.inbound_message_tx.send(system_message("init")).ok();
        handle.inbound_message_tx.send(result_message("done")).ok();
        assert_eq!(
            handle.inbound_message_tx.receiver_count(),
            0,
            "nothing is subscribed yet, so both messages were dropped on the floor"
        );

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(150),
            client.send_and_receive("hi".to_string()),
        )
        .await;
        assert!(
            outcome.is_err(),
            "the result message was already lost, so the turn can never complete; got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn send_and_receive_keeps_one_subscription_for_the_whole_turn() {
        // The two messages are broadcast back to back. The old loop dropped its
        // subscription after the first one and a tokio broadcast replays
        // nothing, so `done` was lost and the turn never ended; now a single
        // subscription spans the turn and both messages arrive.
        let (transport, handle) = MockTransport::pair();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let tx = handle.inbound_message_tx.clone();
        let feeder = tokio::spawn(async move {
            await_subscribers(&tx, 1).await;
            tx.send(system_message("init")).expect("subscribed");
            tx.send(result_message("done")).expect("subscribed");
        });

        let messages = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.send_and_receive("hi".to_string()),
        )
        .await
        .expect("a burst of messages must not fall outside the subscription window")
        .expect("no transport error");
        feeder.await.expect("feeder");

        assert_eq!(system_subtypes(&messages), vec!["init".to_string()]);
        assert!(matches!(messages.last(), Some(Message::Result { .. })));
        assert_eq!(
            handle.inbound_message_tx.receiver_count(),
            0,
            "the turn's subscription is dropped when the turn ends"
        );
    }

    // ========================================================================
    // send_and_receive_stream
    // ========================================================================

    #[tokio::test]
    async fn send_and_receive_stream_yields_until_the_result_then_stops() {
        let (transport, handle) = ScriptedBuilder::new()
            .msg(system_message("init"))
            .msg(result_message("done"))
            .msg(system_message("after-the-turn"))
            .build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let stream = client
            .send_and_receive_stream("hi".to_string())
            .await
            .unwrap();
        let mut stream = std::pin::pin!(stream);
        let mut collected = Vec::new();
        while let Some(item) = stream.next().await {
            collected.push(item.expect("no transport error"));
        }

        assert_eq!(collected.len(), 2, "stops on the Result message");
        assert_eq!(system_subtypes(&collected), vec!["init".to_string()]);
        assert!(matches!(collected.last(), Some(Message::Result { .. })));
        let sent = handle.sent.lock().expect("sent");
        assert_eq!(sent.len(), 1, "the prompt was sent exactly once");
    }

    #[tokio::test]
    async fn send_and_receive_stream_yields_the_failure_then_stops() {
        let (transport, _handle) = ScriptedBuilder::new()
            .msg(system_message("init"))
            .fail("cli died mid-turn")
            .msg(result_message("unreachable"))
            .build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let stream = client
            .send_and_receive_stream("hi".to_string())
            .await
            .unwrap();
        let mut stream = std::pin::pin!(stream);

        let first = stream.next().await.expect("the init message").unwrap();
        assert_eq!(system_subtypes(&[first]), vec!["init".to_string()]);
        let error = stream
            .next()
            .await
            .expect("the failure is yielded, not swallowed")
            .unwrap_err();
        assert!(
            matches!(&error, SdkError::ConnectionError(why) if why == "cli died mid-turn"),
            "got {error:?}"
        );
        assert!(
            stream.next().await.is_none(),
            "a failure ends the turn — the result message after it is never yielded"
        );
    }

    #[tokio::test]
    async fn send_and_receive_stream_stops_forwarding_when_the_caller_drops_the_stream() {
        let (transport, handle) = MockTransport::pair();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        {
            let stream = client
                .send_and_receive_stream("hi".to_string())
                .await
                .unwrap();
            // The forwarding task subscribed inside the lock before we got here.
            await_subscribers(&handle.inbound_message_tx, 1).await;
            drop(stream);
        }

        // The forwarder only notices the dropped receiver on its next send, so
        // it takes one more message to make it exit.
        handle.inbound_message_tx.send(system_message("late")).ok();
        await_subscribers(&handle.inbound_message_tx, 0).await;
    }

    // ========================================================================
    // receive_response
    // ========================================================================

    #[tokio::test]
    async fn receive_response_collects_until_the_result_without_sending_anything() {
        let (transport, handle) = ScriptedBuilder::new()
            .msg(system_message("init"))
            .msg(result_message("done"))
            .build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let messages = client.receive_response().await.unwrap();

        assert_eq!(messages.len(), 2);
        assert!(matches!(messages.last(), Some(Message::Result { .. })));
        assert!(
            handle.sent.lock().expect("sent").is_empty(),
            "receive_response must not write to the CLI"
        );
    }

    #[tokio::test]
    async fn receive_response_propagates_a_stream_failure() {
        let (transport, _handle) = ScriptedBuilder::new().fail("read error").build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let error = client.receive_response().await.unwrap_err();
        assert!(
            matches!(&error, SdkError::ConnectionError(why) if why == "read error"),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn receive_response_reports_a_stream_that_ends_without_a_result() {
        let (transport, _handle) = ScriptedBuilder::new().msg(system_message("init")).build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let error =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.receive_response())
                .await
                .expect("an exhausted stream must not leave receive_response in a 10 ms busy loop")
                .expect_err("there was no Result message to return");
        assert!(
            matches!(&error, SdkError::TransportError(why) if why.contains("ended before a Result")),
            "got {error:?}"
        );
    }

    // ========================================================================
    // receive_messages_stream / receive_response_stream
    // ========================================================================

    #[tokio::test]
    async fn receive_messages_stream_forwards_every_message_including_after_the_result() {
        let (transport, _handle) = ScriptedBuilder::new()
            .msg(system_message("init"))
            .msg(result_message("done"))
            .msg(system_message("after-the-turn"))
            .build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let stream = client.receive_messages_stream().await;
        let mut stream = std::pin::pin!(stream);
        let mut collected = Vec::new();
        while let Some(item) = stream.next().await {
            collected.push(item.expect("no transport error"));
        }

        assert_eq!(
            collected.len(),
            3,
            "unlike receive_response, this stream does not stop on the Result"
        );
        assert_eq!(
            system_subtypes(&collected),
            vec!["init".to_string(), "after-the-turn".to_string()]
        );
    }

    #[tokio::test]
    async fn receive_messages_stream_leaves_the_transport_lock_free() {
        // The relay used to take `self.transport.lock()` and keep the guard for
        // its whole life, which starved every other method of the client until
        // one further message made its channel send fail. The subscription now
        // happens before the spawn, under a guard that is dropped immediately.
        let (transport, handle) = MockTransport::pair();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        drop(client.receive_messages_stream().await);
        await_subscribers(&handle.inbound_message_tx, 1).await;

        tokio::time::timeout(
            std::time::Duration::from_millis(150),
            client.send_message("hi".to_string()),
        )
        .await
        .expect("the relay owns a broadcast subscription, not the transport mutex")
        .expect("send_message succeeds");

        // What has NOT changed: the relay itself still only notices the dropped
        // receiver on its next send, so its subscription outlives the stream by
        // one message.
        assert_eq!(handle.inbound_message_tx.receiver_count(), 1);
        handle.inbound_message_tx.send(system_message("late")).ok();
        await_subscribers(&handle.inbound_message_tx, 0).await;
    }

    #[tokio::test]
    async fn receive_response_stream_stops_on_the_result_message() {
        let (transport, _handle) = ScriptedBuilder::new()
            .msg(system_message("init"))
            .msg(result_message("done"))
            .msg(system_message("after-the-turn"))
            .build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let stream = client.receive_response_stream().await;
        let mut stream = std::pin::pin!(stream);
        let mut collected = Vec::new();
        while let Some(item) = stream.next().await {
            collected.push(item.expect("no transport error"));
        }

        assert_eq!(collected.len(), 2);
        assert_eq!(system_subtypes(&collected), vec!["init".to_string()]);
        assert!(matches!(collected.last(), Some(Message::Result { .. })));
    }

    #[tokio::test]
    async fn receive_response_stream_yields_the_failure_then_stops() {
        let (transport, _handle) = ScriptedBuilder::new()
            .fail("read error")
            .msg(result_message("unreachable"))
            .build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let stream = client.receive_response_stream().await;
        let mut stream = std::pin::pin!(stream);
        let error = stream.next().await.expect("one item").unwrap_err();
        assert!(
            matches!(&error, SdkError::ConnectionError(why) if why == "read error"),
            "got {error:?}"
        );
        assert!(stream.next().await.is_none(), "a failure ends the stream");
    }

    // ========================================================================
    // Transport pass-throughs
    // ========================================================================

    #[tokio::test]
    async fn transport_passthroughs_report_what_the_transport_exposes() {
        // A transport with no subprocess behind it: no stdin, no pid, and the
        // default `subscribe_messages` (None).
        let (transport, _handle) = ScriptedBuilder::new().build();
        let client = InteractiveClient::from_transport(transport);
        assert!(client.clone_stdin_sender().await.is_none());
        assert!(client.child_pid().await.is_none());
        assert!(client.subscribe_messages().await.is_none());

        // A transport that does expose them.
        let (transport, mut handle) = ScriptedBuilder::new().with_stdin().with_pid(4242).build();
        let client = InteractiveClient::from_transport(transport);
        assert_eq!(client.child_pid().await, Some(4242));
        let stdin = client
            .clone_stdin_sender()
            .await
            .expect("the transport has a stdin channel");
        stdin.send("ping\n".to_string()).await.expect("write");
        assert_eq!(
            handle
                .stdin_rx
                .as_mut()
                .expect("stdin receiver")
                .recv()
                .await,
            Some("ping\n".to_string()),
            "the cloned sender writes to the real stdin channel"
        );
    }

    #[tokio::test]
    async fn subscribe_messages_returns_an_out_of_band_stream_when_the_transport_has_a_broadcast() {
        let (transport, handle) = MockTransport::pair();
        let client = InteractiveClient::from_transport(transport);

        let stream = client
            .subscribe_messages()
            .await
            .expect("MockTransport exposes its broadcast");
        let mut stream = std::pin::pin!(stream);

        handle
            .inbound_message_tx
            .send(system_message("out-of-band"))
            .expect("subscribed");
        let message = stream.next().await.expect("one message").unwrap();
        assert_eq!(system_subtypes(&[message]), vec!["out-of-band".to_string()]);
    }

    #[tokio::test]
    async fn hook_callbacks_hands_out_the_very_registry_dispatch_reads() {
        let (transport, _handle) = MockTransport::pair();
        let callback = Arc::new(TestHookCallback::new());
        let hooks = make_hooks_with_callback("PreCompact", callback);
        let client = InteractiveClient::from_transport_with_hooks(transport, hooks);
        client.initialize_hooks().await.unwrap();

        let registry = client.hook_callbacks();
        assert!(
            Arc::ptr_eq(&registry, &client.hook_callbacks),
            "the clone must share state with the client, not copy it"
        );
        assert_eq!(registry.read().await.len(), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn new_marks_the_sdk_entrypoint_and_carries_the_hooks_over() {
        // An explicit `cli_path` is taken on trust by `SubprocessTransport::new`,
        // so this needs no binary on disk and no network.
        let callback = Arc::new(TestHookCallback::new()) as Arc<dyn HookCallback>;
        let hooks = make_hooks_with_callback("PreToolUse", callback);
        let options = ClaudeCodeOptions::builder()
            .cli_path(std::path::PathBuf::from("definitely-not-a-real-cli"))
            .hooks(hooks)
            .build();

        let client = InteractiveClient::new(options).expect("an explicit cli_path is not probed");

        assert_eq!(
            std::env::var("CLAUDE_CODE_ENTRYPOINT").as_deref(),
            Ok("sdk-rust"),
            "new() advertises the SDK entrypoint to the CLI"
        );
        assert!(!client.connected, "new() does not spawn anything");
        assert!(
            client
                .hooks
                .as_ref()
                .is_some_and(|h| h.contains_key("PreToolUse")),
            "the hooks from the options must survive into the client"
        );
        assert!(
            client.hook_callbacks.read().await.is_empty(),
            "callback ids are only minted by initialize_hooks"
        );
    }

    // ========================================================================
    // dispatch_hook_callback — the manual fallback path
    // ========================================================================

    /// Register one callback and return the client plus its callback id.
    async fn client_with_one_callback(
        callback: Arc<TestHookCallback>,
    ) -> (InteractiveClient, String) {
        let (transport, _handle) = MockTransport::pair();
        let hooks = make_hooks_with_callback("PreCompact", callback);
        let client = InteractiveClient::from_transport_with_hooks(transport, hooks);
        client.initialize_hooks().await.unwrap();
        let id = client
            .hook_callbacks
            .read()
            .await
            .keys()
            .next()
            .expect("one callback id")
            .clone();
        (client, id)
    }

    /// A valid `PreCompact` hook input.
    fn pre_compact_input() -> serde_json::Value {
        serde_json::json!({
            "hook_event_name": "PreCompact",
            "session_id": "sess-1",
            "transcript_path": "transcript.json",
            "cwd": ".",
            "trigger": "auto"
        })
    }

    #[tokio::test]
    async fn dispatch_hook_callback_refuses_a_malformed_tool_use_id() {
        let callback = Arc::new(TestHookCallback::new());
        let (client, id) = client_with_one_callback(callback.clone()).await;

        // `tool_use_id` as a number makes `SDKHookCallbackRequest` deserialization
        // fail, which is what sends the code down the manual extraction path.
        let control_msg = serde_json::json!({
            "request": {
                "subtype": "hook_callback",
                "callbackId": id,
                "input": pre_compact_input(),
                "toolUseId": 42
            }
        });

        let error = client
            .dispatch_hook_callback(&control_msg)
            .await
            .expect("a malformed request is answered, not ignored")
            .expect_err("42 is not a tool use id");
        match error {
            SdkError::MessageParseError { error, raw } => {
                assert!(
                    error.contains("tool_use_id must be a string"),
                    "got {error}"
                );
                assert_eq!(raw, "42");
            },
            other => panic!("expected MessageParseError, got {other:?}"),
        }
        // The callback is never told "there is no tool" about a request that
        // named one.
        assert_eq!(callback.calls().await, 0);
        assert_eq!(callback.last_tool_use_id().await, None);
    }

    #[tokio::test]
    async fn dispatch_hook_callback_ignores_a_request_without_any_callback_id() {
        let callback = Arc::new(TestHookCallback::new());
        let (client, _id) = client_with_one_callback(callback.clone()).await;

        let control_msg = serde_json::json!({
            "request": {
                "subtype": "hook_callback",
                "input": pre_compact_input()
            }
        });

        assert!(
            client.dispatch_hook_callback(&control_msg).await.is_none(),
            "no callback_id, no dispatch"
        );
        assert_eq!(callback.calls().await, 0);
    }

    #[tokio::test]
    async fn dispatch_hook_callback_reports_an_unparseable_hook_input() {
        let callback = Arc::new(TestHookCallback::new());
        let (client, id) = client_with_one_callback(callback.clone()).await;

        // No `input` at all: the structured parse fails, the manual path
        // substitutes `{}`, and `{}` is not a HookInput.
        let control_msg = serde_json::json!({
            "request": {"subtype": "hook_callback", "callback_id": id}
        });

        let error = client
            .dispatch_hook_callback(&control_msg)
            .await
            .expect("the callback was found")
            .expect_err("an empty input cannot be a HookInput");
        match error {
            SdkError::MessageParseError { error, raw } => {
                assert!(error.contains("Invalid hook input"), "got {error}");
                assert_eq!(raw, "{}", "the default substituted for the missing input");
            },
            other => panic!("expected MessageParseError, got {other:?}"),
        }
        assert_eq!(callback.calls().await, 0);
    }

    #[tokio::test]
    async fn dispatch_hook_callback_reads_a_flat_control_message_too() {
        let callback = Arc::new(TestHookCallback::new());
        let (client, id) = client_with_one_callback(callback.clone()).await;

        // No `request` wrapper: the fields sit at the top level.
        let control_msg = serde_json::json!({
            "subtype": "hook_callback",
            "callback_id": id,
            "input": pre_compact_input(),
            "tool_use_id": "toolu_1"
        });

        client
            .dispatch_hook_callback(&control_msg)
            .await
            .expect("flat messages are supported")
            .expect("the callback ran");
        assert_eq!(callback.calls().await, 1);
        assert_eq!(
            callback.last_tool_use_id().await,
            Some("toolu_1".to_string())
        );
    }

    // ========================================================================
    // send_hook_response — the lock-free stdin path
    // ========================================================================

    #[tokio::test]
    async fn send_hook_response_writes_one_json_line_to_stdin_when_the_transport_has_one() {
        let (transport, mut handle) = ScriptedBuilder::new().with_stdin().build();
        let mut client = InteractiveClient::from_transport(transport);
        // The stdin writer is cached by connect(); before it there is none.
        client.connect().await.unwrap();

        let output = Ok(HookJSONOutput::Sync(SyncHookJSONOutput {
            continue_: Some(false),
            suppress_output: None,
            stop_reason: Some("stop".to_string()),
            decision: None,
            system_message: None,
            reason: None,
            hook_specific_output: None,
        }));
        client.send_hook_response("req-1", &output).await.unwrap();

        let line = handle
            .stdin_rx
            .as_mut()
            .expect("stdin")
            .recv()
            .await
            .expect("one line");
        assert!(!line.contains('\n'), "stdin lines are written one per line");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(parsed["type"], "control_response");
        assert_eq!(parsed["response"]["subtype"], "success");
        assert_eq!(parsed["response"]["request_id"], "req-1");
        assert_eq!(parsed["response"]["response"]["continue"], false);
        assert_eq!(parsed["response"]["response"]["stopReason"], "stop");
    }

    #[tokio::test]
    async fn send_hook_response_reports_a_closed_stdin_channel() {
        let (transport, handle) = ScriptedBuilder::new().with_stdin().build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();
        // Drop the receiving half: the CLI is gone.
        drop(handle.stdin_rx);

        let output: std::result::Result<HookJSONOutput, SdkError> =
            Err(SdkError::ConnectionError("hook blew up".to_string()));
        let error = client
            .send_hook_response("req-2", &output)
            .await
            .unwrap_err();
        match error {
            SdkError::ConnectionError(why) => assert!(
                why.starts_with("Failed to send hook response:"),
                "got {why}"
            ),
            other => panic!("expected ConnectionError, got {other:?}"),
        }
    }

    // ========================================================================
    // Standalone helpers
    // ========================================================================

    #[test]
    fn is_hook_callback_accepts_both_shapes_and_nothing_else() {
        assert!(is_hook_callback(&serde_json::json!({
            "request": {"subtype": "hook_callback"}
        })));
        assert!(is_hook_callback(&serde_json::json!({
            "subtype": "hook_callback"
        })));
        assert!(!is_hook_callback(&serde_json::json!({
            "request": {"subtype": "can_use_tool"}
        })));
        assert!(!is_hook_callback(&serde_json::json!({})));
        // A non-string subtype is not a hook callback either.
        assert!(!is_hook_callback(&serde_json::json!({"subtype": 7})));
    }

    /// A registry holding exactly one callback under `id`.
    fn registry_with(
        id: &str,
        callback: Arc<dyn HookCallback>,
    ) -> RwLock<HashMap<String, Arc<dyn HookCallback>>> {
        let mut map: HashMap<String, Arc<dyn HookCallback>> = HashMap::new();
        map.insert(id.to_string(), callback);
        RwLock::new(map)
    }

    #[tokio::test]
    async fn dispatch_hook_from_registry_matches_the_client_method_branch_for_branch() {
        let callback = Arc::new(TestHookCallback::new());
        let registry = registry_with("cb-1", callback.clone());

        // Not a hook callback at all.
        assert!(
            dispatch_hook_from_registry(
                &serde_json::json!({"request": {"subtype": "can_use_tool"}}),
                &registry
            )
            .await
            .is_none()
        );

        // Unknown callback id.
        assert!(
            dispatch_hook_from_registry(
                &serde_json::json!({
                    "request": {
                        "subtype": "hook_callback",
                        "callback_id": "nope",
                        "input": pre_compact_input()
                    }
                }),
                &registry
            )
            .await
            .is_none()
        );

        // No callback id.
        assert!(
            dispatch_hook_from_registry(
                &serde_json::json!({"subtype": "hook_callback", "input": pre_compact_input()}),
                &registry
            )
            .await
            .is_none()
        );
        assert_eq!(callback.calls().await, 0);

        // The manual fallback path, refusing an ill-typed tool_use_id.
        let error = dispatch_hook_from_registry(
            &serde_json::json!({
                "request": {
                    "subtype": "hook_callback",
                    "callbackId": "cb-1",
                    "input": pre_compact_input(),
                    "toolUseId": 42
                }
            }),
            &registry,
        )
        .await
        .expect("a malformed request is answered, not ignored")
        .expect_err("42 is not a tool use id");
        assert!(
            matches!(&error, SdkError::MessageParseError { error, .. } if error.contains("tool_use_id must be a string")),
            "got {error:?}"
        );
        assert_eq!(callback.calls().await, 0, "a refused request runs nothing");

        // The structured path, with a usable tool_use_id.
        dispatch_hook_from_registry(
            &serde_json::json!({
                "subtype": "hook_callback",
                "callback_id": "cb-1",
                "input": pre_compact_input(),
                "tool_use_id": "toolu_9"
            }),
            &registry,
        )
        .await
        .expect("found")
        .expect("ran");
        assert_eq!(callback.calls().await, 1);
        assert_eq!(
            callback.last_tool_use_id().await,
            Some("toolu_9".to_string())
        );

        // An input that is not a HookInput.
        let error = dispatch_hook_from_registry(
            &serde_json::json!({
                "subtype": "hook_callback",
                "callback_id": "cb-1",
                "input": {"hook_event_name": "NotAnEvent"}
            }),
            &registry,
        )
        .await
        .expect("found")
        .expect_err("unknown hook event");
        assert!(
            matches!(&error, SdkError::MessageParseError { error, .. } if error.contains("Invalid hook input")),
            "got {error:?}"
        );
        assert_eq!(callback.calls().await, 1, "a parse failure runs nothing");
    }

    #[test]
    fn build_hook_response_json_mirrors_send_hook_response_for_both_outcomes() {
        let ok = Ok(HookJSONOutput::Sync(SyncHookJSONOutput {
            continue_: Some(true),
            suppress_output: Some(true),
            stop_reason: None,
            decision: None,
            system_message: None,
            reason: None,
            hook_specific_output: None,
        }));
        let parsed: serde_json::Value =
            serde_json::from_str(&build_hook_response_json("req-ok", &ok)).expect("valid JSON");
        assert_eq!(parsed["type"], "control_response");
        assert_eq!(parsed["response"]["subtype"], "success");
        assert_eq!(parsed["response"]["request_id"], "req-ok");
        assert_eq!(parsed["response"]["response"]["continue"], true);
        assert_eq!(parsed["response"]["response"]["suppressOutput"], true);

        let err: std::result::Result<HookJSONOutput, SdkError> = Err(SdkError::InvalidState {
            message: "boom".into(),
        });
        let parsed: serde_json::Value =
            serde_json::from_str(&build_hook_response_json("req-ko", &err)).expect("valid JSON");
        assert_eq!(parsed["response"]["subtype"], "error");
        assert_eq!(parsed["response"]["request_id"], "req-ko");
        assert!(
            parsed["response"]["error"]
                .as_str()
                .is_some_and(|e| e.contains("boom")),
            "the error text must survive: {parsed}"
        );
        assert!(
            parsed["response"].get("response").is_none(),
            "an error carries no payload"
        );
    }

    #[test]
    fn build_hook_response_json_and_the_async_path_agree_on_the_wire_shape() {
        // `send_hook_response` and `build_hook_response_json` are two copies of
        // the same format; this test is what fails if they ever drift.
        let output = Ok(HookJSONOutput::Async(crate::types::AsyncHookJSONOutput {
            async_: true,
            async_timeout: Some(1_500),
        }));
        let parsed: serde_json::Value =
            serde_json::from_str(&build_hook_response_json("req-async", &output))
                .expect("valid JSON");
        assert_eq!(parsed["response"]["response"]["async"], true);
        assert_eq!(parsed["response"]["response"]["asyncTimeout"], 1_500);
    }

    #[tokio::test]
    async fn send_hook_response_should_not_need_the_transport_lock_at_all() {
        let (transport, mut handle) = ScriptedBuilder::new().with_stdin().build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();

        let transport_mutex = client.transport.clone();
        let _held = transport_mutex.lock().await;

        let output = Ok(HookJSONOutput::Sync(SyncHookJSONOutput::default()));
        tokio::time::timeout(
            std::time::Duration::from_millis(150),
            client.send_hook_response("req-1", &output),
        )
        .await
        .expect("a hook answer must not wait for the transport mutex")
        .expect("the write succeeds");

        let line = handle
            .stdin_rx
            .as_mut()
            .expect("stdin")
            .recv()
            .await
            .expect("one line");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(parsed["response"]["request_id"], "req-1");
    }

    #[tokio::test]
    async fn disconnect_forgets_the_cached_stdin_sender_and_connect_re_arms_it() {
        // Caching the sender at connect() without clearing it here would be a
        // worse defect than the mutex it removes: hook answers would keep being
        // queued on a channel the CLI no longer reads.
        let (transport, mut handle) = ScriptedBuilder::new().with_stdin().build();
        let mut client = InteractiveClient::from_transport(transport);
        client.connect().await.unwrap();
        client.disconnect().await.unwrap();

        let output = Ok(HookJSONOutput::Sync(SyncHookJSONOutput::default()));
        client
            .send_hook_response("req-after-disconnect", &output)
            .await
            .expect("the fallback answers through the transport");
        assert!(
            handle.stdin_rx.as_mut().expect("stdin").try_recv().is_err(),
            "nothing may be written to the stdin of a disconnected client"
        );

        // A reconnect re-arms the cached sender.
        client.connect().await.unwrap();
        client
            .send_hook_response("req-reconnected", &output)
            .await
            .unwrap();
        let line = handle
            .stdin_rx
            .as_mut()
            .expect("stdin")
            .recv()
            .await
            .expect("one line");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(parsed["response"]["request_id"], "req-reconnected");
    }
}
