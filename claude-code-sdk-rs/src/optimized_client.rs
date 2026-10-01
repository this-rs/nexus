//! Optimized client implementation with performance improvements

use crate::token_tracker::{BudgetLimit, BudgetManager, BudgetWarningCallback, TokenUsageTracker};
use crate::{
    errors::{Result, SdkError},
    transport::{InputMessage, SubprocessTransport, Transport},
    types::{ClaudeCodeOptions, ControlRequest, Message},
};
use futures::stream::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{RwLock, Semaphore, mpsc};
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info, warn};

/// What `Transport::receive_messages` / `subscribe_messages` hand back.
type MessageStream = Pin<Box<dyn Stream<Item = Result<Message>> + Send + 'static>>;

/// Client mode for different usage patterns
#[derive(Debug, Clone, Copy)]
pub enum ClientMode {
    /// One-shot query mode (stateless)
    OneShot,
    /// Interactive mode (stateful conversations)
    Interactive,
    /// Batch processing mode
    Batch {
        /// Maximum number of concurrent requests
        max_concurrent: usize,
    },
}

/// Connection pool for reusing subprocess transports
struct ConnectionPool {
    /// Available idle connections
    idle_connections: Arc<RwLock<VecDeque<Box<dyn Transport + Send>>>>,
    /// Maximum number of connections
    max_connections: usize,
    /// Semaphore for limiting concurrent connections
    connection_semaphore: Arc<Semaphore>,
    /// Base options for creating new connections
    base_options: ClaudeCodeOptions,
}

impl ConnectionPool {
    fn new(base_options: ClaudeCodeOptions, max_connections: usize) -> Self {
        Self {
            idle_connections: Arc::new(RwLock::new(VecDeque::new())),
            max_connections,
            connection_semaphore: Arc::new(Semaphore::new(max_connections)),
            base_options,
        }
    }

    async fn acquire(&self) -> Result<Box<dyn Transport + Send>> {
        // Try to get an idle connection first
        {
            let mut idle = self.idle_connections.write().await;
            if let Some(transport) = idle.pop_front() {
                // Verify connection is still valid
                if transport.is_connected() {
                    debug!("Reusing existing connection from pool");
                    return Ok(transport);
                }
            }
        }

        // Create new connection if under limit
        let _permit =
            self.connection_semaphore
                .acquire()
                .await
                .map_err(|_| SdkError::InvalidState {
                    message: "Failed to acquire connection permit".into(),
                })?;

        let mut transport: Box<dyn Transport + Send> =
            Box::new(SubprocessTransport::new(self.base_options.clone())?);
        transport.connect().await?;
        debug!("Created new connection");
        Ok(transport)
    }

    async fn release(&self, transport: Box<dyn Transport + Send>) {
        if transport.is_connected()
            && self.idle_connections.read().await.len() < self.max_connections
        {
            let mut idle = self.idle_connections.write().await;
            idle.push_back(transport);
            debug!("Returned connection to pool");
        } else {
            // Connection is invalid or pool is full, let it drop
            debug!("Dropping connection");
        }
    }
}

/// Optimized client with improved performance characteristics
pub struct OptimizedClient {
    /// Client mode
    mode: ClientMode,
    /// Connection pool
    pool: Arc<ConnectionPool>,
    /// Message receiver for interactive mode
    message_rx: Arc<RwLock<Option<mpsc::Receiver<Message>>>>,
    /// Current transport for interactive mode
    current_transport: Arc<RwLock<Option<Box<dyn Transport + Send>>>>,
    /// Budget manager for token/cost tracking
    budget_manager: BudgetManager,
}

impl OptimizedClient {
    /// Create a new optimized client
    pub fn new(options: ClaudeCodeOptions, mode: ClientMode) -> Result<Self> {
        unsafe {
            std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "sdk-rust");
        }

        let max_connections = match mode {
            ClientMode::Batch { max_concurrent } => max_concurrent,
            _ => 1,
        };

        let pool = Arc::new(ConnectionPool::new(options, max_connections));

        Ok(Self {
            mode,
            pool,
            message_rx: Arc::new(RwLock::new(None)),
            current_transport: Arc::new(RwLock::new(None)),
            budget_manager: BudgetManager::new(),
        })
    }

    /// Execute a one-shot query with automatic retry
    pub async fn query(&self, prompt: String) -> Result<Vec<Message>> {
        self.query_with_retry(prompt, 3, Duration::from_millis(100))
            .await
    }

    /// Execute a query with custom retry configuration
    pub async fn query_with_retry(
        &self,
        prompt: String,
        max_retries: u32,
        initial_delay: Duration,
    ) -> Result<Vec<Message>> {
        let mut retries = 0;
        let mut delay = initial_delay;

        loop {
            match self.execute_query(&prompt).await {
                Ok(messages) => return Ok(messages),
                Err(e) if retries < max_retries => {
                    warn!("Query failed, retrying in {:?}: {}", delay, e);
                    tokio::time::sleep(delay).await;
                    retries += 1;
                    delay *= 2; // Exponential backoff
                },
                Err(e) => return Err(e),
            }
        }
    }

    /// Internal query execution
    async fn execute_query(&self, prompt: &str) -> Result<Vec<Message>> {
        let mut transport = self.pool.acquire().await?;
        let outcome = self.run_turn(&mut *transport, prompt).await;

        // Return the transport to the pool on *every* path, not just on success:
        // `release` checks `is_connected()` and drops a transport whose CLI died,
        // so a failed turn no longer leaks the connection it borrowed.
        self.pool.release(transport).await;

        outcome
    }

    /// Send one prompt on `transport` and collect the reply for that turn.
    ///
    /// Subscribes **before** sending. `receive_messages` hands back a tokio
    /// broadcast subscription and a broadcast replays nothing: a CLI that answers
    /// between the write to stdin and the subscription would be answering into the
    /// void, and the turn would then only end on the 120 s timeout below.
    async fn run_turn<T: Transport + Send + ?Sized>(
        &self,
        transport: &mut T,
        prompt: &str,
    ) -> Result<Vec<Message>> {
        let stream = transport.receive_messages();

        // Send message
        let message = InputMessage::user(prompt.to_string(), "default".to_string());
        transport.send_message(message).await?;

        // Collect response with timeout
        let timeout_duration = Duration::from_secs(120);
        timeout(timeout_duration, self.collect_messages(stream))
            .await
            .map_err(|_| SdkError::Timeout { seconds: 120 })?
    }

    /// Collect messages until Result message
    async fn collect_messages(&self, mut stream: MessageStream) -> Result<Vec<Message>> {
        let mut messages = Vec::new();

        while let Some(result) = stream.next().await {
            match result {
                Ok(msg) => {
                    debug!("Received: {:?}", msg);
                    let is_result = matches!(msg, Message::Result { .. });

                    // Update budget/usage on result messages
                    if let Message::Result {
                        usage,
                        total_cost_usd,
                        ..
                    } = &msg
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
                        self.budget_manager
                            .update_usage(input_tokens, output_tokens, cost)
                            .await;
                    }
                    messages.push(msg);
                    if is_result {
                        break;
                    }
                },
                Err(e) => return Err(e),
            }
        }

        Ok(messages)
    }

    /// Get token/cost usage statistics
    pub async fn get_usage_stats(&self) -> TokenUsageTracker {
        self.budget_manager.get_usage().await
    }

    /// Set budget limit with optional warning callback
    ///
    /// Example:
    /// ```rust,no_run
    /// use nexus_claude::{OptimizedClient, ClaudeCodeOptions, ClientMode};
    /// use nexus_claude::token_tracker::{BudgetLimit, BudgetWarningCallback};
    /// use std::sync::Arc;
    /// # async fn demo() -> nexus_claude::Result<()> {
    /// let client = OptimizedClient::new(ClaudeCodeOptions::default(), ClientMode::OneShot)?;
    /// let cb: BudgetWarningCallback = Arc::new(|msg: &str| println!("Warn: {}", msg));
    /// client.set_budget_limit(BudgetLimit::with_cost(1.0), Some(cb)).await;
    /// # Ok(()) }
    /// ```
    pub async fn set_budget_limit(
        &self,
        limit: BudgetLimit,
        on_warning: Option<BudgetWarningCallback>,
    ) {
        self.budget_manager.set_limit(limit).await;
        if let Some(cb) = on_warning {
            self.budget_manager.set_warning_callback(cb).await;
        }
    }

    /// Clear budget limit and reset warning state
    pub async fn clear_budget_limit(&self) {
        self.budget_manager.clear_limit().await;
    }

    /// Reset usage statistics to zero
    pub async fn reset_usage_stats(&self) {
        self.budget_manager.reset_usage().await;
    }

    /// Check whether budget is exceeded
    pub async fn is_budget_exceeded(&self) -> bool {
        self.budget_manager.is_exceeded().await
    }

    /// Start an interactive session
    pub async fn start_interactive_session(&self) -> Result<()> {
        if !matches!(self.mode, ClientMode::Interactive) {
            return Err(SdkError::InvalidState {
                message: "Client not in interactive mode".into(),
            });
        }

        // Acquire a transport for the session
        let transport = self.pool.acquire().await?;

        // Create message channel
        let (tx, rx) = mpsc::channel::<Message>(100);

        // Store transport and receiver
        *self.current_transport.write().await = Some(transport);
        *self.message_rx.write().await = Some(rx);

        // Start background message processor
        self.start_message_processor(tx).await;

        info!("Interactive session started");
        Ok(())
    }

    /// Start background task to process messages
    ///
    /// Subscribes **once**, before spawning, through `Transport::subscribe_messages`:
    /// it borrows the transport immutably and yields a `'static` stream. The loop
    /// used to call `receive_messages()` on every iteration instead, which was
    /// wrong twice over. It held the `current_transport` *write* lock across
    /// `stream.next().await`, so `send_interactive` could never acquire the lock to
    /// send the prompt that would have produced that very message — the session
    /// deadlocked on its first turn. And re-subscribing to a broadcast that never
    /// replays dropped everything the CLI printed between two iterations.
    async fn start_message_processor(&self, tx: mpsc::Sender<Message>) {
        let stream = {
            let transport_guard = self.current_transport.read().await;
            transport_guard
                .as_ref()
                .and_then(|transport| transport.subscribe_messages())
        };

        let Some(mut stream) = stream else {
            // No broadcast to listen to: drop `tx` so `receive_interactive`
            // returns instead of waiting for a message that cannot come.
            warn!("Transport exposes no message broadcast; no messages will be forwarded");
            return;
        };

        tokio::spawn(async move {
            while let Some(result) = stream.next().await {
                match result {
                    Ok(msg) => {
                        if tx.send(msg).await.is_err() {
                            error!("Failed to send message to channel");
                            break;
                        }
                    },
                    Err(e) => {
                        error!("Error receiving message: {}", e);
                        break;
                    },
                }
            }
        });
    }

    /// Send a message in interactive mode
    pub async fn send_interactive(&self, prompt: String) -> Result<()> {
        let transport_guard = self.current_transport.read().await;
        if let Some(_transport) = transport_guard.as_ref() {
            // Need to handle transport mutability properly
            drop(transport_guard);

            let mut transport_guard = self.current_transport.write().await;
            if let Some(transport) = transport_guard.as_mut() {
                let message = InputMessage::user(prompt, "default".to_string());
                transport.send_message(message).await?;
            } else {
                return Err(SdkError::InvalidState {
                    message: "Transport lost during operation".into(),
                });
            }
            Ok(())
        } else {
            Err(SdkError::InvalidState {
                message: "No active interactive session".into(),
            })
        }
    }

    /// Receive messages in interactive mode
    pub async fn receive_interactive(&self) -> Result<Vec<Message>> {
        let mut rx_guard = self.message_rx.write().await;
        if let Some(rx) = rx_guard.as_mut() {
            let mut messages = Vec::new();

            // Collect messages until Result
            while let Some(msg) = rx.recv().await {
                let is_result = matches!(msg, Message::Result { .. });
                messages.push(msg);
                if is_result {
                    break;
                }
            }

            Ok(messages)
        } else {
            Err(SdkError::InvalidState {
                message: "No active interactive session".into(),
            })
        }
    }

    /// Process a batch of queries concurrently
    pub async fn process_batch(&self, prompts: Vec<String>) -> Result<Vec<Result<Vec<Message>>>> {
        let max_concurrent = match self.mode {
            ClientMode::Batch { max_concurrent } => max_concurrent,
            _ => {
                return Err(SdkError::InvalidState {
                    message: "Client not in batch mode".into(),
                });
            },
        };

        let semaphore = Arc::new(Semaphore::new(max_concurrent));
        let mut handles = Vec::new();

        for prompt in prompts {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let client = self.clone(); // Assume client is cloneable

            let handle = tokio::spawn(async move {
                let result = client.query(prompt).await;
                drop(permit);
                result
            });

            handles.push(handle);
        }

        // Collect results
        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => results.push(Err(SdkError::TransportError(format!("Task failed: {e}")))),
            }
        }

        Ok(results)
    }

    /// Send interrupt signal
    pub async fn interrupt(&self) -> Result<()> {
        let transport_guard = self.current_transport.read().await;
        if let Some(_transport) = transport_guard.as_ref() {
            drop(transport_guard);

            let mut transport_guard = self.current_transport.write().await;
            if let Some(transport) = transport_guard.as_mut() {
                let request = ControlRequest::Interrupt {
                    request_id: uuid::Uuid::new_v4().to_string(),
                };
                transport.send_control_request(request).await?;
            } else {
                return Err(SdkError::InvalidState {
                    message: "Transport lost during operation".into(),
                });
            }
            info!("Interrupt sent");
            Ok(())
        } else {
            Err(SdkError::InvalidState {
                message: "No active session".into(),
            })
        }
    }

    /// End interactive session
    pub async fn end_interactive_session(&self) -> Result<()> {
        // Clear current transport
        if let Some(transport) = self.current_transport.write().await.take() {
            self.pool.release(transport).await;
        }

        // Clear message receiver
        *self.message_rx.write().await = None;

        info!("Interactive session ended");
        Ok(())
    }
}

// Implement Clone if needed (this is a simplified version)
impl Clone for OptimizedClient {
    fn clone(&self) -> Self {
        Self {
            mode: self.mode,
            pool: self.pool.clone(),
            message_rx: Arc::new(RwLock::new(None)),
            current_transport: Arc::new(RwLock::new(None)),
            budget_manager: self.budget_manager.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, ContentBlock, ControlResponse, TextContent};
    use async_trait::async_trait;
    use serde_json::{Value as JsonValue, json};
    use std::future::Future;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::Poll;
    use tokio::sync::broadcast;

    // =====================================================================
    // A transport test double
    //
    // `ConnectionPool` builds `SubprocessTransport`s itself, so the only seam
    // into it is its own idle queue: these tests push a double in there and let
    // `acquire()` hand it straight back. Nothing here spawns a process, and the
    // `cli_path` every client is built with does not exist, so any code path
    // that *does* try to spawn fails loudly instead of reaching a real CLI.
    // =====================================================================

    /// One scripted stream item. `Clone` so the same script can answer several
    /// subscriptions.
    #[derive(Clone)]
    enum Item {
        Msg(Message),
        Err(&'static str),
    }

    impl Item {
        fn into_result(self) -> Result<Message> {
            match self {
                Item::Msg(message) => Ok(message),
                Item::Err(message) => Err(SdkError::InvalidState {
                    message: message.to_string(),
                }),
            }
        }
    }

    /// How the double answers a subscription.
    #[derive(Clone, Copy)]
    enum Feed {
        /// Replay the script, then end.
        Script,
        /// Never yield, never end: the CLI that went quiet.
        Silent,
        /// Yield whatever the test emits through the handle.
        Live,
        /// Like `Live`, except `subscribe_messages` answers `None` — a transport
        /// with no broadcast behind it.
        NoBroadcast,
    }

    #[derive(Default)]
    struct Log {
        sent: Vec<InputMessage>,
        controls: Vec<ControlRequest>,
        receive_calls: usize,
        subscribe_calls: usize,
    }

    struct Double {
        connected: Arc<AtomicBool>,
        feed: Feed,
        script: Vec<Item>,
        live: broadcast::Sender<Message>,
        log: Arc<Mutex<Log>>,
        send_fails: bool,
        control_fails: bool,
        panic_on_send: bool,
    }

    /// Observation side of a [`Double`], usable after the double has been handed
    /// to the pool.
    #[derive(Clone)]
    struct Handle {
        live: broadcast::Sender<Message>,
        log: Arc<Mutex<Log>>,
    }

    impl Handle {
        fn sent_texts(&self) -> Vec<String> {
            self.log
                .lock()
                .expect("log")
                .sent
                .iter()
                .map(|message| {
                    message.message["content"]
                        .as_str()
                        .unwrap_or("<not a string>")
                        .to_string()
                })
                .collect()
        }

        fn sessions(&self) -> Vec<String> {
            self.log
                .lock()
                .expect("log")
                .sent
                .iter()
                .map(|message| message.session_id.clone())
                .collect()
        }

        fn control_ids(&self) -> Vec<String> {
            self.log
                .lock()
                .expect("log")
                .controls
                .iter()
                .map(|request| match request {
                    ControlRequest::Interrupt { request_id } => request_id.clone(),
                })
                .collect()
        }

        fn receive_calls(&self) -> usize {
            self.log.lock().expect("log").receive_calls
        }

        fn subscribe_calls(&self) -> usize {
            self.log.lock().expect("log").subscribe_calls
        }

        /// How many live subscriptions the broadcast still has — one while the
        /// message processor is running, zero once it has given up.
        fn subscribers(&self) -> usize {
            self.live.receiver_count()
        }

        /// Push a message as if the CLI had printed it.
        fn emit(&self, message: Message) {
            self.live
                .send(message)
                .expect("the message processor must be subscribed");
        }
    }

    impl Double {
        fn new(feed: Feed) -> Self {
            let (live, _idle) = broadcast::channel(64);
            Self {
                connected: Arc::new(AtomicBool::new(true)),
                feed,
                script: Vec::new(),
                live,
                log: Arc::new(Mutex::new(Log::default())),
                send_fails: false,
                control_fails: false,
                panic_on_send: false,
            }
        }

        fn scripted(script: Vec<Item>) -> Self {
            let mut double = Self::new(Feed::Script);
            double.script = script;
            double
        }

        /// A transport whose CLI is gone: `is_connected()` answers `false`.
        fn dead(self) -> Self {
            self.connected.store(false, Ordering::SeqCst);
            self
        }

        fn failing_send(mut self) -> Self {
            self.send_fails = true;
            self
        }

        fn failing_control(mut self) -> Self {
            self.control_fails = true;
            self
        }

        fn panicking_send(mut self) -> Self {
            self.panic_on_send = true;
            self
        }

        fn handle(&self) -> Handle {
            Handle {
                live: self.live.clone(),
                log: self.log.clone(),
            }
        }

        fn boxed(self) -> Box<dyn Transport + Send> {
            Box::new(self)
        }

        fn stream(&self) -> MessageStream {
            match self.feed {
                Feed::Script => Box::pin(futures::stream::iter(
                    self.script.clone().into_iter().map(Item::into_result),
                )),
                Feed::Silent => Box::pin(futures::stream::pending()),
                Feed::Live | Feed::NoBroadcast => {
                    let rx = self.live.subscribe();
                    Box::pin(
                        tokio_stream::wrappers::BroadcastStream::new(rx)
                            .filter_map(|item| async move { item.ok().map(Ok) }),
                    )
                },
            }
        }
    }

    #[async_trait]
    impl Transport for Double {
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        async fn connect(&mut self) -> Result<()> {
            self.connected.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn send_message(&mut self, message: InputMessage) -> Result<()> {
            assert!(
                !self.panic_on_send,
                "this double is scripted to panic inside send_message"
            );
            self.log.lock().expect("log").sent.push(message);
            if self.send_fails {
                return Err(SdkError::ConnectionError("stdin is closed".into()));
            }
            Ok(())
        }

        fn receive_messages(&mut self) -> MessageStream {
            self.log.lock().expect("log").receive_calls += 1;
            self.stream()
        }

        fn subscribe_messages(&self) -> Option<MessageStream> {
            self.log.lock().expect("log").subscribe_calls += 1;
            match self.feed {
                Feed::NoBroadcast => None,
                _ => Some(self.stream()),
            }
        }

        async fn send_control_request(&mut self, request: ControlRequest) -> Result<()> {
            self.log.lock().expect("log").controls.push(request);
            if self.control_fails {
                return Err(SdkError::ConnectionError(
                    "control channel is closed".into(),
                ));
            }
            Ok(())
        }

        async fn receive_control_response(&mut self) -> Result<Option<ControlResponse>> {
            Ok(None)
        }

        async fn send_sdk_control_request(&mut self, _request: JsonValue) -> Result<()> {
            Ok(())
        }

        async fn send_sdk_control_response(&mut self, _response: JsonValue) -> Result<()> {
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::SeqCst)
        }

        async fn disconnect(&mut self) -> Result<()> {
            self.connected.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    // ----- fixtures -------------------------------------------------------

    fn assistant(text: &str) -> Message {
        Message::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent {
                    text: text.to_string(),
                })],
            },
            parent_tool_use_id: None,
        }
    }

    fn result_message(usage: Option<JsonValue>, total_cost_usd: Option<f64>) -> Message {
        Message::Result {
            subtype: "success".to_string(),
            duration_ms: 12,
            duration_api_ms: 7,
            is_error: false,
            num_turns: 1,
            session_id: "fake-session".to_string(),
            total_cost_usd,
            usage,
            result: Some("done".to_string()),
            structured_output: None,
        }
    }

    /// One assistant turn billed 11 input / 22 output tokens for $0.50.
    fn one_turn(text: &str) -> Vec<Item> {
        vec![
            Item::Msg(assistant(text)),
            Item::Msg(result_message(
                Some(json!({"input_tokens": 11, "output_tokens": 22})),
                Some(0.5),
            )),
        ]
    }

    /// A path no process can be spawned from, so every code path that tries to
    /// create a connection fails instead of reaching a real `claude`.
    fn unspawnable_cli() -> std::path::PathBuf {
        std::env::temp_dir().join("nexus-optimized-client-tests-no-such-cli")
    }

    fn unspawnable_options() -> ClaudeCodeOptions {
        ClaudeCodeOptions::builder()
            .cli_path(unspawnable_cli())
            .build()
    }

    fn unspawnable_client(mode: ClientMode) -> OptimizedClient {
        OptimizedClient::new(unspawnable_options(), mode).expect("the client is always built")
    }

    /// Put `transport` in the pool's idle queue, which is the only seam into
    /// `ConnectionPool` from outside.
    async fn prime(client: &OptimizedClient, transport: Box<dyn Transport + Send>) {
        client
            .pool
            .idle_connections
            .write()
            .await
            .push_back(transport);
    }

    async fn idle_count(client: &OptimizedClient) -> usize {
        client.pool.idle_connections.read().await.len()
    }

    /// `Box<dyn Transport>` is not `Debug`, so `expect_err` cannot be used on an
    /// `acquire` outcome.
    fn acquire_error(outcome: Result<Box<dyn Transport + Send>>, why: &str) -> SdkError {
        match outcome {
            Ok(_) => panic!("acquire was expected to fail: {why}"),
            Err(error) => error,
        }
    }

    /// Poll `condition` every 10 ms, up to a second, so a background task gets a
    /// chance to run without the test guessing at a sleep duration.
    async fn poll_until(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        condition()
    }

    fn invalid_state_message(error: SdkError) -> String {
        match error {
            SdkError::InvalidState { message } => message,
            other => panic!("expected an InvalidState error, got {other:?}"),
        }
    }

    // =====================================================================
    // ClientMode
    // =====================================================================

    #[test]
    fn test_client_mode_creation() {
        let options = ClaudeCodeOptions::builder().build();

        assert!(OptimizedClient::new(options.clone(), ClientMode::OneShot).is_ok());
        assert!(OptimizedClient::new(options.clone(), ClientMode::Interactive).is_ok());
        assert!(OptimizedClient::new(options, ClientMode::Batch { max_concurrent: 5 }).is_ok());
    }

    #[test]
    fn test_connection_pool_creation() {
        let options = ClaudeCodeOptions::builder().build();
        let pool = ConnectionPool::new(options, 10);

        assert_eq!(pool.max_connections, 10);
        assert_eq!(pool.connection_semaphore.available_permits(), 10);
    }

    /// The pool size is the mode's concurrency, and every mode but `Batch`
    /// collapses to a single connection.
    #[test]
    fn the_pool_is_sized_from_the_mode() {
        for (mode, expected) in [
            (ClientMode::OneShot, 1_usize),
            (ClientMode::Interactive, 1),
            (ClientMode::Batch { max_concurrent: 7 }, 7),
        ] {
            let client = unspawnable_client(mode);
            assert_eq!(
                client.pool.max_connections, expected,
                "{:?} must size the pool to {expected}",
                client.mode
            );
        }
    }

    /// `new` tells the CLI which SDK is driving it, through the environment.
    #[test]
    #[serial_test::serial]
    fn new_announces_the_rust_sdk_as_the_entrypoint() {
        unsafe {
            std::env::remove_var("CLAUDE_CODE_ENTRYPOINT");
        }
        let _client = unspawnable_client(ClientMode::OneShot);
        assert_eq!(
            std::env::var("CLAUDE_CODE_ENTRYPOINT").ok().as_deref(),
            Some("sdk-rust")
        );
    }

    // =====================================================================
    // ConnectionPool
    // =====================================================================

    /// A released connection that is still alive is the one the next `acquire`
    /// gets back — proved by writing to it and reading the double's log.
    #[tokio::test]
    async fn the_pool_hands_back_the_live_connection_it_was_given() {
        let pool = ConnectionPool::new(unspawnable_options(), 2);
        let double = Double::new(Feed::Script);
        let handle = double.handle();

        pool.release(double.boxed()).await;
        assert_eq!(pool.idle_connections.read().await.len(), 1);

        let mut transport = pool
            .acquire()
            .await
            .expect("the idle connection is reused, so nothing is spawned");
        assert!(
            pool.idle_connections.read().await.is_empty(),
            "the connection left the idle queue"
        );

        transport
            .send_message(InputMessage::user("ping".into(), "s".into()))
            .await
            .expect("send");
        assert_eq!(
            handle.sent_texts(),
            vec!["ping".to_string()],
            "the very transport that was pooled came back"
        );
    }

    /// `release` checks liveness, so a transport whose CLI died is dropped
    /// instead of being queued for the next caller. This is the half of the
    /// contract that `SubprocessTransport::is_connected` has to tell the truth
    /// about.
    #[tokio::test]
    async fn release_drops_a_connection_whose_cli_died() {
        let pool = ConnectionPool::new(unspawnable_options(), 2);
        pool.release(Double::new(Feed::Script).dead().boxed()).await;
        assert!(
            pool.idle_connections.read().await.is_empty(),
            "a dead connection must never be pooled"
        );
    }

    /// `max_connections` caps the idle queue; the surplus is dropped, and it is
    /// the newcomer that goes, not the connection already queued.
    #[tokio::test]
    async fn release_drops_the_surplus_when_the_pool_is_full() {
        let pool = ConnectionPool::new(unspawnable_options(), 1);

        let first = Double::new(Feed::Script);
        let kept = first.handle();
        pool.release(first.boxed()).await;

        let second = Double::new(Feed::Script);
        let dropped = second.handle();
        pool.release(second.boxed()).await;

        assert_eq!(
            pool.idle_connections.read().await.len(),
            1,
            "max_connections = 1 caps the idle queue"
        );

        let mut transport = pool.acquire().await.expect("the queued connection");
        transport
            .send_message(InputMessage::user("ping".into(), "s".into()))
            .await
            .expect("send");
        assert_eq!(kept.sent_texts(), vec!["ping".to_string()]);
        assert!(
            dropped.sent_texts().is_empty(),
            "the surplus connection was dropped, not kept"
        );
    }

    /// An idle connection that died while it was queued is discarded, and
    /// `acquire` falls through to creating a fresh one — which fails here,
    /// because the configured CLI does not exist.
    #[tokio::test]
    async fn acquire_discards_a_dead_idle_connection_before_spawning_a_replacement() {
        let pool = ConnectionPool::new(unspawnable_options(), 1);
        pool.idle_connections
            .write()
            .await
            .push_back(Double::new(Feed::Script).dead().boxed());

        let error = acquire_error(
            pool.acquire().await,
            "there is no CLI to spawn as a replacement",
        );
        assert!(
            matches!(error, SdkError::ProcessError(_)),
            "spawning must fail, got {error:?}"
        );
        assert!(
            pool.idle_connections.read().await.is_empty(),
            "the dead connection was dropped, not put back"
        );
    }

    /// A closed semaphore is reported, not ignored.
    #[tokio::test]
    async fn acquire_turns_a_closed_semaphore_into_an_invalid_state_error() {
        let pool = ConnectionPool::new(unspawnable_options(), 1);
        pool.connection_semaphore.close();

        let error = acquire_error(pool.acquire().await, "no permit can be issued");
        assert_eq!(
            invalid_state_message(error),
            "Failed to acquire connection permit"
        );
    }

    /// The semaphore gates *creation* — with the only permit held, `acquire`
    /// waits. What it does not do is bound anything: the permit is dropped on
    /// the way out, so it never travels with the connection it paid for.
    #[tokio::test(start_paused = true)]
    async fn acquire_waits_for_a_permit_then_immediately_gives_it_back() {
        let pool = ConnectionPool::new(unspawnable_options(), 1);
        let held = pool
            .connection_semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("the first permit");

        assert!(
            tokio::time::timeout(Duration::from_secs(5), pool.acquire())
                .await
                .is_err(),
            "with the single permit held, creating a connection has to wait"
        );

        drop(held);
        let error = acquire_error(
            pool.acquire().await,
            "the permit is free again, so it gets as far as spawning",
        );
        assert!(matches!(error, SdkError::ProcessError(_)), "got {error:?}");
        assert_eq!(
            pool.connection_semaphore.available_permits(),
            1,
            "`_permit` dies with the function body, so N callers can hold N \
             connections against max_connections = 1: the pool bounds nothing"
        );
    }

    // =====================================================================
    // One-shot queries
    // =====================================================================

    /// The happy path, end to end: collection stops at the `Result` message, the
    /// prompt reaches the transport once, the turn is billed, and the connection
    /// goes back to the pool.
    #[tokio::test]
    async fn a_query_stops_at_the_result_message_and_bills_the_turn() {
        let client = unspawnable_client(ClientMode::OneShot);
        let mut script = one_turn("hello");
        script.push(Item::Msg(assistant("printed after the result")));
        let double = Double::scripted(script);
        let handle = double.handle();
        prime(&client, double.boxed()).await;

        let messages = client.query("ping".to_string()).await.expect("query");

        assert_eq!(
            messages.len(),
            2,
            "everything after the Result message is left on the stream: {messages:?}"
        );
        assert!(matches!(messages[0], Message::Assistant { .. }));
        assert!(matches!(messages[1], Message::Result { .. }));
        assert_eq!(handle.sent_texts(), vec!["ping".to_string()]);
        assert_eq!(
            handle.sessions(),
            vec!["default".to_string()],
            "a one-shot query always uses the `default` session id"
        );
        assert_eq!(
            handle.receive_calls(),
            1,
            "one subscription per turn, taken before the prompt is written"
        );
        assert_eq!(
            idle_count(&client).await,
            1,
            "the connection is pooled again"
        );

        let usage = client.get_usage_stats().await;
        assert_eq!(usage.total_input_tokens, 11);
        assert_eq!(usage.total_output_tokens, 22);
        assert!((usage.total_cost_usd - 0.5).abs() < 1e-9);
        assert_eq!(usage.session_count, 1);
    }

    /// A `Result` message with no `usage` and no cost still closes the turn, and
    /// is billed as free rather than refused.
    #[tokio::test]
    async fn a_result_without_usage_is_billed_as_free() {
        let client = unspawnable_client(ClientMode::OneShot);
        prime(
            &client,
            Double::scripted(vec![Item::Msg(result_message(None, None))]).boxed(),
        )
        .await;

        let messages = client.query("ping".to_string()).await.expect("query");

        assert_eq!(messages.len(), 1);
        let usage = client.get_usage_stats().await;
        assert_eq!(usage.total_tokens(), 0);
        assert!((usage.total_cost_usd - 0.0).abs() < 1e-9);
        assert_eq!(usage.session_count, 1, "the turn still counts as a session");
    }

    /// `usage` present but without the token fields: the missing counters read
    /// as zero instead of failing the turn.
    #[tokio::test]
    async fn unknown_usage_fields_read_as_zero_tokens() {
        let client = unspawnable_client(ClientMode::OneShot);
        prime(
            &client,
            Double::scripted(vec![Item::Msg(result_message(
                Some(json!({"cache_read_input_tokens": 9})),
                Some(0.25),
            ))])
            .boxed(),
        )
        .await;

        client.query("ping".to_string()).await.expect("query");

        let usage = client.get_usage_stats().await;
        assert_eq!(usage.total_tokens(), 0);
        assert!((usage.total_cost_usd - 0.25).abs() < 1e-9);
    }

    /// A stream error fails the turn — and the connection still goes back to the
    /// pool. Regression: `execute_query` used to reach `pool.release` only on
    /// the success path, so every failed turn leaked its connection.
    #[tokio::test]
    async fn a_stream_error_fails_the_turn_without_leaking_the_connection() {
        let client = unspawnable_client(ClientMode::OneShot);
        prime(
            &client,
            Double::scripted(vec![
                Item::Msg(assistant("half an answer")),
                Item::Err("stdout reader died"),
            ])
            .boxed(),
        )
        .await;

        let error = client
            .query_with_retry("ping".to_string(), 0, Duration::from_millis(1))
            .await
            .expect_err("the stream error must surface");

        assert_eq!(invalid_state_message(error), "stdout reader died");
        assert_eq!(
            idle_count(&client).await,
            1,
            "a failed turn must still return its connection to the pool"
        );
    }

    /// Same contract when the failure happens on the way out instead.
    #[tokio::test]
    async fn a_send_failure_fails_the_turn_without_leaking_the_connection() {
        let client = unspawnable_client(ClientMode::OneShot);
        let double = Double::scripted(one_turn("never reached")).failing_send();
        let handle = double.handle();
        prime(&client, double.boxed()).await;

        let error = client
            .query_with_retry("ping".to_string(), 0, Duration::from_millis(1))
            .await
            .expect_err("the send failure must surface");

        assert!(
            matches!(error, SdkError::ConnectionError(ref message) if message == "stdin is closed"),
            "got {error:?}"
        );
        assert_eq!(
            handle.receive_calls(),
            1,
            "the subscription is taken before the prompt is written, so it exists even here"
        );
        assert_eq!(idle_count(&client).await, 1);
    }

    /// A CLI that goes quiet ends the turn on the hard-coded two-minute timeout.
    #[tokio::test(start_paused = true)]
    async fn a_silent_cli_ends_the_turn_on_the_two_minute_timeout() {
        let client = unspawnable_client(ClientMode::OneShot);
        prime(&client, Double::new(Feed::Silent).boxed()).await;

        let started = tokio::time::Instant::now();
        let error = client
            .query_with_retry("ping".to_string(), 0, Duration::from_millis(1))
            .await
            .expect_err("nothing will ever arrive");

        assert!(
            matches!(error, SdkError::Timeout { seconds: 120 }),
            "got {error:?}"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(120));
    }

    /// `query_with_retry` retries `max_retries` times *on top of* the first
    /// attempt, doubling the delay each time.
    #[tokio::test(start_paused = true)]
    async fn query_with_retry_doubles_the_delay_and_then_gives_up() {
        let client = unspawnable_client(ClientMode::OneShot);

        let started = tokio::time::Instant::now();
        let error = client
            .query_with_retry("ping".to_string(), 3, Duration::from_millis(100))
            .await
            .expect_err("there is no CLI to spawn");

        assert!(matches!(error, SdkError::ProcessError(_)), "got {error:?}");
        assert_eq!(
            started.elapsed(),
            Duration::from_millis(700),
            "four attempts, three waits: 100 + 200 + 400 ms"
        );
    }

    /// A retry that succeeds returns the successful attempt's messages.
    #[tokio::test(start_paused = true)]
    async fn a_retry_that_succeeds_returns_the_second_attempt() {
        let client = unspawnable_client(ClientMode::OneShot);
        // First attempt: nothing in the pool, so it tries to spawn and fails.
        // The retry finds the connection this task puts there in the meantime.
        let double = Double::scripted(one_turn("second time lucky"));
        let pool = client.pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            pool.idle_connections
                .write()
                .await
                .push_back(double.boxed());
        });

        let messages = client
            .query_with_retry("ping".to_string(), 1, Duration::from_millis(50))
            .await
            .expect("the retry finds a usable connection");

        assert_eq!(messages.len(), 2);
        assert_eq!(client.get_usage_stats().await.session_count, 1);
    }

    // =====================================================================
    // Budget
    // =====================================================================

    /// The budget callback fires on the overrun, `clear_budget_limit` makes the
    /// client unlimited again, and `reset_usage_stats` zeroes the counters.
    #[tokio::test]
    async fn the_budget_limit_warns_on_overrun_and_can_be_cleared() {
        let client = unspawnable_client(ClientMode::OneShot);
        let warnings = Arc::new(AtomicUsize::new(0));
        let counter = warnings.clone();
        let callback: BudgetWarningCallback = Arc::new(move |message: &str| {
            assert_eq!(message, "Budget limit exceeded");
            counter.fetch_add(1, Ordering::SeqCst);
        });
        client
            .set_budget_limit(BudgetLimit::with_tokens(10), Some(callback))
            .await;
        assert!(
            !client.is_budget_exceeded().await,
            "nothing has been spent yet"
        );

        prime(&client, Double::scripted(one_turn("ok")).boxed()).await;
        client.query("ping".to_string()).await.expect("query");

        assert!(
            client.is_budget_exceeded().await,
            "33 tokens against a 10 token cap"
        );
        assert_eq!(warnings.load(Ordering::SeqCst), 1);

        client.clear_budget_limit().await;
        assert!(
            !client.is_budget_exceeded().await,
            "with no limit there is nothing to exceed"
        );

        client.reset_usage_stats().await;
        let usage = client.get_usage_stats().await;
        assert_eq!(usage.total_tokens(), 0);
        assert_eq!(usage.session_count, 0);
    }

    // =====================================================================
    // Interactive sessions
    // =====================================================================

    #[tokio::test]
    async fn an_interactive_session_is_refused_in_any_other_mode() {
        let client = unspawnable_client(ClientMode::OneShot);
        let error = client
            .start_interactive_session()
            .await
            .expect_err("one-shot clients have no session");
        assert_eq!(
            invalid_state_message(error),
            "Client not in interactive mode"
        );
    }

    /// A full interactive turn. Regression: the message processor used to hold
    /// the `current_transport` write lock across `stream.next().await`, so
    /// `send_interactive` could never acquire the lock to send the prompt that
    /// would have produced that message — the session deadlocked on its first
    /// turn. Multi-threaded on purpose: on a current-thread runtime the spawned
    /// processor only runs when the test yields, which hides the lock contention.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_interactive_turn_round_trips_through_the_message_processor() {
        let client = unspawnable_client(ClientMode::Interactive);
        let double = Double::new(Feed::Live);
        let handle = double.handle();
        prime(&client, double.boxed()).await;

        client
            .start_interactive_session()
            .await
            .expect("the session starts");
        assert!(client.current_transport.read().await.is_some());
        assert!(client.message_rx.read().await.is_some());
        assert_eq!(
            handle.subscribe_calls(),
            1,
            "the processor subscribes exactly once, before the first turn"
        );

        tokio::time::timeout(
            Duration::from_secs(5),
            client.send_interactive("salut".to_string()),
        )
        .await
        .expect("send_interactive must not wait on the message processor")
        .expect("send");
        assert_eq!(handle.sent_texts(), vec!["salut".to_string()]);

        handle.emit(assistant("bonjour"));
        handle.emit(result_message(None, None));

        let messages = tokio::time::timeout(Duration::from_secs(5), client.receive_interactive())
            .await
            .expect("the processor must forward what the CLI printed")
            .expect("receive");
        assert_eq!(messages.len(), 2, "collection stops at the Result message");
        assert!(matches!(messages[1], Message::Result { .. }));
        assert_eq!(
            handle.subscribe_calls(),
            1,
            "still one subscription: a broadcast replays nothing, so re-subscribing \
             per message would lose everything printed in between"
        );

        client
            .end_interactive_session()
            .await
            .expect("the session ends");
        assert!(client.current_transport.read().await.is_none());
        assert!(client.message_rx.read().await.is_none());
        assert_eq!(
            idle_count(&client).await,
            1,
            "ending the session returns the connection to the pool"
        );
    }

    /// Once nobody is listening any more the processor gives up instead of
    /// forwarding into a closed channel: it breaks out of its loop, which drops
    /// its subscription to the transport.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_processor_gives_up_when_the_session_has_ended() {
        let client = unspawnable_client(ClientMode::Interactive);
        let double = Double::new(Feed::Live);
        let handle = double.handle();
        prime(&client, double.boxed()).await;

        client.start_interactive_session().await.expect("starts");
        assert!(
            poll_until(|| handle.subscribers() == 1).await,
            "the processor subscribes to the transport"
        );

        // Ending the session drops the receiving half of the channel.
        client.end_interactive_session().await.expect("ends");
        handle.emit(assistant("printed after the session ended"));

        assert!(
            poll_until(|| handle.subscribers() == 0).await,
            "the processor must stop once its channel is gone, not spin on it"
        );
    }

    /// A stream error ends the session's message flow: the processor stops and
    /// drops the channel, so `receive_interactive` returns the partial turn
    /// instead of waiting for a `Result` message that will never come.
    #[tokio::test]
    async fn a_stream_error_stops_the_processor_and_closes_the_channel() {
        let client = unspawnable_client(ClientMode::Interactive);
        prime(
            &client,
            Double::scripted(vec![
                Item::Msg(assistant("un")),
                Item::Err("stdout reader died"),
            ])
            .boxed(),
        )
        .await;

        client.start_interactive_session().await.expect("starts");

        let messages = tokio::time::timeout(Duration::from_secs(5), client.receive_interactive())
            .await
            .expect("the closed channel must end the wait")
            .expect("receive");
        assert_eq!(
            messages.len(),
            1,
            "only what arrived before the error: {messages:?}"
        );
        assert!(matches!(messages[0], Message::Assistant { .. }));
    }

    /// A transport with no broadcast behind it cannot feed the processor. The
    /// channel is closed rather than left open, so `receive_interactive` returns
    /// instead of waiting for a message that can never come.
    #[tokio::test]
    async fn a_transport_without_a_broadcast_closes_the_interactive_channel() {
        let client = unspawnable_client(ClientMode::Interactive);
        let double = Double::new(Feed::NoBroadcast);
        let handle = double.handle();
        prime(&client, double.boxed()).await;

        client.start_interactive_session().await.expect("starts");
        assert_eq!(handle.subscribe_calls(), 1);

        let messages = tokio::time::timeout(Duration::from_secs(5), client.receive_interactive())
            .await
            .expect("receive_interactive must not hang")
            .expect("receive");
        assert!(messages.is_empty(), "got {messages:?}");
    }

    /// The processor stopping mid-turn is not an error: `receive_interactive`
    /// hands back whatever did arrive.
    #[tokio::test]
    async fn receive_interactive_returns_a_partial_turn_when_the_channel_closes() {
        let client = unspawnable_client(ClientMode::Interactive);
        let (tx, rx) = mpsc::channel(4);
        *client.message_rx.write().await = Some(rx);

        tx.send(assistant("un")).await.expect("queue a message");
        drop(tx);

        let messages = client.receive_interactive().await.expect("receive");
        assert_eq!(
            messages.len(),
            1,
            "no Result message arrived, the channel simply closed"
        );
    }

    #[tokio::test]
    async fn the_interactive_api_refuses_to_work_without_a_session() {
        let client = unspawnable_client(ClientMode::Interactive);

        let error = client
            .send_interactive("x".to_string())
            .await
            .expect_err("no session");
        assert_eq!(
            invalid_state_message(error),
            "No active interactive session"
        );

        let error = client.receive_interactive().await.expect_err("no session");
        assert_eq!(
            invalid_state_message(error),
            "No active interactive session"
        );

        let error = client.interrupt().await.expect_err("no session");
        assert_eq!(invalid_state_message(error), "No active session");
    }

    #[tokio::test]
    async fn send_interactive_propagates_a_transport_failure() {
        let client = unspawnable_client(ClientMode::Interactive);
        *client.current_transport.write().await =
            Some(Double::new(Feed::Live).failing_send().boxed());

        let error = client
            .send_interactive("x".to_string())
            .await
            .expect_err("the transport refuses");
        assert!(
            matches!(error, SdkError::ConnectionError(ref message) if message == "stdin is closed"),
            "got {error:?}"
        );
    }

    /// Every interrupt carries its own freshly generated request id.
    #[tokio::test]
    async fn interrupt_sends_one_control_request_per_call() {
        let client = unspawnable_client(ClientMode::Interactive);
        let double = Double::new(Feed::Live);
        let handle = double.handle();
        *client.current_transport.write().await = Some(double.boxed());

        client.interrupt().await.expect("first interrupt");
        client.interrupt().await.expect("second interrupt");

        let ids = handle.control_ids();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "each interrupt gets its own request id");
        assert!(
            uuid::Uuid::parse_str(&ids[0]).is_ok(),
            "{:?} is not a uuid",
            ids[0]
        );
    }

    #[tokio::test]
    async fn interrupt_propagates_a_transport_failure() {
        let client = unspawnable_client(ClientMode::Interactive);
        *client.current_transport.write().await =
            Some(Double::new(Feed::Live).failing_control().boxed());

        let error = client.interrupt().await.expect_err("the transport refuses");
        assert!(
            matches!(error, SdkError::ConnectionError(ref message) if message == "control channel is closed"),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn ending_a_session_that_never_started_is_a_no_op() {
        let client = unspawnable_client(ClientMode::Interactive);
        client.end_interactive_session().await.expect("ok");
        assert_eq!(idle_count(&client).await, 0);
        assert!(client.message_rx.read().await.is_none());
    }

    /// `send_interactive` and `interrupt` both take the read lock, drop it, then
    /// take the write lock, and report "Transport lost during operation" if the
    /// slot emptied in between. Nothing awaits between the two locks when they
    /// are uncontended, so landing in that window takes hand-driven polling:
    /// a competing writer has to be queued *behind* the read and granted the
    /// write lock first.
    async fn transport_taken_between_the_two_locks<F>(
        slot: &Arc<RwLock<Option<Box<dyn Transport + Send>>>>,
        operation: F,
    ) -> SdkError
    where
        F: Future<Output = Result<()>>,
    {
        let blocker = slot.write().await;

        let mut call = tokio_test::task::spawn(operation);
        assert!(
            call.poll().is_pending(),
            "the call has to queue on the read lock"
        );

        let other = slot.clone();
        let mut taker = tokio_test::task::spawn(async move { other.write().await.take() });
        assert!(
            taker.poll().is_pending(),
            "the competing writer queues behind that read"
        );

        drop(blocker);
        assert!(
            call.poll().is_pending(),
            "the read is granted first; the call then queues for the write lock"
        );
        assert!(
            matches!(taker.poll(), Poll::Ready(Some(_))),
            "the competing writer gets the write lock first and empties the slot"
        );

        match call.poll() {
            Poll::Ready(Err(error)) => error,
            other => panic!("expected the call to fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_interactive_reports_a_transport_taken_mid_call() {
        let client = unspawnable_client(ClientMode::Interactive);
        *client.current_transport.write().await = Some(Double::new(Feed::Live).boxed());

        let error = transport_taken_between_the_two_locks(
            &client.current_transport,
            client.send_interactive("x".to_string()),
        )
        .await;
        assert_eq!(
            invalid_state_message(error),
            "Transport lost during operation"
        );
    }

    #[tokio::test]
    async fn interrupt_reports_a_transport_taken_mid_call() {
        let client = unspawnable_client(ClientMode::Interactive);
        *client.current_transport.write().await = Some(Double::new(Feed::Live).boxed());

        let error =
            transport_taken_between_the_two_locks(&client.current_transport, client.interrupt())
                .await;
        assert_eq!(
            invalid_state_message(error),
            "Transport lost during operation"
        );
    }

    // =====================================================================
    // Batch mode
    // =====================================================================

    #[tokio::test]
    async fn a_batch_is_refused_in_any_other_mode() {
        let client = unspawnable_client(ClientMode::Interactive);
        let error = client
            .process_batch(vec!["x".to_string()])
            .await
            .expect_err("only batch clients process batches");
        assert_eq!(invalid_state_message(error), "Client not in batch mode");
    }

    #[tokio::test]
    async fn an_empty_batch_produces_no_results() {
        let client = unspawnable_client(ClientMode::Batch { max_concurrent: 2 });
        let results = client.process_batch(Vec::new()).await.expect("batch");
        assert!(results.is_empty());
    }

    /// `max_concurrent = 1` serialises the batch over the single pooled
    /// connection, and the clones share the budget manager.
    #[tokio::test]
    async fn a_batch_runs_every_prompt_over_the_pooled_connection() {
        let client = unspawnable_client(ClientMode::Batch { max_concurrent: 1 });
        let double = Double::scripted(one_turn("ok"));
        let handle = double.handle();
        prime(&client, double.boxed()).await;

        let results = client
            .process_batch(vec!["un".to_string(), "deux".to_string()])
            .await
            .expect("batch");

        assert_eq!(results.len(), 2);
        for result in &results {
            assert!(result.is_ok(), "every prompt succeeds: {result:?}");
        }
        assert_eq!(
            handle.sent_texts(),
            vec!["un".to_string(), "deux".to_string()],
            "one permit means one prompt at a time, in order"
        );
        assert_eq!(
            client.get_usage_stats().await.session_count,
            2,
            "the clones bill the original's budget manager"
        );
    }

    /// A task that panics is reported as a failed element, not propagated.
    #[tokio::test]
    async fn a_panicking_task_becomes_a_failed_batch_element() {
        let client = unspawnable_client(ClientMode::Batch { max_concurrent: 1 });
        prime(&client, Double::new(Feed::Script).panicking_send().boxed()).await;

        let results = client
            .process_batch(vec!["boom".to_string()])
            .await
            .expect("the batch itself still succeeds");

        assert_eq!(results.len(), 1);
        match results.into_iter().next().expect("one result") {
            Err(SdkError::TransportError(message)) => {
                assert!(
                    message.starts_with("Task failed:"),
                    "the JoinError must be named: {message}"
                );
            },
            other => panic!("expected a reported task failure, got {other:?}"),
        }
    }

    /// `Batch { max_concurrent: 0 }` builds a semaphore with no permits, so the
    /// very first `acquire_owned` waits for ever: the batch never starts and
    /// never fails. Not fixed here — the guard belongs in `new`, which would
    /// change what `ClientMode::Batch { max_concurrent: 0 }` means.
    #[tokio::test(start_paused = true)]
    async fn a_batch_limited_to_zero_concurrency_never_starts() {
        let client = unspawnable_client(ClientMode::Batch { max_concurrent: 0 });
        prime(&client, Double::scripted(one_turn("never reached")).boxed()).await;

        let outcome = tokio::time::timeout(
            Duration::from_secs(300),
            client.process_batch(vec!["x".to_string()]),
        )
        .await;

        assert!(
            outcome.is_err(),
            "documented defect: max_concurrent = 0 hangs instead of refusing"
        );
    }

    // =====================================================================
    // Clone
    // =====================================================================

    #[tokio::test]
    async fn test_client_cloning() {
        let options = ClaudeCodeOptions::builder().build();
        let client = OptimizedClient::new(options, ClientMode::OneShot).unwrap();

        let cloned = client.clone();

        match (client.mode, cloned.mode) {
            (ClientMode::OneShot, ClientMode::OneShot) => (),
            _ => panic!("Mode not preserved during cloning"),
        }
    }

    /// A clone shares the pool and the budget, but starts without a session: the
    /// batch workers depend on both halves of that.
    #[tokio::test]
    async fn a_clone_shares_the_pool_and_the_budget_but_not_the_session() {
        let client = unspawnable_client(ClientMode::Batch { max_concurrent: 3 });
        *client.current_transport.write().await = Some(Double::new(Feed::Live).boxed());
        let (_tx, rx) = mpsc::channel(1);
        *client.message_rx.write().await = Some(rx);

        let clone = client.clone();
        assert!(
            Arc::ptr_eq(&client.pool, &clone.pool),
            "the connection pool is shared"
        );
        assert!(
            clone.current_transport.read().await.is_none(),
            "a clone starts without an interactive session"
        );
        assert!(clone.message_rx.read().await.is_none());
        assert!(matches!(
            clone.mode,
            ClientMode::Batch { max_concurrent: 3 }
        ));

        clone
            .set_budget_limit(BudgetLimit::with_tokens(10), None)
            .await;
        prime(&clone, Double::scripted(one_turn("ok")).boxed()).await;
        clone.query("ping".to_string()).await.expect("query");

        assert_eq!(
            client.get_usage_stats().await.total_tokens(),
            33,
            "the clone bills the original's budget manager"
        );
        assert!(
            client.is_budget_exceeded().await,
            "and the limit the clone set is visible from the original"
        );
    }
}
