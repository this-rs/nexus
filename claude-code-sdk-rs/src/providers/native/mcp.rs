//! A minimal MCP client: just what the tool loop needs (`initialize`,
//! `notifications/initialized`, `tools/list`, `tools/call`, cancellation).
//!
//! Two transports, chosen by the [`McpServerSpec`] of the session:
//!
//! - **stdio** — the process is started by [`isolated_command`] only (the guard
//!   test forbids any other spawn under `providers/`): empty environment, the base
//!   allowlist plus `EnvSpec.inherit`, then `EnvSpec.set`, then the server's own
//!   `env`. Nothing the host process holds is inherited by name; nothing secret is
//!   on `argv` (the spec's `args` are the caller's, the host's variables never are).
//!   Messages are newline-delimited JSON; stderr is discarded. A server that exits
//!   while the session lives is reported as [`McpError::Died`].
//! - **http** (streamable HTTP) — one `POST` per message, `Accept: application/json,
//!   text/event-stream`; the answer is a JSON body or an SSE stream carrying it;
//!   the `Mcp-Session-Id` the server returns is sent back. The configured headers
//!   (the authorisation among them) are sent as sensitive values. The URL goes
//!   through [`EndpointGuard`] at every request (https, or http to the loopback
//!   only; internal ranges refused; the connection pinned on the validated
//!   addresses), no proxy, no redirect. No error carries the URL, a header or more
//!   than a short redacted fragment of an answer.
//!
//! The legacy HTTP+SSE transport (`McpServerSpec::Sse`) is not implemented: it is
//! refused with `Unsupported { mcp_sse }`.
//!
//! A call can be cancelled by any future the caller supplies: the client then
//! sends `notifications/cancelled` for the request and stops waiting.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, Response};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::oneshot;

use crate::agent::{EnvSpec, McpServerSpec, ProviderError, redact};
use crate::model::{DnsResolver, EndpointGuard, SseDecoder};
use crate::providers::lines::{BoundedLines, Line, too_long_error};
use crate::transport::spawn::{EnvPolicy, isolated_command};

/// Protocol revision announced at `initialize`.
pub const PROTOCOL_VERSION: &str = "2025-03-26";

/// Longest answer body read from an HTTP MCP server.
const BODY_LIMIT: usize = 16 * 1024 * 1024;
/// Pages of `tools/list` followed at most.
const MAX_LIST_PAGES: usize = 50;

/// Limits and policy of the MCP clients of a provider.
#[derive(Clone)]
pub struct McpConfig {
    /// Time allowed to start a server and complete `initialize`.
    pub connect_timeout: Duration,
    /// Time allowed to one request (a tool call, a listing).
    pub call_timeout: Duration,
    /// Accept private ranges (RFC 1918) for HTTP servers; off by default.
    pub allow_private_network: bool,
    /// Longest tool output kept, in bytes; the rest is cut and said so.
    pub max_output_bytes: usize,
    /// Give stdio servers a dedicated `HOME` (contract §11) instead of the host's.
    pub isolated_home: bool,
    /// Another DNS resolver for the HTTP servers' guard (tests, hosts with their
    /// own); `None` uses the system's.
    pub dns_resolver: Option<Arc<dyn DnsResolver>>,
}

impl std::fmt::Debug for McpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConfig")
            .field("connect_timeout", &self.connect_timeout)
            .field("call_timeout", &self.call_timeout)
            .field("allow_private_network", &self.allow_private_network)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("isolated_home", &self.isolated_home)
            .field("dns_resolver", &self.dns_resolver.is_some())
            .finish()
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            call_timeout: Duration::from_secs(300),
            allow_private_network: false,
            max_output_bytes: 100_000,
            isolated_home: true,
            dns_resolver: None,
        }
    }
}

/// What a stdio server needs to be started the way the session wants.
#[derive(Debug, Clone)]
pub struct McpLaunch {
    /// Working directory of the server.
    pub cwd: PathBuf,
    /// The session's environment: names inherited and variables set.
    pub env: EnvSpec,
    /// Dedicated `HOME`, when the provider isolates it.
    pub home: Option<PathBuf>,
}

/// A tool as the server lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    /// Name the server knows it by.
    pub name: String,
    /// Description given to the model.
    pub description: String,
    /// JSON schema of the arguments.
    pub input_schema: Value,
    /// The server declared `annotations.readOnlyHint: true`.
    pub read_only: bool,
}

/// What a tool call returned.
#[derive(Debug, Clone, PartialEq)]
pub struct McpCallResult {
    /// The text blocks joined by a line feed (other blocks are named, not shown).
    pub text: String,
    /// The server flagged the result as an error (`isError`), or answered with a
    /// JSON-RPC error.
    pub is_error: bool,
}

/// Why a request got no answer.
#[derive(Debug, Clone, PartialEq)]
pub enum McpError {
    /// The caller's cancel future completed first.
    Cancelled,
    /// The server process exited (stdio): the exit code when known.
    Died {
        /// Exit code of the process.
        code: Option<i32>,
    },
    /// The server answered a JSON-RPC error.
    Rpc {
        /// The error message, redacted.
        message: String,
    },
    /// Anything else, already typed.
    Failed(ProviderError),
}

impl McpError {
    fn into_provider(self) -> ProviderError {
        match self {
            Self::Cancelled => ProviderError::protocol("the MCP request was cancelled"),
            Self::Died { code } => ProviderError::ProcessExited { code },
            Self::Rpc { message } => {
                ProviderError::protocol(format!("MCP server error: {message}"))
            },
            Self::Failed(error) => error,
        }
    }
}

type Reply = Result<Value, McpError>;

fn rpc_error(value: &Value) -> McpError {
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("error without message");
    McpError::Rpc {
        message: redact(message),
    }
}

/// A connected MCP server.
pub struct McpClient {
    name: String,
    transport: Transport,
    config: McpConfig,
}

enum Transport {
    Stdio(Arc<StdioInner>),
    Http(Arc<HttpInner>),
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl McpClient {
    /// Starts or reaches the server and completes the MCP handshake.
    pub async fn connect(
        name: &str,
        spec: &McpServerSpec,
        launch: &McpLaunch,
        config: &McpConfig,
    ) -> Result<Self, ProviderError> {
        let transport = match spec {
            McpServerSpec::Stdio { command, args, env } => {
                Transport::Stdio(StdioInner::start(command, args, env, launch)?)
            },
            McpServerSpec::Http { url, headers } => {
                Transport::Http(HttpInner::new(url, headers, config)?)
            },
            McpServerSpec::Sse { .. } => return Err(ProviderError::unsupported("mcp_sse")),
            #[allow(unreachable_patterns)]
            _ => return Err(ProviderError::unsupported("mcp_transport")),
        };
        let client = Self {
            name: name.to_owned(),
            transport,
            config: config.clone(),
        };
        let handshake = async {
            client
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": {"name": "nexus-claude", "version": env!("CARGO_PKG_VERSION")},
                    }),
                    std::future::pending(),
                    config.connect_timeout,
                )
                .await
                .map_err(McpError::into_provider)?;
            client
                .notify("notifications/initialized", json!({}))
                .await
                .map_err(McpError::into_provider)
        };
        match tokio::time::timeout(config.connect_timeout, handshake).await {
            Ok(Ok(())) => Ok(client),
            Ok(Err(error)) => {
                client.close().await;
                Err(error)
            },
            Err(_) => {
                client.close().await;
                Err(ProviderError::Timeout {
                    after_ms: config.connect_timeout.as_millis() as u64,
                })
            },
        }
    }

    /// The name of the server in the session's spec.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Lists every tool of the server (all pages).
    pub async fn list_tools(&self) -> Result<Vec<McpTool>, ProviderError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let page = self
                .request(
                    "tools/list",
                    params,
                    std::future::pending(),
                    self.config.call_timeout,
                )
                .await
                .map_err(McpError::into_provider)?;
            let listed = page
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| ProviderError::protocol("tools/list answered no `tools` array"))?;
            for entry in listed {
                if let Some(tool) = parse_tool(entry) {
                    tools.push(tool);
                }
            }
            match page.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
                _ => return Ok(tools),
            }
        }
        Ok(tools)
    }

    /// Calls a tool. `cancel` completing first cancels the call (the server is
    /// told, the caller gets [`McpError::Cancelled`]). A tool that fails (`isError`
    /// or a JSON-RPC error) is a **result**, not an `Err`: the model must see it.
    pub async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        cancel: impl Future<Output = ()> + Send,
    ) -> Result<McpCallResult, McpError> {
        let outcome = self
            .request(
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
                cancel,
                self.config.call_timeout,
            )
            .await;
        match outcome {
            Ok(result) => Ok(parse_call_result(&result, self.config.max_output_bytes)),
            Err(McpError::Rpc { message }) => Ok(McpCallResult {
                text: message,
                is_error: true,
            }),
            Err(other) => Err(other),
        }
    }

    /// Stops the server (kills the process, ends the HTTP session). Idempotent.
    pub async fn close(&self) {
        match &self.transport {
            Transport::Stdio(inner) => inner.shutdown(),
            Transport::Http(inner) => inner.end_session().await,
        }
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: impl Future<Output = ()> + Send,
        timeout: Duration,
    ) -> Reply {
        match &self.transport {
            Transport::Stdio(inner) => inner.request(method, params, cancel, timeout).await,
            Transport::Http(inner) => inner.request(method, params, cancel, timeout).await,
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        match &self.transport {
            Transport::Stdio(inner) => inner.notify(method, params).await,
            Transport::Http(inner) => inner.notify(method, params).await,
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Transport::Stdio(inner) = &self.transport {
            inner.shutdown();
        }
    }
}

fn parse_tool(entry: &Value) -> Option<McpTool> {
    let name = entry.get("name")?.as_str()?.to_owned();
    Some(McpTool {
        name,
        description: entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        input_schema: entry
            .get("inputSchema")
            .filter(|schema| schema.is_object())
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        read_only: entry
            .pointer("/annotations/readOnlyHint")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn truncate_output(mut text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n[output truncated]");
    text
}

/// Turns a `tools/call` result into text for the model.
pub fn parse_call_result(result: &Value, limit: usize) -> McpCallResult {
    let mut parts: Vec<String> = Vec::new();
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    parts.push(
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    );
                },
                Some(other) => parts.push(format!("[{other} content not shown]")),
                None => {},
            }
        }
    }
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        parts.push(structured.to_string());
    }
    McpCallResult {
        text: truncate_output(parts.join("\n"), limit),
        is_error: result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

// ---------------------------------------------------------------------------
// stdio
// ---------------------------------------------------------------------------

struct StdioInner {
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    child: tokio::sync::Mutex<Child>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    next_id: AtomicU64,
    /// `Some(code)` once the process is gone.
    dead: Mutex<Option<Option<i32>>>,
    closing: AtomicBool,
}

impl StdioInner {
    fn start(
        command: &str,
        args: &[String],
        server_env: &std::collections::BTreeMap<String, String>,
        launch: &McpLaunch,
    ) -> Result<Arc<Self>, ProviderError> {
        let mut policy = EnvPolicy::allowlist().with_inherited(launch.env.inherit.clone());
        if let Some(home) = &launch.home {
            policy = policy.with_home(home);
        }
        let mut cmd = isolated_command(command, &policy);
        cmd.args(args)
            .current_dir(&launch.cwd)
            .envs(&launch.env.set)
            .envs(server_env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ProviderError::CliNotFound {
                    program: redact(command),
                }
            } else {
                ProviderError::protocol(format!("MCP server could not start: {:?}", error.kind()))
            }
        })?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::protocol("MCP server has no stdout"))?;
        let inner = Arc::new(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            child: tokio::sync::Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            dead: Mutex::new(None),
            closing: AtomicBool::new(false),
        });
        tokio::spawn(read_loop(Arc::clone(&inner), stdout));
        Ok(inner)
    }

    fn death(&self) -> Option<Option<i32>> {
        *self.dead.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
        if let Ok(mut stdin) = self.stdin.try_lock() {
            stdin.take();
        }
    }

    async fn write(&self, message: &Value) -> Result<(), McpError> {
        if let Some(code) = self.death() {
            return Err(McpError::Died { code });
        }
        let mut line = message.to_string();
        line.push('\n');
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(McpError::Died { code: None });
        };
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            drop(guard);
            return Err(McpError::Died {
                code: self.wait_for_exit().await,
            });
        }
        Ok(())
    }

    async fn wait_for_exit(&self) -> Option<i32> {
        for _ in 0..100 {
            if let Some(code) = self.death() {
                return code;
            }
            if let Ok(Some(status)) = self.child.lock().await.try_wait() {
                return status.code();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        None
    }

    fn fail_all(&self, code: Option<i32>) {
        *self.dead.lock().unwrap_or_else(PoisonError::into_inner) = Some(code);
        self.fail_pending(McpError::Died { code });
    }

    /// Answers every request still waiting with `error`.
    fn fail_pending(&self, error: McpError) {
        let pending: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .collect();
        for (_, reply) in pending {
            let _ = reply.send(Err(error.clone()));
        }
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: impl Future<Output = ()> + Send,
        timeout: Duration,
    ) -> Reply {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (reply, answer) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, reply);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(error) = self.write(&message).await {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            return Err(error);
        }
        tokio::select! {
            answer = answer => answer.unwrap_or(Err(McpError::Died { code: self.death().flatten() })),
            () = cancel => {
                self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                let _ = self.notify(
                    "notifications/cancelled",
                    json!({"requestId": id, "reason": "cancelled by the client"}),
                ).await;
                Err(McpError::Cancelled)
            },
            () = tokio::time::sleep(timeout) => {
                self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                let _ = self.notify(
                    "notifications/cancelled",
                    json!({"requestId": id, "reason": "timeout"}),
                ).await;
                Err(McpError::Failed(ProviderError::Timeout { after_ms: timeout.as_millis() as u64 }))
            },
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }
}

async fn read_loop(inner: Arc<StdioInner>, stdout: ChildStdout) {
    let mut lines = BoundedLines::new(stdout);
    while let Ok(Some(line)) = lines.next_line().await {
        let line = match line {
            Line::Text(line) => line,
            // A reply may have been the one dropped: nobody waits for it forever.
            Line::TooLong => {
                inner.fail_pending(McpError::Failed(too_long_error()));
                continue;
            },
        };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let is_response = message.get("result").is_some() || message.get("error").is_some();
        match (message.get("id"), is_response) {
            (Some(id), true) => {
                let Some(id) = id.as_u64() else { continue };
                let reply = inner
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                if let Some(reply) = reply {
                    let value = match message.get("error") {
                        Some(error) => Err(rpc_error(error)),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = reply.send(value);
                }
            },
            (Some(id), false) if message.get("method").is_some() => {
                // A request from the server: only `ping` is answered.
                let answer = if message["method"] == "ping" {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})
                };
                let _ = inner.write(&answer).await;
            },
            _ => {},
        }
    }
    let code = inner.wait_for_exit().await;
    inner.fail_all(code);
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

struct HttpInner {
    url: String,
    headers: HeaderMap,
    guard: EndpointGuard,
    client: Mutex<Option<(String, Vec<SocketAddr>, Client)>>,
    session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
    call_timeout: Duration,
}

fn map_http_error(error: reqwest::Error, limit: Duration) -> McpError {
    if error.is_timeout() {
        return McpError::Failed(ProviderError::Timeout {
            after_ms: limit.as_millis() as u64,
        });
    }
    let kind = if error.is_connect() {
        "connection failed"
    } else {
        "request failed"
    };
    // `without_url`: the URL may carry a credential.
    McpError::Failed(ProviderError::unreachable(format!(
        "MCP server unreachable ({kind}): {}",
        redact(&error.without_url().to_string())
    )))
}

impl HttpInner {
    fn new(
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
        config: &McpConfig,
    ) -> Result<Arc<Self>, ProviderError> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ProviderError::invalid("an MCP header name is not valid"))?;
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| ProviderError::invalid("an MCP header value is not valid"))?;
            value.set_sensitive(true);
            map.insert(name, value);
        }
        let mut guard = EndpointGuard::new().allow_private_network(config.allow_private_network);
        if let Some(resolver) = &config.dns_resolver {
            guard = guard.with_resolver(Arc::clone(resolver));
        }
        Ok(Arc::new(Self {
            url: url.to_owned(),
            headers: map,
            guard,
            client: Mutex::new(None),
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
            call_timeout: config.call_timeout,
        }))
    }

    async fn prepare(&self) -> Result<(reqwest::Url, Client), McpError> {
        let checked = self
            .guard
            .check(&self.url)
            .await
            .map_err(McpError::Failed)?;
        if let Some((host, addrs, client)) = self
            .client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            && *host == checked.host
            && *addrs == checked.addrs
        {
            return Ok((checked.url.clone(), client.clone()));
        }
        let mut builder = Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("nexus-claude/", env!("CARGO_PKG_VERSION")));
        if !checked.ip_literal {
            builder = builder.resolve_to_addrs(&checked.host, &checked.addrs);
        }
        let client = builder.build().map_err(|_| {
            McpError::Failed(ProviderError::unreachable("HTTP client could not be built"))
        })?;
        *self.client.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((checked.host.clone(), checked.addrs.clone(), client.clone()));
        Ok((checked.url, client))
    }

    async fn post(&self, body: &Value, expect: Option<u64>, timeout: Duration) -> Reply {
        let (url, client) = self.prepare().await?;
        let mut builder = client
            .post(url)
            .headers(self.headers.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .body(body.to_string());
        if let Some(session) = self
            .session_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            builder = builder.header("mcp-session-id", session);
        }
        let response = match tokio::time::timeout(timeout, builder.send()).await {
            Err(_) => {
                return Err(McpError::Failed(ProviderError::Timeout {
                    after_ms: timeout.as_millis() as u64,
                }));
            },
            Ok(Err(error)) => return Err(map_http_error(error, timeout)),
            Ok(Ok(response)) => response,
        };
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *self
                .session_id
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(session.to_owned());
        }
        let status = response.status();
        if !status.is_success() {
            return Err(McpError::Failed(match status.as_u16() {
                401 | 403 => ProviderError::Unauthorized,
                code @ 300..=399 => ProviderError::protocol(format!(
                    "MCP server answered a redirect (HTTP {code}), refused"
                )),
                429 => ProviderError::RateLimited {
                    retry_after_ms: None,
                },
                503 => ProviderError::Overloaded,
                code => ProviderError::protocol(format!("MCP server answered HTTP {code}")),
            }));
        }
        let Some(id) = expect else {
            return Ok(Value::Null);
        };
        let is_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        let message = if is_stream {
            read_sse_answer(response, id, timeout).await?
        } else {
            read_json_answer(response, id, timeout).await?
        };
        match message.get("error") {
            Some(error) => Err(rpc_error(error)),
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        }
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: impl Future<Output = ()> + Send,
        timeout: Duration,
    ) -> Reply {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        tokio::select! {
            reply = self.post(&message, Some(id), timeout) => reply,
            () = cancel => {
                let told = self.notify(
                    "notifications/cancelled",
                    json!({"requestId": id, "reason": "cancelled by the client"}),
                );
                let _ = tokio::time::timeout(Duration::from_secs(2), told).await;
                Err(McpError::Cancelled)
            },
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.post(&message, None, self.call_timeout)
            .await
            .map(|_| ())
    }

    async fn end_session(&self) {
        let Some(session) = self
            .session_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let Ok((url, client)) = self.prepare().await else {
            return;
        };
        let request = client
            .delete(url)
            .headers(self.headers.clone())
            .header("mcp-session-id", session)
            .send();
        let _ = tokio::time::timeout(Duration::from_secs(2), request).await;
    }
}

fn answer_for(message: &Value, id: u64) -> bool {
    message.get("id").and_then(Value::as_u64) == Some(id)
        && (message.get("result").is_some() || message.get("error").is_some())
}

async fn read_json_answer(
    mut response: Response,
    id: u64,
    timeout: Duration,
) -> Result<Value, McpError> {
    let mut body = Vec::new();
    loop {
        match tokio::time::timeout(timeout, response.chunk()).await {
            Err(_) => {
                return Err(McpError::Failed(ProviderError::Timeout {
                    after_ms: timeout.as_millis() as u64,
                }));
            },
            Ok(Err(error)) => return Err(map_http_error(error, timeout)),
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => {
                body.extend_from_slice(&chunk);
                if body.len() > BODY_LIMIT {
                    return Err(McpError::Failed(ProviderError::protocol(
                        "MCP answer is too large",
                    )));
                }
            },
        }
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| McpError::Failed(ProviderError::protocol("MCP answer is not valid JSON")))?;
    let found = match value {
        Value::Array(batch) => batch.into_iter().find(|message| answer_for(message, id)),
        single if answer_for(&single, id) => Some(single),
        _ => None,
    };
    found.ok_or_else(|| McpError::Failed(ProviderError::protocol("MCP answer carries no response")))
}

async fn read_sse_answer(
    mut response: Response,
    id: u64,
    timeout: Duration,
) -> Result<Value, McpError> {
    let mut decoder = SseDecoder::new();
    let mut seen = 0usize;
    loop {
        let chunk = match tokio::time::timeout(timeout, response.chunk()).await {
            Err(_) => {
                return Err(McpError::Failed(ProviderError::Timeout {
                    after_ms: timeout.as_millis() as u64,
                }));
            },
            Ok(Err(error)) => return Err(map_http_error(error, timeout)),
            Ok(Ok(chunk)) => chunk,
        };
        let events = match &chunk {
            Some(bytes) => decoder.push(bytes).map_err(McpError::Failed)?,
            None => decoder.finish().into_iter().collect(),
        };
        for event in events {
            seen += 1;
            if let Ok(message) = serde_json::from_str::<Value>(&event.data)
                && answer_for(&message, id)
            {
                return Ok(message);
            }
        }
        if chunk.is_none() {
            return Err(McpError::Failed(ProviderError::protocol(format!(
                "MCP stream ended without the response ({seen} event(s))"
            ))));
        }
    }
}

/// Directory used as the `HOME` of the stdio servers of an instance: created
/// `0700` under the system temp directory, named after the instance.
pub fn isolated_home(instance: &str) -> Option<PathBuf> {
    let safe: String = instance
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = std::env::temp_dir().join(format!("nexus-native-home-{safe}"));
    create_private(&dir).ok()?;
    Some(dir)
}

fn create_private(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers each call with the next address of its list (the last one stays).
    struct SequenceResolver(Vec<std::net::IpAddr>, std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl DnsResolver for SequenceResolver {
        async fn resolve(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            let call = self.1.fetch_add(1, Ordering::SeqCst);
            Ok(vec![SocketAddr::new(
                self.0[call.min(self.0.len() - 1)],
                port,
            )])
        }
    }

    fn sequence(addresses: &[&str]) -> Arc<SequenceResolver> {
        Arc::new(SequenceResolver(
            addresses.iter().map(|a| a.parse().unwrap()).collect(),
            std::sync::atomic::AtomicUsize::new(0),
        ))
    }

    /// A listener that only counts TCP connections (the TLS handshake that follows
    /// an `https` request to it fails, which is irrelevant here).
    async fn counting_listener() -> (u16, Arc<AtomicU64>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&accepted);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                drop(socket);
            }
        });
        (port, accepted)
    }

    fn launch() -> McpLaunch {
        McpLaunch {
            cwd: std::env::temp_dir(),
            env: EnvSpec::default(),
            home: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stdio_server_printing_an_over_long_line_fails_the_request_with_a_protocol_error() {
        // 9 MiB of `a` on one line, then silence: the line is over the 8 MiB bound.
        let script = "head -c 9437184 /dev/zero | tr '\\000' a; echo; sleep 30";
        let config = McpConfig {
            connect_timeout: Duration::from_secs(20),
            ..McpConfig::default()
        };
        let started = std::time::Instant::now();
        let error = McpClient::connect(
            "x",
            &McpServerSpec::Stdio {
                command: "sh".into(),
                args: vec!["-c".into(), script.into()],
                env: Default::default(),
            },
            &launch(),
            &config,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), "protocol", "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the request must fail at once, not at the timeout"
        );
    }

    #[tokio::test]
    async fn an_http_server_connection_is_pinned_on_the_validated_address() {
        // `mcp-pin-test.invalid` is reserved (RFC 6761) and never resolves through
        // the system: only the address the guard validated can take the connection.
        let (port, accepted) = counting_listener().await;
        let dns = sequence(&["127.0.0.1"]);
        let config = McpConfig {
            connect_timeout: Duration::from_secs(3),
            dns_resolver: Some(dns.clone()),
            ..McpConfig::default()
        };
        // The handshake fails (the listener is not TLS); the connection was made.
        let _ = McpClient::connect(
            "x",
            &McpServerSpec::Http {
                url: format!("https://mcp-pin-test.invalid:{port}/mcp"),
                headers: Default::default(),
            },
            &launch(),
            &config,
        )
        .await;
        assert_eq!(dns.1.load(Ordering::SeqCst), 1);
        assert!(
            accepted.load(Ordering::SeqCst) >= 1,
            "the client must connect to the validated address, not re-resolve the name"
        );
    }

    #[tokio::test]
    async fn an_http_server_name_that_rebinds_is_checked_again_and_refused() {
        let (port, accepted) = counting_listener().await;
        let dns = sequence(&["127.0.0.1", "10.9.8.7"]);
        let config = McpConfig {
            dns_resolver: Some(dns.clone()),
            ..McpConfig::default()
        };
        let inner = HttpInner::new(
            &format!("https://mcp-rebind.invalid:{port}/mcp"),
            &Default::default(),
            &config,
        )
        .unwrap();
        // First answer: an allowed address; the client is built on it and pinned.
        let _ = inner.post(&json!({}), None, Duration::from_secs(3)).await;
        let after_first = accepted.load(Ordering::SeqCst);
        assert!(after_first >= 1);
        // Second answer for the same name: an internal address. Refused, and the
        // client pinned on the first answer is not reused.
        let error = inner
            .post(&json!({}), None, Duration::from_secs(3))
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                McpError::Failed(ProviderError::InvalidRequest { .. })
            ),
            "{error:?}"
        );
        assert_eq!(accepted.load(Ordering::SeqCst), after_first);
        assert_eq!(dns.1.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_tool_listing_entry_is_read_with_its_read_only_hint() {
        let tool = parse_tool(&json!({
            "name": "echo",
            "description": "d",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
            "annotations": {"readOnlyHint": true},
        }))
        .unwrap();
        assert!(tool.read_only);
        assert_eq!(tool.name, "echo");
        let bare = parse_tool(&json!({"name": "w"})).unwrap();
        assert!(!bare.read_only);
        assert_eq!(bare.input_schema["type"], "object");
        assert!(parse_tool(&json!({"description": "no name"})).is_none());
    }

    #[test]
    fn a_call_result_joins_text_and_names_other_blocks() {
        let result = parse_call_result(
            &json!({"content": [
                {"type": "text", "text": "a"},
                {"type": "image", "data": "xx", "mimeType": "image/png"},
                {"type": "text", "text": "b"},
            ]}),
            1000,
        );
        assert_eq!(result.text, "a\n[image content not shown]\nb");
        assert!(!result.is_error);
        let error = parse_call_result(
            &json!({"content": [{"type": "text", "text": "boom"}], "isError": true}),
            1000,
        );
        assert!(error.is_error);
        let structured = parse_call_result(&json!({"structuredContent": {"n": 1}}), 1000);
        assert_eq!(structured.text, r#"{"n":1}"#);
    }

    #[test]
    fn a_long_output_is_cut_on_a_character_boundary_and_says_so() {
        let result = parse_call_result(
            &json!({"content": [{"type": "text", "text": "é".repeat(100)}]}),
            11,
        );
        assert!(result.text.ends_with("[output truncated]"));
        assert!(result.text.starts_with("ééééé"));
    }

    #[tokio::test]
    async fn a_missing_stdio_command_is_cli_not_found() {
        let launch = McpLaunch {
            cwd: std::env::temp_dir(),
            env: EnvSpec::default(),
            home: None,
        };
        let error = McpClient::connect(
            "x",
            &McpServerSpec::stdio("/nonexistent/nexus-mcp-server"),
            &launch,
            &McpConfig::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), "cli_not_found");
    }

    #[tokio::test]
    async fn the_legacy_sse_transport_is_refused_by_name() {
        let launch = McpLaunch {
            cwd: std::env::temp_dir(),
            env: EnvSpec::default(),
            home: None,
        };
        let error = McpClient::connect(
            "x",
            &McpServerSpec::Sse {
                url: "http://127.0.0.1:1/sse".into(),
                headers: Default::default(),
            },
            &launch,
            &McpConfig::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, ProviderError::unsupported("mcp_sse"));
    }

    #[tokio::test]
    async fn an_unreachable_http_server_is_a_typed_error_without_the_url() {
        let launch = McpLaunch {
            cwd: std::env::temp_dir(),
            env: EnvSpec::default(),
            home: None,
        };
        let config = McpConfig {
            connect_timeout: Duration::from_secs(3),
            ..McpConfig::default()
        };
        let error = McpClient::connect(
            "x",
            &McpServerSpec::Http {
                url: "http://127.0.0.1:9/secret-path-xyz".into(),
                headers: Default::default(),
            },
            &launch,
            &config,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), "endpoint_unreachable", "{error}");
        assert!(!error.to_string().contains("secret-path-xyz"));
    }

    #[tokio::test]
    async fn a_redirect_from_an_http_server_is_refused_and_not_followed() {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpListener;
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let hits = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&hits);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = target.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                    .await;
            }
        });
        let redirector = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = redirector.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = redirector.accept().await {
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer).await;
                let answer = format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{target_port}/x\r\nContent-Length: 0\r\n\r\n"
                );
                let _ = stream.write_all(answer.as_bytes()).await;
            }
        });
        let launch = McpLaunch {
            cwd: std::env::temp_dir(),
            env: EnvSpec::default(),
            home: None,
        };
        let error = McpClient::connect(
            "x",
            &McpServerSpec::Http {
                url: format!("http://127.0.0.1:{port}/mcp"),
                headers: [(
                    "Authorization".to_owned(),
                    "Bearer REDIRECT-TOKEN-123456".to_owned(),
                )]
                .into(),
            },
            &launch,
            &McpConfig::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), "protocol", "{error}");
        assert!(error.to_string().contains("redirect"), "{error}");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 0, "the redirect was followed");
    }

    #[tokio::test]
    async fn a_plain_http_server_off_the_loopback_is_refused_by_the_guard() {
        let launch = McpLaunch {
            cwd: std::env::temp_dir(),
            env: EnvSpec::default(),
            home: None,
        };
        let error = McpClient::connect(
            "x",
            &McpServerSpec::Http {
                url: "http://203.0.113.9/mcp".into(),
                headers: Default::default(),
            },
            &launch,
            &McpConfig::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), "invalid_request", "{error}");
    }
}
