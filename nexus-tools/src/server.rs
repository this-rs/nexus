//! The MCP dispatcher and its stdio transport.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::limits::{DEFAULT_MAX_OUTPUT_CHARS, truncate};
use crate::profile::Profile;
use crate::protocol::{self, code, error_response, id_key, result_response};
use crate::registry::ToolRegistry;
use crate::tool::{CallContext, Notifier, SessionState, ToolResult};

/// One session on a server: who it is and its state.
#[derive(Debug, Clone)]
pub struct Session {
    /// The tools it may use.
    pub profile: Profile,
    /// Its state, shared by its calls.
    pub state: Arc<SessionState>,
    /// Where tools may send notifications (set by transports that have a channel back).
    pub notifier: Option<Notifier>,
}

impl Session {
    /// A session with fresh state.
    pub fn new(profile: Profile) -> Self {
        Self {
            profile,
            state: Arc::new(SessionState::default()),
            notifier: None,
        }
    }
}

/// The server: a registry and the rules every answer follows.
#[derive(Debug, Clone)]
pub struct Server {
    registry: ToolRegistry,
    max_output_chars: usize,
}

impl Server {
    /// A server over `registry`, with the default output cap.
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
        }
    }

    /// Sets how many characters of a tool result are kept.
    pub fn with_max_output_chars(mut self, max: usize) -> Self {
        self.max_output_chars = max;
        self
    }

    /// The registry.
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Answers one JSON-RPC message; `None` for a notification.
    pub async fn handle(&self, session: &Session, message: Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(error_response(
                Value::Null,
                code::INVALID_REQUEST,
                "not a JSON-RPC object",
            ));
        };
        let id = object.get("id").cloned();
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            // A response to something we never asked, or garbage.
            return id.map(|id| error_response(id, code::INVALID_REQUEST, "missing method"));
        };
        let Some(id) = id else {
            return None; // a notification: initialized, cancelled… nothing to answer
        };
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        Some(match method {
            "initialize" => result_response(id, self.initialize(&params)),
            "ping" => result_response(id, json!({})),
            "tools/list" => result_response(id, self.list(session)),
            "tools/call" => match self.call(session, &params).await {
                Ok(result) => result_response(id, result),
                Err((code, message)) => error_response(id, code, message),
            },
            other => error_response(
                id,
                code::METHOD_NOT_FOUND,
                format!("unknown method: {other}"),
            ),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let version = protocol::negotiate(params.get("protocolVersion").and_then(Value::as_str));
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "nexus-tools", "version": env!("CARGO_PKG_VERSION")},
        })
    }

    fn list(&self, session: &Session) -> Value {
        let tools: Vec<Value> = self
            .registry
            .visible(&session.profile)
            .iter()
            .map(|tool| {
                let a = tool.annotations();
                json!({
                    "name": tool.name(),
                    "description": tool.description(),
                    "inputSchema": tool.input_schema(),
                    "annotations": {
                        "readOnlyHint": a.read_only,
                        "destructiveHint": a.destructive,
                        "idempotentHint": a.idempotent,
                        "openWorldHint": a.open_world,
                    },
                })
            })
            .collect();
        json!({ "tools": tools })
    }

    async fn call(&self, session: &Session, params: &Value) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(Value::as_str).ok_or((
            code::INVALID_PARAMS,
            "tools/call needs a tool name".to_owned(),
        ))?;
        // Unknown and forbidden are one answer: the caller learns nothing about tools it
        // may not use (outside the profile, or left out of the process by `--tools`).
        let tool = self
            .registry
            .resolve(&session.profile, name)
            .ok_or_else(|| {
                (
                    code::INVALID_PARAMS,
                    format!("unknown tool: {name} (not served to this session)"),
                )
            })?;
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(object @ Value::Object(_)) => object.clone(),
            Some(_) => {
                return Err((
                    code::INVALID_PARAMS,
                    "arguments must be an object".to_owned(),
                ));
            },
        };
        let context = CallContext {
            session_id: session.profile.session_id.clone(),
            state: Arc::clone(&session.state),
            notifier: session.notifier.clone(),
        };
        let started = Instant::now();
        // In its own task: a tool that panics is one failed call, not a dead server.
        // Dropping this future (the request was cancelled) must stop the tool too: a bare
        // `JoinHandle` detaches on drop and the tool would run on.
        let outcome = AbortOnDrop(tokio::spawn(
            async move { tool.call(&context, arguments).await },
        ))
        .await;
        let result = match outcome {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => ToolResult::error("the call was cancelled"),
            Err(_) => ToolResult::error("the tool failed unexpectedly"),
        };
        // Name and outcome only: arguments and results may hold anything.
        tracing::info!(
            tool = name,
            is_error = result.is_error,
            ms = started.elapsed().as_millis() as u64,
            "tool call"
        );
        let mut content =
            vec![json!({"type": "text", "text": truncate(&result.text, self.max_output_chars)})];
        content.extend(
            result
                .images
                .iter()
                .map(|i| json!({"type": "image", "data": i.data, "mimeType": i.mime_type})),
        );
        Ok(json!({"content": content, "isError": result.is_error}))
    }
}

/// A task handle that aborts the task when dropped.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> std::future::Future for AbortOnDrop<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

/// Serves one session over a line-delimited stream (stdio): requests run concurrently,
/// `notifications/cancelled` aborts the call it names, and the end of input ends the
/// session (its calls are aborted: the client is gone).
pub async fn serve_lines<R, W>(server: Arc<Server>, session: Session, reader: R, writer: W)
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut lines) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(line) = lines.recv().await {
            // An empty line is the end marker: other holders of a sender (a notifier kept
            // by a background task) must not keep the session open after its input ended.
            if line.is_empty() {
                break;
            }
            if writer.write_all(line.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
                || writer.flush().await.is_err()
            {
                break;
            }
        }
    });
    let notify_out = out.clone();
    let session = Session {
        notifier: Some(Notifier::new(move |notification| {
            let _ = notify_out.send(notification.to_string());
        })),
        ..session
    };
    let inflight: Arc<Mutex<HashMap<String, AbortHandle>>> = Arc::default();
    let mut reader = reader;
    let mut buffer = String::new();
    loop {
        buffer.clear();
        match reader.read_line(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {},
        }
        let text = buffer.trim();
        if text.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(text) {
            Ok(message) => message,
            Err(_) => {
                let _ = out.send(
                    error_response(Value::Null, code::PARSE_ERROR, "invalid JSON").to_string(),
                );
                continue;
            },
        };
        if message.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
            if let Some(request) = message.pointer("/params/requestId") {
                let handle = inflight
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id_key(request));
                if let Some(handle) = handle {
                    handle.abort();
                }
            }
            continue;
        }
        let key = message.get("id").map(id_key);
        let (server, session, out, tracked) = (
            Arc::clone(&server),
            session.clone(),
            out.clone(),
            Arc::clone(&inflight),
        );
        let task_key = key.clone();
        let task = tokio::spawn(async move {
            if let Some(response) = server.handle(&session, message).await {
                let _ = out.send(response.to_string());
            }
            if let Some(key) = task_key {
                tracked
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&key);
            }
        });
        if let Some(key) = key {
            inflight
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key, task.abort_handle());
        }
    }
    for (_, handle) in inflight
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain()
    {
        handle.abort();
    }
    let _ = out.send(String::new());
    let _ = writer_task.await;
}
