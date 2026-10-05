//! [`claude_code_replay`]: a real [`ClaudeCodeProvider`] on an in-memory replay
//! transport — no executable, no process, no network.
//!
//! The provider, its session, its pump, the projection of messages and the
//! control JSON are the production code. Only the transport is replaced: it
//! plays a list of [`Message`]s for each turn and **captures every line the
//! façade writes to the CLI's stdin**, so a host can assert, from its own crate,
//! both what its code receives and what the CLI would have been told.
//!
//! # What is captured, and what is not
//!
//! Captured, in order ([`ClaudeCodeReplay::stdin_lines`]): the input message of
//! each turn (serialised exactly as the subprocess transport does), every
//! control line (permission answers, `set_model`, `set_permission_mode`,
//! interrupt, the automatic answer to `AskUserQuestion`), the hook registration
//! request and the hook responses. The options built for each session are kept
//! too ([`ClaudeCodeReplay::last_options`]).
//!
//! Not captured, because there is no process: the command line, the child's
//! environment, the MCP configuration file, signals and exit codes, the CLI
//! version probe.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::InteractiveClient;
use crate::errors::{Result, SdkError};
use crate::providers::claude_code::{ClaudeCodeConfig, ClaudeCodeProvider};
use crate::transport::{InputMessage, Transport};
use crate::types::{ClaudeCodeOptions, ControlRequest, ControlResponse, Message};

/// Capacity of the replay's channels.
const CHANNEL_CAPACITY: usize = 1024;

/// One step of a replayed turn.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplayStep {
    /// The CLI prints this message.
    Message(Message),
    /// The CLI sends this control message (a `can_use_tool` request, a
    /// `hook_callback`), verbatim.
    Control(Value),
    /// The CLI waits until a stdin line containing this text was written (the
    /// answer to a permission, an interrupt), then goes on.
    AwaitStdin(String),
    /// The CLI dies: its message stream ends.
    Exit,
}

impl From<Message> for ReplayStep {
    fn from(message: Message) -> Self {
        Self::Message(message)
    }
}

#[derive(Default)]
struct Shared {
    /// Every line written to the replayed CLI's stdin, across sessions, in order.
    stdin: Mutex<Vec<String>>,
    /// Options of the sessions opened so far.
    options: Mutex<Vec<ClaudeCodeOptions>>,
    /// Injection channel of the session opened last.
    inject: Mutex<Option<mpsc::UnboundedSender<ReplayStep>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A [`ClaudeCodeProvider`] on a replay transport, and what it captured.
#[derive(Clone)]
pub struct ClaudeCodeReplay {
    provider: Arc<ClaudeCodeProvider>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for ClaudeCodeReplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeReplay")
            .field("stdin_lines", &lock(&self.shared.stdin).len())
            .finish()
    }
}

/// A Claude Code provider whose sessions replay `turns`: the `i`-th input
/// message of a session plays `turns[i]` (nothing once the list is exhausted).
/// Every session starts again at `turns[0]`. Default instance configuration.
pub fn claude_code_replay(turns: Vec<Vec<Message>>) -> ClaudeCodeReplay {
    let turns = turns
        .into_iter()
        .map(|turn| turn.into_iter().map(ReplayStep::Message).collect())
        .collect();
    claude_code_replay_steps(ClaudeCodeConfig::default(), turns)
}

/// [`claude_code_replay`] with an instance configuration and turns that may
/// also send control requests, wait for an answer on stdin, or die.
pub fn claude_code_replay_steps(
    config: ClaudeCodeConfig,
    turns: Vec<Vec<ReplayStep>>,
) -> ClaudeCodeReplay {
    let shared = Arc::new(Shared::default());
    let factory_shared = Arc::clone(&shared);
    let provider = ClaudeCodeProvider::with_client_factory(
        config,
        Arc::new(move |options: ClaudeCodeOptions| {
            let hooks = options.hooks.clone();
            lock(&factory_shared.options).push(options);
            let transport: Box<dyn Transport + Send> = Box::new(ReplayTransport::new(
                Arc::clone(&factory_shared),
                turns.clone(),
            ));
            Ok(match hooks {
                Some(hooks) => InteractiveClient::from_transport_with_hooks(transport, hooks),
                None => InteractiveClient::from_transport(transport),
            })
        }),
    );
    ClaudeCodeReplay {
        provider: Arc::new(provider),
        shared,
    }
}

impl ClaudeCodeReplay {
    /// The provider to open sessions on.
    pub fn provider(&self) -> Arc<ClaudeCodeProvider> {
        Arc::clone(&self.provider)
    }

    /// Every line written to the CLI's stdin so far, in order.
    pub fn stdin_lines(&self) -> Vec<String> {
        lock(&self.shared.stdin).clone()
    }

    /// [`ClaudeCodeReplay::stdin_lines`] parsed as JSON.
    pub fn stdin_json(&self) -> Vec<Value> {
        self.stdin_lines()
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Waits until a stdin line containing `text` was written and returns it;
    /// `None` after `within`. Writes travel through a channel: a line is
    /// captured shortly after the call that wrote it returns.
    pub async fn wait_for_stdin(&self, text: &str, within: Duration) -> Option<String> {
        let search = async {
            loop {
                if let Some(line) = self
                    .stdin_lines()
                    .into_iter()
                    .find(|line| line.contains(text))
                {
                    return line;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(within, search).await.ok()
    }

    /// Options built for the session opened last.
    pub fn last_options(&self) -> Option<ClaudeCodeOptions> {
        lock(&self.shared.options).last().cloned()
    }

    fn inject(&self, step: ReplayStep) -> bool {
        lock(&self.shared.inject)
            .as_ref()
            .is_some_and(|inject| inject.send(step).is_ok())
    }

    /// Makes the CLI of the session opened last print a message now, whether a
    /// turn is running or not. `false` when no session is live.
    pub fn push_message(&self, message: Message) -> bool {
        self.inject(ReplayStep::Message(message))
    }

    /// Makes the CLI of the session opened last send a control message now.
    pub fn push_control(&self, control: Value) -> bool {
        self.inject(ReplayStep::Control(control))
    }

    /// Kills the CLI of the session opened last: its message stream ends.
    pub fn exit(&self) -> bool {
        self.inject(ReplayStep::Exit)
    }
}

/// The transport of one replayed session.
struct ReplayTransport {
    shared: Arc<Shared>,
    turns: Option<Vec<Vec<ReplayStep>>>,
    connected: bool,
    stdin_tx: Option<mpsc::Sender<String>>,
    /// A receiver, not a sender: the stream of messages must end when the
    /// replayed CLI does.
    messages_rx: Option<broadcast::Receiver<Message>>,
    control_rx: Option<mpsc::Receiver<Value>>,
}

impl ReplayTransport {
    fn new(shared: Arc<Shared>, turns: Vec<Vec<ReplayStep>>) -> Self {
        Self {
            shared,
            turns: Some(turns),
            connected: false,
            stdin_tx: None,
            messages_rx: None,
            control_rx: None,
        }
    }

    async fn write(&self, line: String) -> Result<()> {
        let stdin = self
            .stdin_tx
            .as_ref()
            .ok_or_else(|| SdkError::InvalidState {
                message: "Not connected".into(),
            })?;
        stdin
            .send(line)
            .await
            .map_err(|_| SdkError::TransportError("the replayed CLI is gone".into()))
    }

    fn stream(&self) -> Pin<Box<dyn Stream<Item = Result<Message>> + Send + 'static>> {
        match &self.messages_rx {
            Some(receiver) => Box::pin(
                tokio_stream::wrappers::BroadcastStream::new(receiver.resubscribe())
                    .filter_map(|item| async move { item.ok().map(Ok) }),
            ),
            None => Box::pin(futures::stream::empty()),
        }
    }
}

/// The replayed CLI: reads stdin, plays a turn for each input message.
struct Player {
    shared: Arc<Shared>,
    stdin: mpsc::Receiver<String>,
    inject: mpsc::UnboundedReceiver<ReplayStep>,
    messages: broadcast::Sender<Message>,
    control: mpsc::Sender<Value>,
}

impl Player {
    /// Next stdin line, recorded; injections are played while waiting. `None`
    /// when stdin closed or the CLI was told to exit.
    async fn next_line(&mut self) -> Option<String> {
        loop {
            tokio::select! {
                line = self.stdin.recv() => {
                    let line = line?;
                    lock(&self.shared.stdin).push(line.clone());
                    return Some(line);
                },
                Some(step) = self.inject.recv() => {
                    if !self.play(step).await {
                        return None;
                    }
                },
            }
        }
    }

    /// Plays one step; `false` when the CLI is gone.
    async fn play(&mut self, step: ReplayStep) -> bool {
        match step {
            ReplayStep::Message(message) => {
                let _ = self.messages.send(message);
                true
            },
            ReplayStep::Control(control) => {
                let _ = self.control.send(control).await;
                true
            },
            ReplayStep::AwaitStdin(text) => loop {
                match Box::pin(self.next_line()).await {
                    Some(line) if line.contains(&text) => break true,
                    Some(_) => {},
                    None => break false,
                }
            },
            ReplayStep::Exit => false,
        }
    }

    async fn run(mut self, turns: Vec<Vec<ReplayStep>>) {
        let mut turns = turns.into_iter();
        while let Some(line) = self.next_line().await {
            let is_input = serde_json::from_str::<Value>(&line)
                .is_ok_and(|value| value.get("type").and_then(Value::as_str) == Some("user"));
            if !is_input {
                continue;
            }
            for step in turns.next().unwrap_or_default() {
                if !self.play(step).await {
                    return;
                }
            }
        }
    }
}

#[async_trait]
impl Transport for ReplayTransport {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    async fn connect(&mut self) -> Result<()> {
        let Some(turns) = self.turns.take() else {
            return Ok(());
        };
        let (stdin_tx, stdin_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (messages_tx, messages_rx) = broadcast::channel(CHANNEL_CAPACITY);
        let (control_tx, control_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (inject_tx, inject_rx) = mpsc::unbounded_channel();
        *lock(&self.shared.inject) = Some(inject_tx);
        let player = Player {
            shared: Arc::clone(&self.shared),
            stdin: stdin_rx,
            inject: inject_rx,
            messages: messages_tx,
            control: control_tx,
        };
        tokio::spawn(player.run(turns));
        self.stdin_tx = Some(stdin_tx);
        self.messages_rx = Some(messages_rx);
        self.control_rx = Some(control_rx);
        self.connected = true;
        Ok(())
    }

    async fn send_message(&mut self, message: InputMessage) -> Result<()> {
        self.write(serde_json::to_string(&message)?).await
    }

    fn receive_messages(
        &mut self,
    ) -> Pin<Box<dyn Stream<Item = Result<Message>> + Send + 'static>> {
        self.stream()
    }

    fn subscribe_messages(
        &self,
    ) -> Option<Pin<Box<dyn Stream<Item = Result<Message>> + Send + 'static>>> {
        self.messages_rx.as_ref().map(|_| self.stream())
    }

    async fn send_control_request(&mut self, request: ControlRequest) -> Result<()> {
        let ControlRequest::Interrupt { request_id } = request;
        // The wire format of `SubprocessTransport::send_control_request`.
        let line = serde_json::json!({
            "type": "control_request",
            "request": { "type": "interrupt", "request_id": request_id }
        });
        self.write(line.to_string()).await
    }

    async fn receive_control_response(&mut self) -> Result<Option<ControlResponse>> {
        Ok(None)
    }

    async fn send_sdk_control_request(&mut self, request: Value) -> Result<()> {
        self.write(request.to_string()).await
    }

    async fn send_sdk_control_response(&mut self, response: Value) -> Result<()> {
        let line = serde_json::json!({ "type": "control_response", "response": response });
        self.write(line.to_string()).await
    }

    fn take_sdk_control_receiver(&mut self) -> Option<mpsc::Receiver<Value>> {
        self.control_rx.take()
    }

    fn clone_stdin_sender(&self) -> Option<mpsc::Sender<String>> {
        self.stdin_tx.clone()
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.stdin_tx = None;
        self.connected = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::{
        AgentEvent, AgentProvider, AgentSession, PermissionDecision, ProviderError, SessionSpec,
        StopReason, TurnInput,
    };

    const WAIT: Duration = Duration::from_secs(5);

    fn message(value: Value) -> Message {
        crate::message_parser::parse_message(value)
            .unwrap()
            .unwrap()
    }

    fn text(text: &str) -> Message {
        message(
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}),
        )
    }

    fn result() -> Message {
        message(json!({
            "type": "result", "subtype": "success", "duration_ms": 5, "duration_api_ms": 4,
            "is_error": false, "num_turns": 1, "session_id": "replayed", "total_cost_usd": 0.01,
            "usage": {"input_tokens": 1, "output_tokens": 2}, "result": "done"
        }))
    }

    async fn collect(session: &dyn AgentSession, prompt: &str) -> Vec<AgentEvent> {
        let stream = session.send_turn(TurnInput::text(prompt)).await.unwrap();
        tokio::time::timeout(WAIT, stream.collect::<Vec<_>>())
            .await
            .expect("the replayed turn ends")
    }

    #[tokio::test]
    async fn a_replayed_turn_reaches_the_host_and_its_input_is_captured() {
        let replay = claude_code_replay(vec![vec![text("bonjour"), result()]]);
        let session = replay
            .provider()
            .open(SessionSpec::new("/work"))
            .await
            .unwrap();
        let events = collect(&*session, "salut").await;
        assert!(matches!(&events[0], AgentEvent::Text { text, .. } if text == "bonjour"));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                stop_reason: StopReason::Completed,
                ..
            })
        ));
        assert_eq!(
            replay.stdin_lines(),
            [
                r#"{"type":"user","message":{"content":"salut","role":"user"},"parent_tool_use_id":null,"session_id":"default"}"#
            ]
        );
        assert_eq!(
            session.resume_token().unwrap().to_wire(),
            r#"{"k":"claude_code","v":1,"d":{"session_id":"replayed"}}"#
        );
        let options = replay.last_options().unwrap();
        assert_eq!(
            options.permission_prompt_tool_name.as_deref(),
            Some("stdio")
        );
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_scripted_permission_is_answered_with_the_original_input() {
        let ask = json!({
            "type": "control_request", "request_id": "req-1",
            "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"}}
        });
        let replay = claude_code_replay_steps(
            ClaudeCodeConfig::default(),
            vec![vec![
                ReplayStep::Control(ask),
                ReplayStep::AwaitStdin("req-1".to_owned()),
                text("ran").into(),
                result().into(),
            ]],
        );
        let session = replay
            .provider()
            .open(SessionSpec::new("/work"))
            .await
            .unwrap();
        let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
        let mut seen = Vec::new();
        while let Some(event) = tokio::time::timeout(WAIT, stream.next()).await.unwrap() {
            if let AgentEvent::PermissionAsk { request_id, .. } = &event {
                session
                    .answer_permission(request_id, PermissionDecision::allow_once())
                    .await
                    .unwrap();
            }
            seen.push(event);
        }
        assert!(
            seen.iter()
                .any(|event| matches!(event, AgentEvent::Text { text, .. } if text == "ran"))
        );
        assert_eq!(
            replay.stdin_lines()[1],
            r#"{"response":{"request_id":"req-1","response":{"behavior":"allow","updatedInput":{"command":"ls"}},"subtype":"success"},"type":"control_response"}"#
        );
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn injected_events_arrive_out_of_band_and_an_exit_kills_the_session() {
        let replay = claude_code_replay(vec![]);
        let session = replay
            .provider()
            .open(SessionSpec::new("/work"))
            .await
            .unwrap();
        let mut out_of_band = session.out_of_band().unwrap();
        assert!(replay.push_message(text("spontaneous")));
        let first = tokio::time::timeout(WAIT, out_of_band.next())
            .await
            .unwrap();
        assert!(matches!(first, Some(AgentEvent::Text { text, .. }) if text == "spontaneous"));
        assert!(replay.exit());
        let second = tokio::time::timeout(WAIT, out_of_band.next())
            .await
            .unwrap();
        assert_eq!(
            second,
            Some(AgentEvent::Error {
                error: ProviderError::ProcessExited { code: None }
            })
        );
        assert_eq!(
            session.send_turn(TurnInput::text("too late")).await.err(),
            Some(ProviderError::ProcessExited { code: None })
        );
        session.close().await.unwrap();
        assert_eq!(
            session.send_turn(TurnInput::text("closed")).await.err(),
            Some(ProviderError::Closed)
        );
    }
}
