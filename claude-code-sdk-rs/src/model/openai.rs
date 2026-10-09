//! [`OpenAiEndpoint`]: chat/completions over HTTP + SSE for any OpenAI-compatible
//! server (OpenAI, DeepSeek, vLLM, Ollama, llama-server, NIM, ...).
//!
//! Security properties (decision A36, contract §7 and §11):
//!
//! - every request goes through [`EndpointGuard`]; the connection is pinned on the
//!   addresses the guard validated; proxies are never used (a proxy would resolve
//!   the name itself and defeat the pin); redirects are never followed (so the
//!   credential cannot be replayed to another host);
//! - the credential is resolved by [`CredentialResolver::resolve`] for **each**
//!   request, held only for the duration of that request (or of its stream), sent as
//!   a sensitive `Authorization: Bearer` header, and registered with the redactor
//!   (`redact_with`) for every error built from the server's answer;
//! - an error never carries the credential or more than 512 bytes of the body.
//!
//! The HTTP client is cached per validated address set and rebuilt when DNS now
//! answers differently (the guard runs again at every request).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, HeaderValue, RETRY_AFTER};
use reqwest::redirect::Policy;
use reqwest::{Client, RequestBuilder, Response};
use serde_json::{Value, json};

use super::guard::{CheckedEndpoint, DnsResolver, EndpointGuard};
use super::quirks::EndpointQuirks;
use super::sse::SseDecoder;
use super::wire::{self, PROBE_TOOL, StreamParser};
use super::{
    ChatMessage, CompletionChunk, CompletionRequest, CompletionStream, EndpointProbe,
    ModelEndpoint, ToolSpec,
};
use crate::agent::credentials::WipedText;
use crate::agent::{
    ContextWindow, ContextWindowSource, CredentialRef, CredentialResolver, ModelInfo,
    ProviderError, ProviderHealth, Secret, redact_with,
};

/// Largest part of an error body that is read at all.
const ERROR_BODY_READ_LIMIT: usize = 8 * 1024;
/// Largest `/models` answer accepted.
const CATALOG_LIMIT: usize = 4 * 1024 * 1024;

/// Static description of one endpoint instance. Holds a credential **reference**,
/// never a credential.
#[derive(Debug, Clone)]
pub struct OpenAiEndpointConfig {
    /// Identifier of the instance (also the `instance` given to the resolver).
    pub instance_id: String,
    /// Base URL up to and including the version segment, e.g. `https://api.deepseek.com/v1`.
    pub base_url: String,
    /// Where the API key lives.
    pub credential: CredentialRef,
    /// Dialect flags.
    pub quirks: EndpointQuirks,
    /// Accept private-network and CGNAT addresses (default `false`, see `guard.rs`).
    pub allow_private_network: bool,
    /// TCP/TLS connection timeout.
    pub connect_timeout: Duration,
    /// Wait for the response headers (and for non-streaming answers).
    pub response_timeout: Duration,
    /// Longest silence tolerated inside a stream.
    pub idle_timeout: Duration,
    /// How long a probe result stays valid.
    pub probe_ttl: Duration,
}

impl OpenAiEndpointConfig {
    /// A configuration with no credential, generic quirks and default timeouts.
    pub fn new(instance_id: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            base_url: base_url.into(),
            credential: CredentialRef::None,
            quirks: EndpointQuirks::generic(),
            allow_private_network: false,
            connect_timeout: Duration::from_secs(10),
            response_timeout: Duration::from_secs(120),
            idle_timeout: Duration::from_secs(120),
            probe_ttl: Duration::from_secs(3600),
        }
    }
}

type SharedField = Arc<Mutex<Option<&'static str>>>;

/// Locks, taking the data over when a panicking holder poisoned the mutex: these
/// caches hold plain values, a half-finished update is at worst a stale entry,
/// and a panic elsewhere must not take every later request down with it.
fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An OpenAI-compatible model endpoint.
pub struct OpenAiEndpoint {
    config: OpenAiEndpointConfig,
    credentials: Arc<dyn CredentialResolver>,
    guard: EndpointGuard,
    client: Mutex<Option<(String, Vec<SocketAddr>, Client)>>,
    probes: Mutex<HashMap<String, (Instant, EndpointProbe)>>,
}

impl std::fmt::Debug for OpenAiEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiEndpoint")
            .field("instance_id", &self.config.instance_id)
            .finish_non_exhaustive()
    }
}

impl OpenAiEndpoint {
    /// Creates the endpoint. Nothing is checked or sent yet: the URL is validated by
    /// the guard at the first request (`health()` is the connection test).
    pub fn new(config: OpenAiEndpointConfig, credentials: Arc<dyn CredentialResolver>) -> Self {
        let guard = EndpointGuard::new().allow_private_network(config.allow_private_network);
        Self {
            config,
            credentials,
            guard,
            client: Mutex::new(None),
            probes: Mutex::new(HashMap::new()),
        }
    }

    /// Uses another DNS resolver for the guard (tests, hosts with their own).
    pub fn with_dns_resolver(mut self, resolver: Arc<dyn DnsResolver>) -> Self {
        self.guard = self.guard.with_resolver(resolver);
        self
    }

    /// The configuration.
    pub fn config(&self) -> &OpenAiEndpointConfig {
        &self.config
    }

    async fn prepare(&self) -> Result<(CheckedEndpoint, Client), ProviderError> {
        let checked = self.guard.check(&self.config.base_url).await?;
        if let Some((host, addrs, client)) = locked(&self.client).as_ref()
            && *host == checked.host
            && *addrs == checked.addrs
        {
            return Ok((checked, client.clone()));
        }
        let mut builder = Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .connect_timeout(self.config.connect_timeout)
            .user_agent(concat!("nexus-claude/", env!("CARGO_PKG_VERSION")));
        if !checked.ip_literal {
            // Close the DNS-rebinding window: connect to what was validated.
            builder = builder.resolve_to_addrs(&checked.host, &checked.addrs);
        }
        let client = builder
            .build()
            .map_err(|_| ProviderError::unreachable("HTTP client could not be built"))?;
        *locked(&self.client) = Some((checked.host.clone(), checked.addrs.clone(), client.clone()));
        Ok((checked, client))
    }

    fn url(checked: &CheckedEndpoint, suffix: &str) -> String {
        let mut url = checked.url.clone();
        url.set_query(None);
        url.set_fragment(None);
        let path = format!("{}/{suffix}", url.path().trim_end_matches('/'));
        url.set_path(&path);
        url.to_string()
    }

    /// Resolves the credential for this request and attaches it, sensitive.
    async fn authorise(
        &self,
        builder: RequestBuilder,
    ) -> Result<(RequestBuilder, Option<Secret>), ProviderError> {
        let secret = self
            .credentials
            .resolve(&self.config.instance_id, &self.config.credential)
            .await?;
        let Some(secret) = secret else {
            return Ok((builder, None));
        };
        // The `Bearer <key>` text is wiped as soon as the header value is built.
        let bearer = WipedText::bearer(&secret);
        let mut value = HeaderValue::from_bytes(bearer.as_bytes())
            .map_err(|_| ProviderError::invalid("credential cannot be sent as an HTTP header"))?;
        value.set_sensitive(true);
        Ok((builder.header(AUTHORIZATION, value), Some(secret)))
    }

    async fn send(
        &self,
        builder: RequestBuilder,
        secret: Option<&Secret>,
    ) -> Result<Response, ProviderError> {
        let limit = self.config.response_timeout;
        let response = match tokio::time::timeout(limit, builder.send()).await {
            Err(_) => return Err(timeout_error(limit)),
            Ok(Err(error)) => return Err(map_reqwest(error, secret, limit)),
            Ok(Ok(response)) => response,
        };
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = read_body(response, ERROR_BODY_READ_LIMIT, limit)
            .await
            .unwrap_or_default();
        Err(classify_http(
            status.as_u16(),
            retry_after.as_deref(),
            &String::from_utf8_lossy(&body),
            secret,
        ))
    }

    async fn start(
        &self,
        request: CompletionRequest,
        forced_tool: Option<&str>,
    ) -> Result<(CompletionStream, SharedField), ProviderError> {
        let (checked, client) = self.prepare().await?;
        let body = wire::build_request(&request, &self.config.quirks, forced_tool);
        let payload = serde_json::to_vec(&body)
            .map_err(|_| ProviderError::invalid("request is not serialisable"))?;
        let builder = client
            .post(Self::url(&checked, "chat/completions"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .body(payload);
        let (builder, secret) = self.authorise(builder).await?;
        let response = self.send(builder, secret.as_ref()).await?;
        Ok(chunk_stream(
            response,
            secret,
            self.config.quirks.clone(),
            self.config.idle_timeout,
        ))
    }

    fn cached_probe(&self, model: &str) -> Option<EndpointProbe> {
        let probes = locked(&self.probes);
        let (at, probe) = probes.get(model)?;
        (at.elapsed() < self.config.probe_ttl).then(|| probe.clone())
    }
}

#[async_trait]
impl ModelEndpoint for OpenAiEndpoint {
    fn id(&self) -> &str {
        &self.config.instance_id
    }

    async fn health(&self) -> ProviderHealth {
        match self.models().await {
            Ok(models) => {
                let mut health = ProviderHealth::ok(None);
                health.detail = Some(format!("{} model(s) listed", models.len()));
                health
            },
            Err(error) => ProviderHealth::unavailable(error),
        }
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let (checked, client) = self.prepare().await?;
        let builder = client.get(Self::url(&checked, "models"));
        let (builder, secret) = self.authorise(builder).await?;
        let response = self.send(builder, secret.as_ref()).await?;
        let body = read_body(response, CATALOG_LIMIT, self.config.response_timeout)
            .await
            .map_err(|e| scrub(e, secret.as_ref()))?;
        parse_models(&body)
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionStream, ProviderError> {
        Ok(self.start(request, None).await?.0)
    }

    async fn probe(&self, model: &str) -> Result<EndpointProbe, ProviderError> {
        if let Some(hit) = self.cached_probe(model) {
            return Ok(hit);
        }
        let mut request =
            CompletionRequest::new(model, vec![ChatMessage::user("Call the ping tool now.")]);
        request.tools = vec![ToolSpec {
            name: PROBE_TOOL.into(),
            description: "Connectivity check: call it once, with no arguments.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }];
        // A reasoning model thinks before it calls: room for that, or the budget
        // ends in the reasoning and the probe sees "no tool call".
        request.max_tokens = Some(4096);
        request.temperature = Some(0.0);
        let no_tools = || ProviderError::ModelNoTools {
            model: model.to_string(),
        };
        // Forcing the tool is a convenience, not what is being asked: a 400 to the
        // FORCED probe is a refusal of the forcing as often as of tools (reasoning
        // models in thinking mode: DeepSeek, Anthropic, Qwen, Kimi..., each with its
        // own wording), so the model is asked again with `auto` before anything is
        // concluded. Only that answer says whether the model has tools.
        let forced =
            !self.config.quirks.omit_tool_choice && !self.config.quirks.no_forced_tool_choice;
        let first = self.start(request.clone(), Some(PROBE_TOOL)).await;
        let started = match first {
            Err(ProviderError::InvalidRequest { .. }) if forced => self.start(request, None).await,
            other => other,
        };
        let (mut stream, field) = started.map_err(|error| tools_refusal(error, &no_tools))?;
        let mut called = false;
        while let Some(item) = stream.next().await {
            match item.map_err(|error| tools_refusal(error, &no_tools))? {
                CompletionChunk::ToolCall(call) if call.name == PROBE_TOOL => called = true,
                _ => {},
            }
        }
        if !called {
            return Err(no_tools());
        }
        // The catalogue is best effort: a failure here does not fail the probe.
        let context_window = self.models().await.ok().and_then(|models| {
            models
                .into_iter()
                .find(|info| info.id == model)
                .and_then(|info| info.context_window)
                .map(|window| window.value)
        });
        let probe = EndpointProbe {
            tools: true,
            parallel_tools: self.config.quirks.explicit_parallel_tool_calls,
            reasoning_field: locked(&field).map(str::to_string),
            context_window,
            checked_at_ms: crate::agent::now_ms(),
        };
        locked(&self.probes).insert(model.to_string(), (Instant::now(), probe.clone()));
        Ok(probe)
    }
}

/// The refusal is about a PARAMETER that concerns tools, not about tools, however it
/// is spelled: `tool_choice`, `toolChoice` (Bedrock), "tool choice", and
/// `parallel_tool_calls` (sent by the nim and llama_server presets; OpenAI answers
/// "Unsupported parameter: 'parallel_tool_calls' is not supported with this model").
fn names_a_tool_parameter(detail: &str) -> bool {
    let squashed: String = detail
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    squashed.contains("toolchoice") || squashed.contains("paralleltoolcalls")
}

/// A 400 that says the model has no tool support is `ModelNoTools` for the probe.
/// One that only refuses a tool parameter (`tool_choice`, `parallel_tool_calls`) is
/// not: the model was never asked.
fn tools_refusal(error: ProviderError, no_tools: &dyn Fn() -> ProviderError) -> ProviderError {
    if let ProviderError::InvalidRequest { detail } = &error {
        let lower = detail.to_ascii_lowercase();
        if lower.contains("tool")
            && !names_a_tool_parameter(&lower)
            && (lower.contains("support") || lower.contains("not allowed"))
        {
            return no_tools();
        }
    }
    error
}

fn timeout_error(after: Duration) -> ProviderError {
    ProviderError::Timeout {
        after_ms: after.as_millis() as u64,
    }
}

fn map_reqwest(error: reqwest::Error, secret: Option<&Secret>, limit: Duration) -> ProviderError {
    if error.is_timeout() {
        return timeout_error(limit);
    }
    let connect = error.is_connect();
    let text = error.without_url().to_string();
    let detail = match secret {
        Some(secret) => redact_with(&text, &[secret]),
        None => redact_with(&text, &[]),
    };
    if connect {
        return ProviderError::EndpointUnreachable {
            detail: format!("connection failed: {detail}"),
        };
    }
    ProviderError::EndpointUnreachable { detail }
}

/// Re-masks the free-text part of an error with the request's secret.
fn scrub(error: ProviderError, secret: Option<&Secret>) -> ProviderError {
    let secrets: Vec<&Secret> = secret.into_iter().collect();
    let mask = |detail: String| redact_with(&detail, &secrets);
    match error {
        ProviderError::Protocol { detail } => ProviderError::Protocol {
            detail: mask(detail),
        },
        ProviderError::EndpointUnreachable { detail } => ProviderError::EndpointUnreachable {
            detail: mask(detail),
        },
        ProviderError::InvalidRequest { detail } => ProviderError::InvalidRequest {
            detail: mask(detail),
        },
        other => other,
    }
}

async fn read_body(
    mut response: Response,
    limit: usize,
    per_chunk: Duration,
) -> Result<Vec<u8>, ProviderError> {
    let mut body = Vec::new();
    loop {
        match tokio::time::timeout(per_chunk, response.chunk()).await {
            Err(_) => return Err(timeout_error(per_chunk)),
            Ok(Err(error)) => return Err(map_reqwest(error, None, per_chunk)),
            Ok(Ok(None)) => return Ok(body),
            Ok(Ok(Some(chunk))) => {
                body.extend_from_slice(&chunk);
                if body.len() > limit {
                    body.truncate(limit);
                    return Ok(body);
                }
            },
        }
    }
}

/// Seconds (integer or decimal) of a `Retry-After` header, in milliseconds. The
/// HTTP-date form is not interpreted (`None`).
fn parse_retry_after(value: &str) -> Option<u64> {
    let seconds: f64 = value.trim().parse().ok()?;
    (seconds.is_finite() && seconds >= 0.0).then_some((seconds * 1000.0) as u64)
}

/// Maps a failed HTTP answer to a [`ProviderError`]. The body is masked (with the
/// request's secret registered, then the generic rules) before being truncated to 512 bytes.
pub(crate) fn classify_http(
    status: u16,
    retry_after: Option<&str>,
    body: &str,
    secret: Option<&Secret>,
) -> ProviderError {
    let secrets: Vec<&Secret> = secret.into_iter().collect();
    let detail = || redact_with(&format!("HTTP {status}: {}", body.trim()), &secrets);
    match status {
        300..=399 => ProviderError::Protocol {
            detail: format!("redirect (HTTP {status}) refused: the endpoint must answer directly"),
        },
        401 | 403 => ProviderError::Unauthorized,
        429 => ProviderError::RateLimited {
            retry_after_ms: retry_after.and_then(parse_retry_after),
        },
        503 | 529 => ProviderError::Overloaded,
        404 => ProviderError::InvalidRequest {
            detail: format!("HTTP 404: model or endpoint not found ({})", detail()),
        },
        400 | 413 | 422 => match wire::classify_message(body) {
            Some(classified) => classified,
            None => ProviderError::InvalidRequest { detail: detail() },
        },
        500..=599 => match wire::classify_message(body) {
            Some(ProviderError::Overloaded) => ProviderError::Overloaded,
            _ => ProviderError::EndpointUnreachable { detail: detail() },
        },
        _ => ProviderError::InvalidRequest { detail: detail() },
    }
}

fn parse_models(body: &[u8]) -> Result<Vec<ModelInfo>, ProviderError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| ProviderError::protocol("the model catalogue is not valid JSON"))?;
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::protocol("the model catalogue has no `data` list"))?;
    let mut models = Vec::new();
    for entry in entries {
        let Some(id) = entry
            .get("id")
            .or_else(|| entry.get("name"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let mut info = ModelInfo::new(id);
        let window = [
            "context_length",
            "max_model_len",
            "context_window",
            "max_context_length",
        ]
        .iter()
        .find_map(|key| entry.get(*key).and_then(Value::as_u64))
        .or_else(|| {
            entry
                .get("meta")
                .and_then(|meta| meta.get("n_ctx_train"))
                .and_then(Value::as_u64)
        });
        info.context_window = window.map(|value| ContextWindow {
            value,
            source: ContextWindowSource::Catalog,
        });
        models.push(info);
    }
    Ok(models)
}

/// Turns a successful response into a stream of chunks. The secret lives as long as
/// the stream (to mask mid-stream errors) and is dropped (zeroed) with it.
fn chunk_stream(
    mut response: Response,
    secret: Option<Secret>,
    quirks: EndpointQuirks,
    idle: Duration,
) -> (CompletionStream, SharedField) {
    let field: SharedField = Arc::new(Mutex::new(None));
    let shared = Arc::clone(&field);
    let stream = async_stream::stream! {
        let mut decoder = SseDecoder::new();
        let mut parser = StreamParser::new(quirks);
        let mut finished = false;
        'read: loop {
            let bytes = match tokio::time::timeout(idle, response.chunk()).await {
                Err(_) => { yield Err(timeout_error(idle)); return; }
                Ok(Err(error)) => { yield Err(map_reqwest(error, secret.as_ref(), idle)); return; }
                Ok(Ok(bytes)) => bytes,
            };
            let events = match &bytes {
                Some(bytes) => decoder.push(bytes),
                None => Ok(decoder.finish().into_iter().collect()),
            };
            let events = match events {
                Ok(events) => events,
                Err(error) => { yield Err(scrub(error, secret.as_ref())); return; }
            };
            for event in events {
                match parser.feed(&event.data) {
                    Ok(chunks) => {
                        *locked(&shared) = parser.reasoning_field_seen;
                        for chunk in chunks { yield Ok(chunk); }
                    }
                    Err(error) => { yield Err(scrub(error, secret.as_ref())); return; }
                }
                if event.is_done() { finished = true; break 'read; }
            }
            if bytes.is_none() { break; }
        }
        let _ = finished;
        match parser.end() {
            Ok(chunks) => { for chunk in chunks { yield Ok(chunk); } }
            Err(error) => yield Err(scrub(error, secret.as_ref())),
        }
    };
    (Box::pin(stream), field)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poison<T: Send>(mutex: &Mutex<T>) {
        std::thread::scope(|scope| {
            let _ = scope
                .spawn(|| {
                    let _guard = mutex.lock().unwrap();
                    panic!("poisoning the mutex on purpose");
                })
                .join();
        });
        assert!(mutex.is_poisoned());
    }

    #[tokio::test]
    async fn a_poisoned_cache_does_not_take_the_endpoint_down() {
        let mut config = OpenAiEndpointConfig::new("poisoned", "http://127.0.0.1:9/v1");
        config.allow_private_network = true;
        let endpoint = OpenAiEndpoint::new(config, Arc::new(crate::agent::EnvCredentialResolver));
        poison(&endpoint.client);
        poison(&endpoint.probes);
        // Reading and filling both caches goes on, on the data the panic left.
        assert!(endpoint.cached_probe("m").is_none());
        let (_checked, _client) = endpoint.prepare().await.expect("prepare after poison");
        let (_checked, _client) = endpoint.prepare().await.expect("the cache is reused");
        let field: SharedField = Arc::new(Mutex::new(None));
        poison(&field);
        assert_eq!(locked(&field).map(str::to_string), None);
    }

    #[test]
    fn http_statuses_are_classified() {
        assert_eq!(
            classify_http(401, None, "", None),
            ProviderError::Unauthorized
        );
        assert_eq!(
            classify_http(403, None, "", None),
            ProviderError::Unauthorized
        );
        assert_eq!(
            classify_http(429, Some("7"), "", None),
            ProviderError::RateLimited {
                retry_after_ms: Some(7000)
            }
        );
        assert_eq!(
            classify_http(429, Some("1.5"), "", None),
            ProviderError::RateLimited {
                retry_after_ms: Some(1500)
            }
        );
        assert_eq!(
            classify_http(429, Some("Wed, 21 Oct 2026 07:28:00 GMT"), "", None),
            ProviderError::RateLimited {
                retry_after_ms: None
            }
        );
        assert_eq!(
            classify_http(503, None, "", None),
            ProviderError::Overloaded
        );
        assert_eq!(
            classify_http(529, None, "", None),
            ProviderError::Overloaded
        );
        assert_eq!(
            classify_http(500, None, "model is overloaded", None),
            ProviderError::Overloaded
        );
        assert!(matches!(
            classify_http(502, None, "bad gateway", None),
            ProviderError::EndpointUnreachable { .. }
        ));
        assert!(matches!(
            classify_http(
                400,
                None,
                r#"{"error":{"code":"context_length_exceeded"}}"#,
                None
            ),
            ProviderError::ContextTooSmall { .. }
        ));
        assert!(matches!(
            classify_http(404, None, "no such model", None),
            ProviderError::InvalidRequest { .. }
        ));
        assert!(matches!(
            classify_http(302, None, "", None),
            ProviderError::Protocol { .. }
        ));
        assert!(matches!(
            classify_http(418, None, "teapot", None),
            ProviderError::InvalidRequest { .. }
        ));
    }

    #[test]
    fn error_bodies_are_masked_then_truncated() {
        let secret = Secret::new("zz-registered-secret-value");
        let body = format!("bad key zz-registered-secret-value {}", "x".repeat(2000));
        let error = classify_http(400, None, &body, Some(&secret));
        let ProviderError::InvalidRequest { detail } = error else {
            panic!()
        };
        assert!(!detail.contains("zz-registered-secret-value"));
        assert!(detail.len() <= 512 + '…'.len_utf8());
        assert!(detail.starts_with("HTTP 400"));
    }

    #[test]
    fn a_secret_split_by_truncation_cannot_leak() {
        // The secret straddles the 512-byte mark: masking happens before truncation.
        let secret = Secret::new("tok_ABCDEFGHIJKLMNOP");
        let body = format!("{}tok_ABCDEFGHIJKLMNOP tail", "a".repeat(500));
        let ProviderError::InvalidRequest { detail } =
            classify_http(400, None, &body, Some(&secret))
        else {
            panic!()
        };
        assert!(!detail.contains("tok_ABC"), "{detail}");
    }

    #[test]
    fn catalogue_parsing_reads_context_windows() {
        let body = br#"{"data":[{"id":"a","max_model_len":8192},{"id":"b","meta":{"n_ctx_train":4096}},{"id":"c"},{"nope":1}]}"#;
        let models = parse_models(body).unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].context_window.unwrap().value, 8192);
        assert_eq!(models[1].context_window.unwrap().value, 4096);
        assert_eq!(models[2].context_window, None);
        assert!(parse_models(b"[]").is_err());
        assert!(parse_models(b"not json").is_err());
    }

    #[test]
    fn urls_are_joined_without_double_slashes_or_query() {
        let checked = CheckedEndpoint {
            url: reqwest::Url::parse("http://127.0.0.1:9/v1/?x=1#f").unwrap(),
            host: "127.0.0.1".into(),
            port: 9,
            addrs: vec!["127.0.0.1:9".parse().unwrap()],
            ip_literal: true,
        };
        assert_eq!(
            OpenAiEndpoint::url(&checked, "chat/completions"),
            "http://127.0.0.1:9/v1/chat/completions"
        );
    }
}
