//! `model::OpenAiEndpoint` against the `fake_openai` server: streaming, tool calls,
//! usage, HTTP errors, secret hygiene, redirects, the tool-call probe and its cache,
//! and the endpoint guard. No network beyond 127.0.0.1.

#[path = "support/fake_openai.rs"]
mod fake_openai;

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use fake_openai::FakeOpenAi;
use futures::StreamExt;
use nexus_claude::agent::{CredentialRef, CredentialResolver, HealthStatus, ProviderError, Secret};
use nexus_claude::model::{
    ChatMessage, CompletionChunk, CompletionRequest, DnsResolver, EndpointQuirks, FinishReason,
    ModelEndpoint, OpenAiEndpoint, OpenAiEndpointConfig, ToolCallChunk, ToolSpec,
};
use serde_json::{Value, json};

const KEY: &str = "tok_Zq81mLpWx39vNbR2";

/// Resolver that always answers `KEY` and counts how often it is asked.
struct CountingResolver(AtomicUsize);

#[async_trait]
impl CredentialResolver for CountingResolver {
    async fn resolve(
        &self,
        instance: &str,
        reference: &CredentialRef,
    ) -> Result<Option<Secret>, ProviderError> {
        assert_eq!(instance, "test");
        assert_eq!(reference, &CredentialRef::Vault("k".into()));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Some(Secret::new(KEY)))
    }
}

fn endpoint_with(
    url: String,
    quirks: EndpointQuirks,
    tweak: impl FnOnce(&mut OpenAiEndpointConfig),
) -> (OpenAiEndpoint, Arc<CountingResolver>) {
    let resolver = Arc::new(CountingResolver(AtomicUsize::new(0)));
    let mut config = OpenAiEndpointConfig::new("test", url);
    config.credential = CredentialRef::Vault("k".into());
    config.quirks = quirks;
    config.response_timeout = Duration::from_secs(10);
    config.idle_timeout = Duration::from_secs(10);
    tweak(&mut config);
    (OpenAiEndpoint::new(config, resolver.clone()), resolver)
}

fn endpoint(server: &FakeOpenAi, quirks: EndpointQuirks) -> OpenAiEndpoint {
    endpoint_with(server.base_url(), quirks, |_| {}).0
}

fn route_sse(events: &[Value]) -> Value {
    json!({"method": "POST", "path": "/v1/chat/completions", "status": 200, "sse": events})
}

fn delta(delta: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": delta}]})
}

fn finish(reason: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

fn hello() -> CompletionRequest {
    CompletionRequest::new("m", vec![ChatMessage::user("hi")])
}

async fn run(
    endpoint: &OpenAiEndpoint,
    request: CompletionRequest,
) -> Result<Vec<CompletionChunk>, ProviderError> {
    let mut stream = endpoint.complete(request).await?;
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item?);
    }
    Ok(chunks)
}

fn expect_err<T: std::fmt::Debug>(result: Result<T, ProviderError>) -> ProviderError {
    result.expect_err("an error was expected")
}

fn assert_no_key(error: &ProviderError) {
    let text = format!(
        "{error} {error:?} {}",
        serde_json::to_string(error).unwrap()
    );
    assert!(!text.contains(KEY), "credential leaked: {text}");
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_stream() {
    let server = FakeOpenAi::start(json!([route_sse(&[
        delta(json!({"role": "assistant", "content": ""})),
        delta(json!({"content": "Hel"})),
        delta(json!({"content": "lo"})),
        finish("stop"),
        json!("[DONE]"),
    ])]));
    let chunks = run(&endpoint(&server, EndpointQuirks::generic()), hello())
        .await
        .unwrap();
    assert_eq!(
        chunks,
        vec![
            CompletionChunk::Text("Hel".into()),
            CompletionChunk::Text("lo".into()),
            CompletionChunk::Finish(FinishReason::Stop),
        ]
    );
    let sent = &server.requests_to("POST", "/v1/chat/completions")[0];
    assert_eq!(sent["authorization_present"], true);
    assert_eq!(sent["body"]["stream"], true);
    assert_eq!(sent["body"]["stream_options"]["include_usage"], true);
    assert_eq!(sent["body"]["messages"][0]["content"], "hi");
    assert!(
        !server.raw_log().contains(KEY),
        "the log must never hold the key value"
    );
}

#[tokio::test]
async fn reasoning_stream_under_both_field_names() {
    for (quirks, field) in [
        (EndpointQuirks::deepseek(), "reasoning_content"),
        (EndpointQuirks::vllm(), "reasoning"),
    ] {
        let server = FakeOpenAi::start(json!([route_sse(&[
            delta(json!({field: "think "})),
            delta(json!({field: "hard"})),
            delta(json!({"content": "42"})),
            finish("stop"),
            json!("[DONE]"),
        ])]));
        let chunks = run(&endpoint(&server, quirks), hello()).await.unwrap();
        assert_eq!(
            chunks[0],
            CompletionChunk::Reasoning("think ".into()),
            "{field}"
        );
        assert_eq!(chunks[1], CompletionChunk::Reasoning("hard".into()));
        assert_eq!(chunks[2], CompletionChunk::Text("42".into()));
    }
}

#[tokio::test]
async fn fragmented_tool_call_is_assembled() {
    let server = FakeOpenAi::start(json!([route_sse(&[
        delta(
            json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "read", "arguments": ""}}]})
        ),
        delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"pa"}}]})),
        delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "th\":\"a.txt\"}"}}]})),
        finish("tool_calls"),
        json!("[DONE]"),
    ])]));
    let chunks = run(&endpoint(&server, EndpointQuirks::generic()), hello())
        .await
        .unwrap();
    assert_eq!(
        chunks,
        vec![
            CompletionChunk::ToolCall(ToolCallChunk {
                id: "call_1".into(),
                name: "read".into(),
                arguments: r#"{"path":"a.txt"}"#.into()
            }),
            CompletionChunk::Finish(FinishReason::ToolCalls),
        ]
    );
}

#[tokio::test]
async fn whole_tool_call_gives_the_same_chunk() {
    let server = FakeOpenAi::start(json!([route_sse(&[
        delta(
            json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a.txt\"}"}}]})
        ),
        finish("tool_calls"),
        json!("[DONE]"),
    ])]));
    let chunks = run(&endpoint(&server, EndpointQuirks::generic()), hello())
        .await
        .unwrap();
    assert_eq!(
        chunks[0],
        CompletionChunk::ToolCall(ToolCallChunk {
            id: "call_1".into(),
            name: "read".into(),
            arguments: r#"{"path":"a.txt"}"#.into()
        })
    );
}

#[tokio::test]
async fn usage_is_reported_before_finish() {
    let server = FakeOpenAi::start(json!([route_sse(&[
        delta(json!({"content": "x"})),
        finish("stop"),
        json!({"choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 7, "total_tokens": 27,
            "prompt_tokens_details": {"cached_tokens": 8}, "completion_tokens_details": {"reasoning_tokens": 3}}}),
        json!("[DONE]"),
    ])]));
    let chunks = run(&endpoint(&server, EndpointQuirks::generic()), hello())
        .await
        .unwrap();
    let CompletionChunk::Usage(usage) = &chunks[1] else {
        panic!("{chunks:?}")
    };
    assert_eq!(usage.input_tokens, Some(12));
    assert_eq!(usage.cache_read_tokens, Some(8));
    assert_eq!(usage.output_tokens, Some(7));
    assert_eq!(usage.reasoning_tokens, Some(3));
    assert_eq!(
        chunks.last(),
        Some(&CompletionChunk::Finish(FinishReason::Stop))
    );
}

#[tokio::test]
async fn truncated_stream_is_a_protocol_error() {
    let server = FakeOpenAi::start(json!([{
        "method": "POST", "path": "/v1/chat/completions", "status": 200,
        "sse": [delta(json!({"content": "partial"})), delta(json!({"content": "more"}))], "close_after": 1
    }]));
    let error = expect_err(run(&endpoint(&server, EndpointQuirks::generic()), hello()).await);
    assert!(
        matches!(
            error,
            ProviderError::Protocol { .. } | ProviderError::EndpointUnreachable { .. }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn silent_stream_times_out() {
    let server = FakeOpenAi::start(json!([{
        "method": "POST", "path": "/v1/chat/completions", "status": 200,
        "sse": [delta(json!({"content": "a"}))], "event_delay_ms": 1500
    }]));
    let (endpoint, _) = endpoint_with(server.base_url(), EndpointQuirks::generic(), |c| {
        c.idle_timeout = Duration::from_millis(300)
    });
    let error = expect_err(run(&endpoint, hello()).await);
    assert!(matches!(error, ProviderError::Timeout { .. }), "{error:?}");
    assert!(error.retryable());
}

// ---------------------------------------------------------------------------
// HTTP errors and secret hygiene
// ---------------------------------------------------------------------------

async fn error_for(route: Value) -> ProviderError {
    let server = FakeOpenAi::start(json!([route]));
    expect_err(run(&endpoint(&server, EndpointQuirks::generic()), hello()).await)
}

fn post(status: u16, extra: Value) -> Value {
    let mut route = json!({"method": "POST", "path": "/v1/chat/completions", "status": status});
    route
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    route
}

#[tokio::test]
async fn http_401_is_unauthorized() {
    let error = error_for(post(
        401,
        json!({"body": {"error": {"message": "bad key"}}}),
    ))
    .await;
    assert_eq!(error, ProviderError::Unauthorized);
    assert_no_key(&error);
}

#[tokio::test]
async fn http_429_carries_retry_after() {
    let error = error_for(post(
        429,
        json!({"headers": {"Retry-After": "3"}, "body": "slow down"}),
    ))
    .await;
    assert_eq!(
        error,
        ProviderError::RateLimited {
            retry_after_ms: Some(3000)
        }
    );
    assert!(error.retryable());
}

#[tokio::test]
async fn http_503_is_overloaded() {
    assert_eq!(
        error_for(post(503, json!({"body": "busy"}))).await,
        ProviderError::Overloaded
    );
}

#[tokio::test]
async fn context_length_400_is_context_too_small() {
    let error = error_for(post(400, json!({"body": {"error": {"code": "context_length_exceeded", "message": "maximum context length is 8192 tokens"}}}))).await;
    assert!(matches!(error, ProviderError::ContextTooSmall { .. }));
}

#[tokio::test]
async fn connection_refused_is_unreachable() {
    // Bind then drop a listener to get a port nobody listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (endpoint, _) = endpoint_with(
        format!("http://127.0.0.1:{port}/v1"),
        EndpointQuirks::generic(),
        |_| {},
    );
    let error = expect_err(run(&endpoint, hello()).await);
    assert!(
        matches!(error, ProviderError::EndpointUnreachable { .. }),
        "{error:?}"
    );
    assert!(error.retryable());
    assert_no_key(&error);
}

#[tokio::test]
async fn error_body_echoing_the_key_is_redacted_and_truncated() {
    let long_tail = "y".repeat(3000);
    let body = format!("Invalid API key {KEY} for this account. {long_tail}");
    let error = error_for(post(400, json!({"body": body}))).await;
    assert_no_key(&error);
    let ProviderError::InvalidRequest { detail } = &error else {
        panic!("{error:?}")
    };
    assert!(detail.len() <= 520, "detail is {} bytes", detail.len());
    // also when the server answers in JSON, chunked
    let error = error_for(post(
        400,
        json!({"body": {"error": {"message": format!("key {KEY} rejected")}}, "chunked": true}),
    ))
    .await;
    assert_no_key(&error);
    // and for a 5xx
    let error = error_for(post(
        500,
        json!({"body": format!("crash while using {KEY}")}),
    ))
    .await;
    assert_no_key(&error);
}

#[tokio::test]
async fn error_object_inside_the_stream_is_redacted() {
    let server = FakeOpenAi::start(json!([route_sse(&[
        json!({"error": {"message": format!("upstream refused {KEY}")}})
    ])]));
    let error = expect_err(run(&endpoint(&server, EndpointQuirks::generic()), hello()).await);
    assert!(matches!(error, ProviderError::Protocol { .. }));
    assert_no_key(&error);
}

#[tokio::test]
async fn redirect_is_not_followed() {
    let server = FakeOpenAi::start(json!([
        post(302, json!({"headers": {"Location": "/v1/elsewhere"}, "body": ""})),
        {"method": "POST", "path": "/v1/elsewhere", "status": 200, "sse": [finish("stop"), "[DONE]"]},
    ]));
    let error = expect_err(run(&endpoint(&server, EndpointQuirks::generic()), hello()).await);
    assert!(matches!(error, ProviderError::Protocol { .. }), "{error:?}");
    assert_eq!(
        server.requests().len(),
        1,
        "no second request: the key would have followed the redirect"
    );
    assert!(server.requests_to("POST", "/v1/elsewhere").is_empty());
}

#[tokio::test]
async fn credential_is_resolved_on_every_request_and_never_shown() {
    let server = FakeOpenAi::start(json!([route_sse(&[finish("stop"), json!("[DONE]")])]));
    let (endpoint, resolver) = endpoint_with(server.base_url(), EndpointQuirks::generic(), |_| {});
    run(&endpoint, hello()).await.unwrap();
    run(&endpoint, hello()).await.unwrap();
    assert_eq!(resolver.0.load(Ordering::SeqCst), 2);
    assert!(!format!("{endpoint:?} {:?}", endpoint.config()).contains(KEY));
    assert!(
        server
            .requests()
            .iter()
            .all(|r| r["authorization_present"] == true)
    );
}

#[tokio::test]
async fn locked_vault_is_not_papered_over() {
    struct Locked;
    #[async_trait]
    impl CredentialResolver for Locked {
        async fn resolve(
            &self,
            _: &str,
            _: &CredentialRef,
        ) -> Result<Option<Secret>, ProviderError> {
            Err(ProviderError::CredentialsLocked)
        }
    }
    let server = FakeOpenAi::start(json!([]));
    let mut config = OpenAiEndpointConfig::new("test", server.base_url());
    config.credential = CredentialRef::Vault("k".into());
    let endpoint = OpenAiEndpoint::new(config, Arc::new(Locked));
    assert_eq!(
        expect_err(run(&endpoint, hello()).await),
        ProviderError::CredentialsLocked
    );
    assert!(
        server.requests().is_empty(),
        "no unauthenticated request when the vault is locked"
    );
}

// ---------------------------------------------------------------------------
// Quirks end to end
// ---------------------------------------------------------------------------

fn tool_transcript() -> CompletionRequest {
    let mut request = CompletionRequest::new(
        "m",
        vec![
            ChatMessage::user("read it"),
            ChatMessage::assistant_tool_calls(vec![ToolCallChunk {
                id: "c1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }])
            .with_reasoning("need the file"),
            ChatMessage::tool("c1", "data"),
        ],
    );
    request.tools = vec![ToolSpec {
        name: "read".into(),
        description: "".into(),
        parameters: json!({"type": "object"}),
    }];
    request
}

#[tokio::test]
async fn deepseek_reasoning_is_sent_back_only_with_tools() {
    let server = FakeOpenAi::start(json!([route_sse(&[finish("stop"), json!("[DONE]")])]));
    let endpoint = endpoint(&server, EndpointQuirks::deepseek());
    run(&endpoint, tool_transcript()).await.unwrap();
    let mut without_tools = tool_transcript();
    without_tools.tools.clear();
    run(&endpoint, without_tools).await.unwrap();
    let sent = server.requests_to("POST", "/v1/chat/completions");
    assert_eq!(
        sent[0]["body"]["messages"][1]["reasoning_content"],
        "need the file"
    );
    assert!(
        sent[1]["body"]["messages"][1]
            .get("reasoning_content")
            .is_none()
    );
    assert!(sent[1]["body"].get("tools").is_none());
}

#[tokio::test]
async fn ollama_preset_sends_no_tool_choice() {
    let server = FakeOpenAi::start(json!([route_sse(&[finish("stop"), json!("[DONE]")])]));
    run(
        &endpoint(&server, EndpointQuirks::ollama()),
        tool_transcript(),
    )
    .await
    .unwrap();
    assert!(server.requests()[0]["body"].get("tool_choice").is_none());
}

// ---------------------------------------------------------------------------
// Catalogue, health, probe
// ---------------------------------------------------------------------------

fn models_route() -> Value {
    json!({"method": "GET", "path": "/v1/models", "status": 200,
        "body": {"object": "list", "data": [{"id": "m", "max_model_len": 32768}, {"id": "other"}]}})
}

#[tokio::test]
async fn models_and_health() {
    let server = FakeOpenAi::start(json!([models_route()]));
    let endpoint = endpoint(&server, EndpointQuirks::generic());
    let models = endpoint.models().await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].context_window.unwrap().value, 32768);
    assert_eq!(endpoint.health().await.status, HealthStatus::Ok);
    assert_eq!(endpoint.id(), "test");
}

#[tokio::test]
async fn health_failure_is_a_value() {
    let server = FakeOpenAi::start(
        json!([{"method": "GET", "path": "/v1/models", "status": 401, "body": "no"}]),
    );
    let health = endpoint(&server, EndpointQuirks::generic()).health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    assert_eq!(health.error, Some(ProviderError::Unauthorized));
}

fn probe_sse(call: bool, reasoning_field: Option<&str>) -> Value {
    let mut events = Vec::new();
    if let Some(field) = reasoning_field {
        events.push(delta(json!({field: "hmm"})));
    }
    if call {
        events.push(delta(json!({"tool_calls": [{"index": 0, "id": "p1", "function": {"name": "ping", "arguments": "{}"}}]})));
        events.push(finish("tool_calls"));
    } else {
        events.push(delta(json!({"content": "I cannot call tools"})));
        events.push(finish("stop"));
    }
    events.push(json!("[DONE]"));
    route_sse(&events)
}

#[tokio::test]
async fn probe_ok_reports_tools_reasoning_field_and_context() {
    let server = FakeOpenAi::start(json!([probe_sse(true, Some("reasoning")), models_route()]));
    let probe = endpoint(&server, EndpointQuirks::vllm())
        .probe("m")
        .await
        .unwrap();
    assert!(probe.tools);
    assert_eq!(probe.reasoning_field.as_deref(), Some("reasoning"));
    assert_eq!(probe.context_window, Some(32768));
    assert!(probe.checked_at_ms > 1_700_000_000_000);
    let sent = &server.requests_to("POST", "/v1/chat/completions")[0]["body"];
    assert_eq!(sent["tools"][0]["function"]["name"], "ping");
    assert_eq!(sent["tool_choice"]["function"]["name"], "ping");
}

#[tokio::test]
async fn probe_without_tool_call_is_model_no_tools() {
    let server = FakeOpenAi::start(json!([probe_sse(false, None)]));
    let error = expect_err(
        endpoint(&server, EndpointQuirks::generic())
            .probe("m")
            .await,
    );
    assert_eq!(error, ProviderError::ModelNoTools { model: "m".into() });
}

#[tokio::test]
async fn probe_refused_by_the_server_with_a_tools_message_is_model_no_tools() {
    let server = FakeOpenAi::start(json!([post(
        400,
        json!({"body": {"error": {"message": "registry.ollama.ai/library/x does not support tools"}}})
    )]));
    let error = expect_err(endpoint(&server, EndpointQuirks::ollama()).probe("x").await);
    assert_eq!(error, ProviderError::ModelNoTools { model: "x".into() });
}

#[tokio::test]
async fn probe_is_cached_per_model_and_expires() {
    let server = FakeOpenAi::start(json!([probe_sse(true, None), models_route()]));
    let (endpoint, _) = endpoint_with(server.base_url(), EndpointQuirks::generic(), |c| {
        c.probe_ttl = Duration::from_millis(600)
    });
    let first = endpoint.probe("m").await.unwrap();
    let second = endpoint.probe("m").await.unwrap();
    assert_eq!(first, second);
    assert_eq!(
        server.requests_to("POST", "/v1/chat/completions").len(),
        1,
        "second probe must come from the cache"
    );
    endpoint.probe("another").await.unwrap();
    assert_eq!(
        server.requests_to("POST", "/v1/chat/completions").len(),
        2,
        "cache is per model"
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    endpoint.probe("m").await.unwrap();
    assert_eq!(
        server.requests_to("POST", "/v1/chat/completions").len(),
        3,
        "expired entry is probed again"
    );
}

#[tokio::test]
async fn failed_probe_is_not_cached() {
    let server = FakeOpenAi::start(json!([
        probe_sse(false, None),
        probe_sse(true, None),
        models_route()
    ]));
    let endpoint = endpoint(&server, EndpointQuirks::generic());
    assert!(endpoint.probe("m").await.is_err());
    assert!(endpoint.probe("m").await.is_ok());
}

#[tokio::test]
async fn probe_only_talks_to_the_instance_url() {
    let server = FakeOpenAi::start(json!([probe_sse(true, None), models_route()]));
    endpoint(&server, EndpointQuirks::generic())
        .probe("m")
        .await
        .unwrap();
    for request in server.requests() {
        assert!(request["path"].as_str().unwrap().starts_with("/v1/"));
    }
}

// ---------------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------------

struct MapResolver(IpAddr, AtomicUsize);

#[async_trait]
impl DnsResolver for MapResolver {
    async fn resolve(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(vec![SocketAddr::new(self.0, port)])
    }
}

#[tokio::test]
async fn plain_http_outside_loopback_is_refused_before_any_request() {
    let (endpoint, resolver) = endpoint_with(
        "http://models.example.com/v1".into(),
        EndpointQuirks::generic(),
        |_| {},
    );
    let error = expect_err(run(&endpoint, hello()).await);
    assert!(
        matches!(error, ProviderError::InvalidRequest { .. }),
        "{error:?}"
    );
    assert_eq!(
        resolver.0.load(Ordering::SeqCst),
        0,
        "the credential must not even be resolved"
    );
    assert_no_key(&error);
}

#[tokio::test]
async fn private_range_is_refused_after_dns_resolution() {
    let (endpoint, resolver) = endpoint_with(
        "https://models.example.com/v1".into(),
        EndpointQuirks::generic(),
        |_| {},
    );
    let dns = Arc::new(MapResolver(
        "10.1.2.3".parse().unwrap(),
        AtomicUsize::new(0),
    ));
    let endpoint = endpoint.with_dns_resolver(dns.clone());
    let error = expect_err(endpoint.models().await);
    assert!(
        matches!(error, ProviderError::InvalidRequest { .. }),
        "{error:?}"
    );
    assert_eq!(dns.1.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.0.load(Ordering::SeqCst), 0);
    let health = endpoint.health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    // cloud metadata stays refused even with the private-network switch
    let (lan, _) = endpoint_with(
        "https://models.example.com/v1".into(),
        EndpointQuirks::generic(),
        |c| c.allow_private_network = true,
    );
    let lan = lan.with_dns_resolver(Arc::new(MapResolver(
        "169.254.169.254".parse().unwrap(),
        AtomicUsize::new(0),
    )));
    assert!(matches!(
        expect_err(lan.models().await),
        ProviderError::InvalidRequest { .. }
    ));
}

/// Answers each call with the next address of its list (the last one repeats).
struct SequenceResolver(Vec<IpAddr>, AtomicUsize);

#[async_trait]
impl DnsResolver for SequenceResolver {
    async fn resolve(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let call = self.1.fetch_add(1, Ordering::SeqCst);
        let ip = self.0[call.min(self.0.len() - 1)];
        Ok(vec![SocketAddr::new(ip, port)])
    }
}

/// A listener that only counts TCP connections: the TLS handshake that follows
/// an `https` request to it fails, which is irrelevant here.
async fn counting_listener() -> (u16, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });
    (port, accepted)
}

#[tokio::test]
async fn connection_is_pinned_on_the_validated_address() {
    // `pin-test.invalid` is reserved (RFC 6761) and never resolves through the
    // system: only the address the guard validated can make the TCP connection.
    let (port, accepted) = counting_listener().await;
    let (endpoint, _) = endpoint_with(
        format!("https://pin-test.invalid:{port}/v1"),
        EndpointQuirks::generic(),
        |_| {},
    );
    let dns = Arc::new(SequenceResolver(
        vec!["127.0.0.1".parse().unwrap()],
        AtomicUsize::new(0),
    ));
    let endpoint = endpoint.with_dns_resolver(dns.clone());
    // The handshake fails (the listener is not TLS); the connection was made.
    let _ = endpoint.models().await;
    assert_eq!(dns.1.load(Ordering::SeqCst), 1);
    assert!(
        accepted.load(Ordering::SeqCst) >= 1,
        "the client must connect to the validated address, not re-resolve the name"
    );
}

#[tokio::test]
async fn a_rebound_name_is_checked_again_and_refused() {
    // First answer: an allowed address. Second answer for the same name: an
    // internal one. The second request must be refused, and must not reuse the
    // client pinned on the first answer.
    let (port, accepted) = counting_listener().await;
    let (endpoint, _) = endpoint_with(
        format!("https://rebind.invalid:{port}/v1"),
        EndpointQuirks::generic(),
        |_| {},
    );
    let dns = Arc::new(SequenceResolver(
        vec!["127.0.0.1".parse().unwrap(), "10.9.8.7".parse().unwrap()],
        AtomicUsize::new(0),
    ));
    let endpoint = endpoint.with_dns_resolver(dns.clone());
    let _ = endpoint.models().await;
    let after_first = accepted.load(Ordering::SeqCst);
    assert!(after_first >= 1);
    let error = expect_err(endpoint.models().await);
    assert!(
        matches!(error, ProviderError::InvalidRequest { .. }),
        "{error:?}"
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        after_first,
        "no connection after the rebinding"
    );
    assert_eq!(dns.1.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn credentials_in_the_url_are_refused_and_not_echoed() {
    let (endpoint, _) = endpoint_with(
        "http://alice:hunter2@127.0.0.1:1/v1".into(),
        EndpointQuirks::generic(),
        |_| {},
    );
    let error = expect_err(run(&endpoint, hello()).await);
    let text = format!("{error} {error:?}");
    assert!(matches!(error, ProviderError::InvalidRequest { .. }));
    assert!(!text.contains("hunter2") && !text.contains("alice"));
}

#[tokio::test]
async fn localhost_name_works_with_the_system_resolver() {
    let server = FakeOpenAi::start(json!([models_route()]));
    let (endpoint, _) = endpoint_with(
        format!("http://localhost:{}/v1", server.port()),
        EndpointQuirks::generic(),
        |_| {},
    );
    assert_eq!(endpoint.models().await.unwrap().len(), 2);
}
