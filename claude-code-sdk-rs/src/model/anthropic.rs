//! [`AnthropicEndpoint`]: the Anthropic Messages API over HTTP + SSE, for the native
//! harness (CTX5): Anthropic's own API, or a gateway that speaks the same protocol.
//!
//! The wire itself (request JSON, the three `cache_control` points, the stream) is
//! [`super::anthropic_wire`]. This file is the HTTP side, with the security properties
//! of [`super::OpenAiEndpoint`] (decision A36): every request goes through the
//! [`EndpointGuard`] and is pinned on the validated addresses, no proxy, no redirect;
//! the credential is resolved for **each** request, sent as a sensitive header, and
//! masked out of every error built from the server's answer.
//!
//! | | |
//! |---|---|
//! | messages | `POST {base}/v1/messages`, `anthropic-version: 2023-06-01`, streamed |
//! | catalogue | `GET {base}/v1/models` (paginated): `max_input_tokens`, `capabilities.image_input`, `capabilities.thinking` |
//! | key | `x-api-key` ([`AnthropicAuth::ApiKey`]) or `Authorization: Bearer` ([`AnthropicAuth::Bearer`], gateways) |
//!
//! A base URL that already ends in `/v1` is not given a second one.
//!
//! The probe sends nothing to the model: Claude models on the Messages API take tools
//! and images, and think; what the catalogue says of a model overrides that (its
//! window, `image_input`, `thinking`), and a catalogue that cannot be read leaves the
//! window unknown.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use reqwest::{Client, RequestBuilder, Response};
use serde_json::Value;

use super::anthropic_wire::{self, StreamParser, WireOptions};
use super::guard::{CheckedEndpoint, DnsResolver, EndpointGuard};
use super::openai::{
    locked, map_reqwest, pinned_client, read_body, scrub, send_request, timeout_error,
};
use super::sse::SseDecoder;
use super::{CompletionRequest, CompletionStream, EndpointProbe, ModelEndpoint};
use crate::agent::credentials::WipedText;
use crate::agent::{
    ContextWindow, ContextWindowSource, CredentialRef, CredentialResolver, ModelInfo,
    ProviderError, ProviderHealth, Secret,
};

/// Anthropic's own API, when no endpoint is configured.
pub const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";

/// The `anthropic-version` header every request carries.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Largest `/v1/models` page accepted.
const CATALOG_LIMIT: usize = 4 * 1024 * 1024;
/// Pages of the catalogue read at most.
const CATALOG_PAGES: usize = 10;

/// How the key is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicAuth {
    /// `x-api-key: <key>`: Anthropic's own API.
    ApiKey,
    /// `Authorization: Bearer <key>`: what most gateways want.
    Bearer,
}

/// Static description of one endpoint instance. Holds a credential **reference**,
/// never a credential.
#[derive(Debug, Clone)]
pub struct AnthropicEndpointConfig {
    /// Identifier of the instance (the `instance` given to the resolver: the model
    /// provider's id for a composed native provider).
    pub instance_id: String,
    /// Base URL, [`ANTHROPIC_API_BASE`] by default; `/v1` is added unless present.
    pub base_url: String,
    /// Where the key lives.
    pub credential: CredentialRef,
    /// How the key is sent.
    pub auth: AnthropicAuth,
    /// Accept private-network and CGNAT addresses (default `false`, see `guard.rs`).
    pub allow_private_network: bool,
    /// The `thinking` object sent with every request (`{"type": "adaptive"}`…);
    /// `None` sends none: the model's default applies.
    pub thinking: Option<Value>,
    /// Put the three `cache_control` points (tools, system, last turn). Default `true`.
    pub prompt_cache: bool,
    /// `max_tokens` of a request that names none.
    pub default_max_tokens: u32,
    /// TCP/TLS connection timeout.
    pub connect_timeout: Duration,
    /// Wait for the response headers (and for non-streaming answers).
    pub response_timeout: Duration,
    /// Longest silence tolerated inside a stream.
    pub idle_timeout: Duration,
    /// How long a probe result stays valid.
    pub probe_ttl: Duration,
}

impl AnthropicEndpointConfig {
    /// Anthropic's own API (`x-api-key`), no credential, default timeouts.
    pub fn new(instance_id: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            base_url: ANTHROPIC_API_BASE.to_owned(),
            credential: CredentialRef::None,
            auth: AnthropicAuth::ApiKey,
            allow_private_network: false,
            thinking: None,
            prompt_cache: true,
            default_max_tokens: anthropic_wire::DEFAULT_MAX_TOKENS,
            connect_timeout: Duration::from_secs(10),
            response_timeout: Duration::from_secs(120),
            idle_timeout: Duration::from_secs(120),
            probe_ttl: Duration::from_secs(3600),
        }
    }

    /// A gateway at `base_url` (`Authorization: Bearer`).
    pub fn gateway(instance_id: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            auth: AnthropicAuth::Bearer,
            ..Self::new(instance_id)
        }
    }

    fn wire_options(&self) -> WireOptions {
        WireOptions {
            thinking: self.thinking.clone(),
            prompt_cache: self.prompt_cache,
            default_max_tokens: self.default_max_tokens,
        }
    }
}

/// What the catalogue says of one model.
#[derive(Debug, Clone)]
struct Listed {
    info: ModelInfo,
    thinking: Option<bool>,
}

/// An Anthropic Messages API endpoint.
pub struct AnthropicEndpoint {
    config: AnthropicEndpointConfig,
    credentials: Arc<dyn CredentialResolver>,
    guard: EndpointGuard,
    client: Mutex<Option<(String, Vec<SocketAddr>, Client)>>,
    probes: Mutex<HashMap<String, (Instant, EndpointProbe)>>,
}

impl std::fmt::Debug for AnthropicEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicEndpoint")
            .field("instance_id", &self.config.instance_id)
            .finish_non_exhaustive()
    }
}

impl AnthropicEndpoint {
    /// Creates the endpoint. Nothing is checked or sent yet.
    pub fn new(config: AnthropicEndpointConfig, credentials: Arc<dyn CredentialResolver>) -> Self {
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
    pub fn config(&self) -> &AnthropicEndpointConfig {
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
        let client = pinned_client(&checked, self.config.connect_timeout)?;
        *locked(&self.client) = Some((checked.host.clone(), checked.addrs.clone(), client.clone()));
        Ok((checked, client))
    }

    /// `{base}/v1/{suffix}`, without a second `/v1` when the base has one.
    fn url(checked: &CheckedEndpoint, suffix: &str) -> String {
        let mut url = checked.url.clone();
        url.set_query(None);
        url.set_fragment(None);
        let base = url.path().trim_end_matches('/');
        let path = if base.ends_with("/v1") {
            format!("{base}/{suffix}")
        } else {
            format!("{base}/v1/{suffix}")
        };
        url.set_path(&path);
        url.to_string()
    }

    /// The version header, and the credential for this request, sensitive.
    async fn authorise(
        &self,
        builder: RequestBuilder,
    ) -> Result<(RequestBuilder, Option<Secret>), ProviderError> {
        let builder = builder.header("anthropic-version", ANTHROPIC_VERSION);
        let secret = self
            .credentials
            .resolve(&self.config.instance_id, &self.config.credential)
            .await?;
        let Some(secret) = secret else {
            return Ok((builder, None));
        };
        let unusable = || ProviderError::invalid("credential cannot be sent as an HTTP header");
        let (name, mut value) = match self.config.auth {
            AnthropicAuth::ApiKey => (
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_bytes(secret.expose().as_bytes()).map_err(|_| unusable())?,
            ),
            AnthropicAuth::Bearer => {
                // The `Bearer <key>` text is wiped as soon as the header value is built.
                let bearer = WipedText::bearer(&secret);
                (
                    AUTHORIZATION,
                    HeaderValue::from_bytes(bearer.as_bytes()).map_err(|_| unusable())?,
                )
            },
        };
        value.set_sensitive(true);
        Ok((builder.header(name, value), Some(secret)))
    }

    async fn send(
        &self,
        builder: RequestBuilder,
        secret: Option<&Secret>,
    ) -> Result<Response, ProviderError> {
        send_request(builder, secret, self.config.response_timeout).await
    }

    /// Every model of the catalogue, page after page (at most [`CATALOG_PAGES`]).
    async fn listed(&self) -> Result<Vec<Listed>, ProviderError> {
        let (checked, client) = self.prepare().await?;
        let mut models = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..CATALOG_PAGES {
            let mut builder = client
                .get(Self::url(&checked, "models"))
                .query(&[("limit", "1000")]);
            if let Some(after) = &after {
                builder = builder.query(&[("after_id", after.as_str())]);
            }
            let (builder, secret) = self.authorise(builder).await?;
            let response = self.send(builder, secret.as_ref()).await?;
            let body = read_body(response, CATALOG_LIMIT, self.config.response_timeout)
                .await
                .map_err(|e| scrub(e, secret.as_ref()))?;
            let (page, next) = parse_models(&body)?;
            models.extend(page);
            match next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        Ok(models)
    }

    fn cached_probe(&self, model: &str) -> Option<EndpointProbe> {
        let probes = locked(&self.probes);
        let (at, probe) = probes.get(model)?;
        (at.elapsed() < self.config.probe_ttl).then(|| probe.clone())
    }
}

/// One page of `/v1/models`, and the id to continue after when there is more.
fn parse_models(body: &[u8]) -> Result<(Vec<Listed>, Option<String>), ProviderError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| ProviderError::protocol("the model catalogue is not valid JSON"))?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::protocol("the model catalogue has no `data` list"))?;
    let supported = |entry: &Value, capability: &str| {
        entry
            .get("capabilities")
            .and_then(|caps| caps.get(capability))
            .and_then(|cap| cap.get("supported"))
            .and_then(Value::as_bool)
    };
    let models: Vec<Listed> = entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?;
            let mut info = ModelInfo::new(id);
            info.context_window =
                entry
                    .get("max_input_tokens")
                    .and_then(Value::as_u64)
                    .map(|value| ContextWindow {
                        value,
                        source: ContextWindowSource::Catalog,
                    });
            info.supports_images = supported(entry, "image_input");
            Some(Listed {
                info,
                thinking: supported(entry, "thinking"),
            })
        })
        .collect();
    let more = value.get("has_more").and_then(Value::as_bool) == Some(true);
    let next = more
        .then(|| {
            value
                .get("last_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .flatten();
    Ok((models, next))
}

#[async_trait]
impl ModelEndpoint for AnthropicEndpoint {
    fn id(&self) -> &str {
        &self.config.instance_id
    }

    fn wire_messages(&self, request: &CompletionRequest) -> Option<Vec<Value>> {
        Some(anthropic_wire::wire_messages(request))
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
        Ok(self
            .listed()
            .await?
            .into_iter()
            .map(|listed| listed.info)
            .collect())
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionStream, ProviderError> {
        let (checked, client) = self.prepare().await?;
        let body = anthropic_wire::build_request(&request, &self.config.wire_options());
        let payload = serde_json::to_vec(&body)
            .map_err(|_| ProviderError::invalid("request is not serialisable"))?;
        let builder = client
            .post(Self::url(&checked, "messages"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .body(payload);
        let (builder, secret) = self.authorise(builder).await?;
        let response = self.send(builder, secret.as_ref()).await?;
        Ok(chunk_stream(response, secret, self.config.idle_timeout))
    }

    async fn probe(&self, model: &str) -> Result<EndpointProbe, ProviderError> {
        if let Some(hit) = self.cached_probe(model) {
            return Ok(hit);
        }
        // The catalogue is best effort: a gateway without one still serves the model.
        let listed = self
            .listed()
            .await
            .ok()
            .and_then(|models| models.into_iter().find(|listed| listed.info.id == model));
        let probe = EndpointProbe {
            tools: true,
            parallel_tools: Some(true),
            reasoning_field: (listed.as_ref().and_then(|l| l.thinking) != Some(false))
                .then(|| "thinking".to_owned()),
            context_window: listed
                .as_ref()
                .and_then(|l| l.info.context_window)
                .map(|window| window.value),
            images: Some(
                listed
                    .as_ref()
                    .and_then(|l| l.info.supports_images)
                    .unwrap_or(true),
            ),
            checked_at_ms: crate::agent::now_ms(),
        };
        locked(&self.probes).insert(model.to_owned(), (Instant::now(), probe.clone()));
        Ok(probe)
    }

    async fn probe_images(&self, model: &str) -> Option<bool> {
        self.probe(model).await.ok().and_then(|probe| probe.images)
    }
}

/// Turns a successful response into a stream of chunks. The secret lives as long as
/// the stream (to mask mid-stream errors) and is dropped (zeroed) with it.
fn chunk_stream(
    mut response: Response,
    secret: Option<Secret>,
    idle: Duration,
) -> CompletionStream {
    let stream = async_stream::stream! {
        let mut decoder = SseDecoder::new();
        let mut parser = StreamParser::new();
        loop {
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
                    Ok(chunks) => { for chunk in chunks { yield Ok(chunk); } }
                    Err(error) => { yield Err(scrub(error, secret.as_ref())); return; }
                }
            }
            if bytes.is_none() { break; }
        }
        match parser.end() {
            Ok(chunks) => { for chunk in chunks { yield Ok(chunk); } }
            Err(error) => yield Err(scrub(error, secret.as_ref())),
        }
    };
    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checked(url: &str) -> CheckedEndpoint {
        CheckedEndpoint {
            url: reqwest::Url::parse(url).unwrap(),
            host: "127.0.0.1".into(),
            port: 9,
            addrs: vec!["127.0.0.1:9".parse().unwrap()],
            ip_literal: true,
        }
    }

    #[test]
    fn the_version_segment_is_added_once() {
        for (base, expected) in [
            ("http://127.0.0.1:9", "http://127.0.0.1:9/v1/messages"),
            ("http://127.0.0.1:9/", "http://127.0.0.1:9/v1/messages"),
            ("http://127.0.0.1:9/v1", "http://127.0.0.1:9/v1/messages"),
            (
                "http://127.0.0.1:9/v1/?x=1",
                "http://127.0.0.1:9/v1/messages",
            ),
            ("http://127.0.0.1:9/gw", "http://127.0.0.1:9/gw/v1/messages"),
        ] {
            assert_eq!(AnthropicEndpoint::url(&checked(base), "messages"), expected);
        }
    }

    #[test]
    fn the_catalogue_reads_window_vision_thinking_and_the_next_page() {
        let body = br#"{"data":[
            {"id":"a","max_input_tokens":1000000,"capabilities":{"image_input":{"supported":true},"thinking":{"supported":true}}},
            {"id":"b","capabilities":{"image_input":{"supported":false},"thinking":{"supported":false}}},
            {"id":"c"},{"nope":1}],"has_more":true,"last_id":"c"}"#;
        let (models, next) = parse_models(body).unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].info.context_window.unwrap().value, 1_000_000);
        assert_eq!(
            (models[0].info.supports_images, models[0].thinking),
            (Some(true), Some(true))
        );
        assert_eq!(
            (models[1].info.supports_images, models[1].thinking),
            (Some(false), Some(false))
        );
        assert_eq!(
            (models[2].info.supports_images, models[2].thinking),
            (None, None)
        );
        assert_eq!(next.as_deref(), Some("c"));
        assert_eq!(
            parse_models(br#"{"data":[],"has_more":false}"#).unwrap().1,
            None
        );
        assert!(parse_models(b"[]").is_err());
    }
}
