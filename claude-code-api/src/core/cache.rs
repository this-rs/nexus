use dashmap::DashMap;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use crate::models::openai::{ChatCompletionResponse, ChatMessage, ContentPart, MessageContent};

#[derive(Clone)]
pub struct ResponseCache {
    inner: Arc<ResponseCacheInner>,
}

struct ResponseCacheInner {
    cache: DashMap<String, CacheEntry>,
    config: CacheConfig,
}

#[derive(Clone)]
pub struct CacheConfig {
    pub max_entries: usize,
    pub ttl_seconds: u64,
    pub enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            ttl_seconds: 3600, // 1 hour
            enabled: true,
        }
    }
}

#[derive(Clone, Debug)]
struct CacheEntry {
    response: ChatCompletionResponse,
    created_at: Instant,
    hit_count: usize,
}

/// Tags that make every field of a cache key self-describing.
///
/// A digest built by concatenating the request's bytes is ambiguous: the reader
/// cannot tell where one field ends and the next begins, so unrelated requests
/// collapse onto the same key. Tagging and length-prefixing every field makes
/// the encoding injective — two requests share a key only if they are equal on
/// everything the key covers.
mod tag {
    pub const ABSENT: u8 = 0;
    pub const MODEL: u8 = 1;
    pub const ROLE: u8 = 2;
    pub const NAME: u8 = 3;
    pub const TEXT: u8 = 4;
    pub const ARRAY: u8 = 5;
    pub const PART_TEXT: u8 = 6;
    pub const PART_IMAGE: u8 = 7;
    pub const DETAIL: u8 = 8;
    pub const TOOL_CALLS: u8 = 9;
    pub const CALL_NAME: u8 = 10;
    pub const CALL_ARGS: u8 = 11;
}

/// Hash one field as `tag | length | bytes`.
fn hash_field(hasher: &mut Sha256, tag: u8, bytes: &[u8]) {
    hasher.update([tag]);
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Hash the length of a sequence, so a shorter one is never the prefix of a
/// longer one.
fn hash_count(hasher: &mut Sha256, count: usize) {
    hasher.update((count as u64).to_le_bytes());
}

/// Hash the absence of an optional field, so `None` and `Some("")` differ.
fn hash_absent(hasher: &mut Sha256) {
    hasher.update([tag::ABSENT]);
}

impl ResponseCache {
    pub fn new(config: CacheConfig) -> Self {
        let cache = Self {
            inner: Arc::new(ResponseCacheInner {
                cache: DashMap::new(),
                config,
            }),
        };

        // 启动清理任务.
        //
        // `cleanup_loop` never returns, so the task — and the clone of the map it
        // holds — lives as long as the process. The gateway builds one cache at
        // startup, so that is one task, not one per request.
        let cache_clone = cache.clone();
        tokio::spawn(async move {
            cache_clone.cleanup_loop().await;
        });

        cache
    }

    /// The cache key for a `(model, messages)` pair.
    ///
    /// Every field is tagged and length-prefixed (see [`tag`]), so two calls
    /// share a key only when the model and the whole message list are equal.
    ///
    /// Known gap: the key covers the conversation, not the rest of the request.
    /// `tools`, `tool_choice`, `temperature`, `max_tokens` and `stop` never
    /// reach this function, so a caller that declares a tool can still be
    /// served the answer computed for a caller that declared none. Closing that
    /// gap means changing the signature, hence the two call sites in
    /// `api::chat`.
    pub fn generate_key(model: &str, messages: &[ChatMessage]) -> String {
        let mut hasher = Sha256::new();
        hash_field(&mut hasher, tag::MODEL, model.as_bytes());
        hash_count(&mut hasher, messages.len());

        for msg in messages {
            hash_field(&mut hasher, tag::ROLE, msg.role.as_bytes());

            match &msg.name {
                Some(name) => hash_field(&mut hasher, tag::NAME, name.as_bytes()),
                None => hash_absent(&mut hasher),
            }

            match &msg.content {
                Some(MessageContent::Text(text)) => {
                    hash_field(&mut hasher, tag::TEXT, text.as_bytes());
                },
                Some(MessageContent::Array(parts)) => {
                    hasher.update([tag::ARRAY]);
                    hash_count(&mut hasher, parts.len());
                    for part in parts {
                        match part {
                            ContentPart::Text { text } => {
                                hash_field(&mut hasher, tag::PART_TEXT, text.as_bytes());
                            },
                            ContentPart::ImageUrl { image_url } => {
                                hash_field(&mut hasher, tag::PART_IMAGE, image_url.url.as_bytes());
                                match &image_url.detail {
                                    Some(detail) => {
                                        hash_field(&mut hasher, tag::DETAIL, detail.as_bytes());
                                    },
                                    None => hash_absent(&mut hasher),
                                }
                            },
                        }
                    }
                },
                None => hash_absent(&mut hasher),
            }

            // An assistant turn whose content is empty still carries meaning:
            // two histories that differ only by the tool the assistant called
            // must not share a key. The call `id` is left out on purpose — it is
            // a fresh correlation handle on every turn, and hashing it would
            // make every tool-using conversation a permanent cache miss.
            match &msg.tool_calls {
                Some(calls) => {
                    hasher.update([tag::TOOL_CALLS]);
                    hash_count(&mut hasher, calls.len());
                    for call in calls {
                        hash_field(&mut hasher, tag::CALL_NAME, call.function.name.as_bytes());
                        hash_field(
                            &mut hasher,
                            tag::CALL_ARGS,
                            call.function.arguments.as_bytes(),
                        );
                    }
                },
                None => hash_absent(&mut hasher),
            }
        }

        format!("{:x}", hasher.finalize())
    }

    pub fn get(&self, key: &str) -> Option<ChatCompletionResponse> {
        if !self.inner.config.enabled {
            return None;
        }

        let mut entry = self.inner.cache.get_mut(key)?;

        // 检查是否过期
        let ttl = Duration::from_secs(self.inner.config.ttl_seconds);
        if entry.created_at.elapsed() > ttl {
            // The guard has to go before the removal, or the shard deadlocks on
            // itself. That opens a window in which another caller may store a
            // fresh response under this key, so re-check the age under the
            // removal's own lock instead of removing whatever is there now.
            drop(entry);
            self.inner
                .cache
                .remove_if(key, |_, entry| entry.created_at.elapsed() > ttl);
            debug!("Cache entry expired: {}", key);
            return None;
        }

        entry.hit_count += 1;
        let hit_count = entry.hit_count;
        let response = entry.response.clone();

        info!("Cache hit for key: {} (hits: {})", key, hit_count);
        Some(response)
    }

    pub fn put(&self, key: String, response: ChatCompletionResponse) {
        if !self.inner.config.enabled {
            return;
        }

        // Replacing a key that is already cached does not grow the map, so
        // evicting first would destroy an unrelated entry for nothing. Left
        // unguarded, a prompt that keeps being re-stored after its own entry
        // expires drains the rest of the cache one victim at a time.
        if !self.inner.cache.contains_key(&key)
            && self.inner.cache.len() >= self.inner.config.max_entries
        {
            self.evict_oldest();
        }

        // A cached response is handed to callers other than the one it was
        // computed for, so nothing identifying that first caller may be stored.
        // `conversation_id` points at its history, which a later caller could
        // otherwise read and append to; every caller gets its own back from the
        // gateway on a hit.
        let mut response = response;
        response.conversation_id = None;

        let entry = CacheEntry {
            response,
            created_at: Instant::now(),
            hit_count: 0,
        };

        self.inner.cache.insert(key.clone(), entry);
        debug!("Cached response for key: {}", key);
    }

    /// Remove the entry stored longest ago. A no-op only on an empty map:
    /// seeding the comparison from `Instant::now()` instead would also skip the
    /// eviction whenever the clock had not ticked since the newest insert, and
    /// let the map grow past `max_entries`.
    fn evict_oldest(&self) {
        let oldest = self
            .inner
            .cache
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().created_at))
            .min_by_key(|(_, created_at)| *created_at);

        if let Some((key, _)) = oldest {
            self.inner.cache.remove(&key);
            debug!("Evicted oldest cache entry: {}", key);
        }
    }

    async fn cleanup_loop(&self) {
        let ttl = Duration::from_secs(self.inner.config.ttl_seconds);

        loop {
            tokio::time::sleep(Duration::from_secs(300)).await; // 每5分钟清理一次

            let mut expired_keys = Vec::new();

            for entry in self.inner.cache.iter() {
                if entry.value().created_at.elapsed() > ttl {
                    expired_keys.push(entry.key().clone());
                }
            }

            for key in expired_keys {
                self.inner.cache.remove(&key);
                debug!("Removed expired cache entry: {}", key);
            }

            info!(
                "Cache cleanup: {} entries remaining",
                self.inner.cache.len()
            );
        }
    }

    pub fn stats(&self) -> CacheStats {
        let mut total_hits = 0;
        let mut total_entries = 0;

        for entry in self.inner.cache.iter() {
            total_entries += 1;
            total_hits += entry.value().hit_count;
        }

        CacheStats {
            total_entries,
            total_hits,
            enabled: self.inner.config.enabled,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CacheStats {
    pub total_entries: usize,
    pub total_hits: usize,
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::openai::{ChatChoice, FunctionCall, ImageUrl, ToolCall, Usage};

    fn config(max_entries: usize, ttl_seconds: u64) -> CacheConfig {
        CacheConfig {
            max_entries,
            ttl_seconds,
            enabled: true,
        }
    }

    fn response(id: &str) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: id.into(),
            object: "chat.completion".into(),
            created: 0,
            model: "claude-sonnet-4".into(),
            choices: vec![ChatChoice {
                index: 0,
                message: message("assistant", text("ok")),
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
            },
            conversation_id: None,
        }
    }

    fn message(role: &str, content: Option<MessageContent>) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content,
            name: None,
            tool_calls: None,
        }
    }

    fn text(body: &str) -> Option<MessageContent> {
        Some(MessageContent::Text(body.into()))
    }

    fn parts(parts: Vec<ContentPart>) -> Option<MessageContent> {
        Some(MessageContent::Array(parts))
    }

    fn image(url: &str, detail: Option<&str>) -> ContentPart {
        ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: url.into(),
                detail: detail.map(Into::into),
            },
        }
    }

    fn calls(name: &str, arguments: &str, id: &str) -> Option<Vec<ToolCall>> {
        Some(vec![ToolCall {
            id: id.into(),
            tool_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }])
    }

    /// The key of a one-user-turn request, the shape `api::chat` sends most.
    fn key(model: &str, content: Option<MessageContent>) -> String {
        ResponseCache::generate_key(model, &[message("user", content)])
    }

    // ── generate_key: no two different requests may share a key ──
    //
    // The digest used to be a plain concatenation of the request's bytes, with
    // no mark between one field and the next. Each test below is a pair of
    // requests whose bytes concatenated to the same stream.

    #[test]
    fn the_model_name_cannot_bleed_into_the_first_role() {
        assert_ne!(
            ResponseCache::generate_key("gpt-4", &[message("o", None)]),
            ResponseCache::generate_key("gpt-4o", &[message("", None)]),
            "the model must not be readable as the start of the first role"
        );
    }

    #[test]
    fn a_role_cannot_swallow_the_start_of_the_prompt() {
        assert_ne!(
            ResponseCache::generate_key("m", &[message("user", text("ab"))]),
            ResponseCache::generate_key("m", &[message("userab", None)]),
            "the role must not be readable as the start of the content"
        );
    }

    #[test]
    fn a_prompt_split_into_parts_is_not_the_same_request_as_one_string() {
        assert_ne!(
            key("m", text("ab")),
            key(
                "m",
                parts(vec![
                    ContentPart::Text { text: "a".into() },
                    ContentPart::Text { text: "b".into() },
                ])
            ),
            "one text part per fragment is a different request from one string"
        );
    }

    #[test]
    fn a_prompt_quoting_an_image_url_is_not_a_request_for_that_image() {
        let url = "https://example.invalid/chart.png";
        assert_ne!(
            key("m", parts(vec![ContentPart::Text { text: url.into() }])),
            key("m", parts(vec![image(url, None)])),
            "quoting a URL as text must not be served the answer about the image"
        );
    }

    #[test]
    fn the_image_detail_level_is_part_of_the_key() {
        let url = "https://example.invalid/chart.png";
        let high = key("m", parts(vec![image(url, Some("high"))]));
        let low = key("m", parts(vec![image(url, Some("low"))]));
        let unset = key("m", parts(vec![image(url, None)]));

        assert_ne!(high, low, "`detail` changes what the model is shown");
        assert_ne!(high, unset, "an explicit detail is not the default");
    }

    #[test]
    fn the_speakers_name_is_part_of_the_key() {
        let mut named = message("user", text("meme question"));
        named.name = Some("alice".into());

        assert_ne!(
            ResponseCache::generate_key("m", &[message("user", text("meme question"))]),
            ResponseCache::generate_key("m", &[named]),
            "`name` identifies the speaker and reaches the model"
        );
    }

    #[test]
    fn an_assistant_tool_call_is_part_of_the_key() {
        let history = |assistant: ChatMessage| {
            ResponseCache::generate_key(
                "m",
                &[message("user", text("quel temps a Lyon ?")), assistant],
            )
        };
        let called = |arguments: &str| {
            let mut msg = message("assistant", None);
            msg.tool_calls = calls("get_weather", arguments, "call_1");
            history(msg)
        };

        assert_ne!(
            called(r#"{"city":"Lyon"}"#),
            called(r#"{"city":"Paris"}"#),
            "two histories that asked different questions of a tool differ"
        );
        assert_ne!(
            called("{}"),
            history(message("assistant", None)),
            "a turn that called a tool is not an empty turn"
        );
    }

    /// Deliberate: the correlation id is fresh on every turn, so hashing it
    /// would turn every tool-using conversation into a permanent miss.
    #[test]
    fn the_tool_call_id_is_not_part_of_the_key() {
        let keyed = |id: &str| {
            let mut msg = message("assistant", None);
            msg.tool_calls = calls("get_weather", "{}", id);
            ResponseCache::generate_key("m", &[msg])
        };

        assert_eq!(keyed("call_1"), keyed("call_2"));
    }

    #[test]
    fn the_same_request_always_hashes_to_the_same_key() {
        assert_eq!(key("m", text("bonjour")), key("m", text("bonjour")));
        assert_eq!(
            ResponseCache::generate_key("m", &[]),
            ResponseCache::generate_key("m", &[]),
            "an empty conversation is still a well-defined key"
        );
        assert_ne!(
            key("m", text("bonjour")),
            key("autre-modele", text("bonjour")),
            "the model is part of the key"
        );
        assert_ne!(
            ResponseCache::generate_key("m", &[]),
            key("m", text("")),
            "no turn is not the same as one empty turn"
        );
    }

    // ── get / put: storage, expiry, eviction ──

    #[tokio::test]
    async fn a_disabled_cache_neither_stores_nor_serves() {
        let cache = ResponseCache::new(CacheConfig {
            enabled: false,
            ..CacheConfig::default()
        });

        cache.put("k".into(), response("r1"));

        assert!(cache.get("k").is_none());
        let stats = cache.stats();
        assert_eq!(stats.total_entries, 0, "nothing may be stored");
        assert!(!stats.enabled);
    }

    #[tokio::test]
    async fn a_fresh_entry_is_served_and_every_read_counts_as_a_hit() {
        let cache = ResponseCache::new(config(10, 3_600));

        assert!(cache.get("k").is_none(), "a miss before anything is stored");

        cache.put("k".into(), response("r1"));

        assert_eq!(cache.get("k").unwrap().id, "r1");
        assert_eq!(cache.get("k").unwrap().id, "r1");

        let stats = cache.stats();
        assert_eq!(stats.total_entries, 1);
        assert_eq!(stats.total_hits, 2);
        assert!(stats.enabled);
    }

    #[tokio::test]
    async fn an_entry_past_its_ttl_is_neither_served_nor_kept() {
        let cache = ResponseCache::new(config(10, 0));

        cache.put("k".into(), response("r1"));
        assert_eq!(cache.stats().total_entries, 1, "stored before it is read");

        tokio::time::sleep(Duration::from_millis(5)).await;

        assert!(cache.get("k").is_none(), "an expired entry is not served");
        assert_eq!(
            cache.stats().total_entries,
            0,
            "and is not left behind to be swept later"
        );
    }

    #[tokio::test]
    async fn re_storing_a_cached_key_must_not_evict_an_unrelated_entry() {
        let cache = ResponseCache::new(config(2, 3_600));

        cache.put("cold".into(), response("r-cold"));
        tokio::time::sleep(Duration::from_millis(2)).await;
        cache.put("hot".into(), response("r-hot"));

        // `api::chat` stores the key on every miss, so a popular prompt is
        // re-stored over and over. Replacing an entry does not grow the map.
        cache.put("hot".into(), response("r-hot-2"));

        assert_eq!(
            cache.stats().total_entries,
            2,
            "replacing an entry costs no room, so nothing may be evicted"
        );
        assert!(
            cache.get("cold").is_some(),
            "re-storing `hot` must not destroy `cold`"
        );
        assert_eq!(
            cache.get("hot").unwrap().id,
            "r-hot-2",
            "and must replace the response"
        );
    }

    #[tokio::test]
    async fn a_put_at_capacity_evicts_the_entry_stored_longest_ago() {
        let cache = ResponseCache::new(config(2, 3_600));

        cache.put("first".into(), response("r-first"));
        tokio::time::sleep(Duration::from_millis(2)).await;
        cache.put("second".into(), response("r-second"));
        tokio::time::sleep(Duration::from_millis(2)).await;
        cache.put("third".into(), response("r-third"));

        assert!(cache.get("first").is_none(), "the oldest entry goes first");
        assert!(cache.get("second").is_some());
        assert!(cache.get("third").is_some());
        assert_eq!(cache.stats().total_entries, 2, "capacity is respected");
    }

    /// `max_entries: 0` is not how the cache is turned off — `enabled: false`
    /// is. With no room at all, every store evicts the previous entry and the
    /// cache degrades to the last response, which is what the eviction pass
    /// does when it is handed an empty map.
    #[tokio::test]
    async fn a_cache_with_no_room_keeps_only_the_last_response() {
        let cache = ResponseCache::new(config(0, 3_600));

        cache.put("a".into(), response("r-a"));
        assert_eq!(cache.stats().total_entries, 1);

        cache.put("b".into(), response("r-b"));

        assert!(cache.get("a").is_none());
        assert_eq!(cache.get("b").unwrap().id, "r-b");
        assert_eq!(cache.stats().total_entries, 1);
    }

    // ── the background sweep ──
    //
    // The sweep sleeps for five minutes between passes, so these tests run on
    // tokio's virtual clock. Entry ages come from `Instant`, which the virtual
    // clock does not move: a ttl of 0 is what makes an entry old here.

    #[tokio::test(start_paused = true)]
    async fn the_background_sweep_drops_expired_entries_no_one_reads() {
        let cache = ResponseCache::new(config(10, 0));

        cache.put("k".into(), response("r1"));
        assert_eq!(cache.stats().total_entries, 1);

        tokio::time::sleep(Duration::from_secs(301)).await;

        assert_eq!(
            cache.stats().total_entries,
            0,
            "an expired entry must not wait for a read to be reclaimed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_background_sweep_keeps_entries_within_their_ttl() {
        let cache = ResponseCache::new(config(10, 3_600));

        cache.put("k".into(), response("r1"));

        tokio::time::sleep(Duration::from_secs(301)).await;

        assert_eq!(cache.stats().total_entries, 1);
        assert_eq!(
            cache.get("k").unwrap().id,
            "r1",
            "the sweep must not touch a live entry"
        );
    }

    #[tokio::test]
    async fn a_stored_response_drops_the_first_callers_conversation_id() {
        let cache = ResponseCache::new(config(10, 3_600));

        let mut first = response("r1");
        first.conversation_id = Some("conv-du-premier-appelant".into());
        cache.put("k".into(), first);

        assert_eq!(
            cache.get("k").unwrap().conversation_id,
            None,
            "a hit must not hand a later caller the first one's conversation"
        );
    }

    #[tokio::test]
    async fn stats_counts_every_entry_and_every_hit() {
        let cache = ResponseCache::new(config(10, 3_600));

        cache.put("a".into(), response("r-a"));
        cache.put("b".into(), response("r-b"));
        assert!(cache.get("a").is_some());
        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_some());

        let stats = cache.stats();
        assert_eq!(stats.total_entries, 2);
        assert_eq!(stats.total_hits, 3);
        assert!(stats.enabled);
    }
}
