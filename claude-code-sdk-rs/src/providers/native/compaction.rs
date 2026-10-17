//! Context compaction of a native session (contract §4 `compaction`).
//!
//! When the prompt gets close to the context window, the old part of the history
//! is replaced by a summary written by the **same endpoint** (a fixed prompt, no
//! tools) and the last `keep_recent` messages stay **intact** — in particular
//! their `reasoning`, which DeepSeek wants back with the tool calls (A39).
//!
//! Only automatic compaction exists: the contract has no manual trigger for the
//! native harness. With an unknown context window nothing is compacted; the
//! endpoint's refusal then surfaces as the typed `context_too_small`.
//!
//! A summary that cannot be written never fails the turn: compaction is a
//! convenience, and the loop says the failure with
//! `provider_notice { compaction_failed }` while the history stays intact (see
//! `loop::maybe_compact`).
//!
//! **Images** of the kept tail stay whole. Those of the summarised part are
//! rendered as `[image <media type>]` in the summarisation prompt (never the
//! pixels: the summary is text) and the loop says so with
//! `provider_notice { images_compacted }`; nothing is lost in silence.
//!
//! # The cheap tier before the summary (CTX3)
//!
//! A summary costs an endpoint call and rewrites the history. Long before it is
//! needed, most of the growth of a session is in **tool results the model no
//! longer reads**: what it took from them is in the conversation that followed.
//! Those older than the last [`CompactionConfig::keep_tool_rounds`] rounds are
//! therefore replaced by [`STUB_MARKER`] — the message stays (same role, same
//! `tool_call_id`), only its text goes, and eighty-odd characters of stub take the
//! place of a whole file.
//!
//! Three rules make it safe for the prompt cache, which this tier must not waste
//! (a stub that costs more than it saves is a loss):
//!
//! - **only under pressure** ([`should_stub`], `stub_ratio` of the window): a
//!   session far from its window touches nothing, so an ordinary one keeps its
//!   cache whole from the first request to the last;
//! - **by blocks** ([`CompactionConfig::stub_block`]): one pass stubs every result
//!   that has aged out, never one result at a time — each pass costs one re-cached
//!   suffix, so the passes are counted, not the results;
//! - **once per result**: a stubbed text is recognised by [`is_stub`] and never
//!   touched again, so a later pass over the same history changes nothing.
//!
//! Images a tool returned are not stubbed here: they live in the user message that
//! follows the results, and a full compaction already summarises them as
//! `[image <type>]`. A session carrying many old ones still reaches the summary.

use futures::StreamExt;

use crate::agent::{ProviderError, Usage};
use crate::model::{ChatMessage, CompletionChunk, CompletionRequest, ModelEndpoint, Role};

/// The fixed prompt of the summarisation call.
pub const SUMMARY_SYSTEM_PROMPT: &str = "You are compacting the history of an agent session. Summarise the conversation below so that the agent can continue the work: keep the user's goals, decisions taken, facts learned, files and identifiers mentioned, tool results that matter, and what remains to do. Be dense and factual. Do not address the user and do not call tools.";

/// First line of the message that stands for the compacted history.
pub const SUMMARY_MARKER: &str = "[Summary of the earlier conversation]";

/// Longest content of one message in the summarisation prompt, in characters.
const RENDER_LIMIT: usize = 4000;

/// What one image is assumed to cost in the prompt, in tokens: a rough middle
/// between a low-detail tile (about 85 on OpenAI) and a full-resolution image
/// on a VL model (several thousand). The base64 size says nothing of it.
pub const IMAGE_TOKEN_ESTIMATE: u64 = 1024;

/// What an image becomes in the summarisation prompt (followed by its media type).
pub const IMAGE_MARKER: &str = "[image";

/// What a tool result older than the kept rounds becomes: its message stays (same
/// role, same `tool_call_id`), its text is gone, and the model is told how to get it
/// back. Deliberately short: a stub that is long is a stub that pays nothing.
pub const STUB_MARKER: &str =
    "[tool result stubbed to save context — call the tool again if you still need it]";

/// How a stubbed result begins: recognition is a prefix, so a marker that gains a
/// word later still counts as stubbed and is never stubbed twice.
const STUB_PREFIX: &str = "[tool result stubbed";

/// Whether `content` is a stub (see [`STUB_MARKER`]), i.e. this result has already
/// been elided and must not be counted, freed or elided again.
pub fn is_stub(content: Option<&str>) -> bool {
    content.is_some_and(|text| text.starts_with(STUB_PREFIX))
}

/// When and how much to compact.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionConfig {
    /// Automatic compaction on or off. Off also disables the cheap tier: a host that
    /// said no compaction at all does not want its history rewritten either.
    pub enabled: bool,
    /// Fraction of the context window at which compaction starts.
    pub threshold_ratio: f64,
    /// Number of last messages kept intact.
    pub keep_recent: usize,
    /// Output cap of the summarisation call, in tokens.
    pub summary_max_tokens: u32,
    /// Fraction of the context window at which old tool results start being stubbed
    /// (the cheap tier). Meant **below** `threshold_ratio`: stub first, summarise only
    /// if that is not enough.
    pub stub_ratio: f64,
    /// Rounds of the tool loop whose tool results are never stubbed; `0` turns the
    /// cheap tier off entirely. A round is one assistant message carrying tool calls,
    /// with the results that answer it: parallel calls age together.
    pub keep_tool_rounds: usize,
    /// Fewest aged-out results worth one cache break. The pass stubs a block, never a
    /// single result: each pass rewrites the history under the cache once.
    pub stub_block: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_ratio: 0.8,
            keep_recent: 6,
            summary_max_tokens: 2048,
            stub_ratio: 0.5,
            keep_tool_rounds: 6,
            stub_block: 4,
        }
    }
}

/// Rough size of a message list in tokens (four characters per token): used
/// when the endpoint has not reported the size of the prompt.
pub fn estimate_tokens(messages: &[ChatMessage]) -> u64 {
    let chars: usize = messages
        .iter()
        .map(|message| {
            message.content.as_deref().map_or(0, str::len)
                + message.reasoning.as_deref().map_or(0, str::len)
                + message
                    .tool_calls
                    .iter()
                    .map(|call| call.name.len() + call.arguments.len())
                    .sum::<usize>()
        })
        .sum();
    (chars as u64).div_ceil(4) + count_images(messages) as u64 * IMAGE_TOKEN_ESTIMATE
}

/// Number of images carried by `messages`.
pub fn count_images(messages: &[ChatMessage]) -> usize {
    messages.iter().map(|message| message.images.len()).sum()
}

/// Whether a prompt of `prompt_tokens` calls for compaction in `window`.
pub fn should_compact(config: &CompactionConfig, window: Option<u64>, prompt_tokens: u64) -> bool {
    let Some(window) = window else { return false };
    config.enabled && (prompt_tokens as f64) >= (window as f64) * config.threshold_ratio
}

/// Whether a prompt of `prompt_tokens` is close enough to `window` for the cheap
/// tier — stubbing old tool results — to be worth a cache break.
///
/// Nothing is stubbed without a known window, for the same reason nothing is
/// compacted without one: there is no way to say whether the history is a problem.
pub fn should_stub(config: &CompactionConfig, window: Option<u64>, prompt_tokens: u64) -> bool {
    let Some(window) = window else { return false };
    config.enabled
        && config.keep_tool_rounds > 0
        && (prompt_tokens as f64) >= (window as f64) * config.stub_ratio
}

/// Start of the last `keep_rounds` rounds, or `None` when the history holds no more
/// than that (nothing is old enough to be stubbed).
///
/// The unit is the **round**, not the message: one assistant message carrying tool
/// calls and the results that answer it. Counting messages instead would cut a round
/// in half on a parallel call, leaving one of its results stubbed and its twin whole.
pub fn kept_rounds_from(messages: &[ChatMessage], keep_rounds: usize) -> Option<usize> {
    if keep_rounds == 0 {
        return None;
    }
    let mut seen = 0;
    for (index, message) in messages.iter().enumerate().rev() {
        if message.role == Role::Assistant && !message.tool_calls.is_empty() {
            seen += 1;
            if seen == keep_rounds {
                return Some(index);
            }
        }
    }
    None
}

/// What one stub pass would change: the old results it would elide, in order, and
/// how much prompt text that takes out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StubPlan {
    /// Indices of the tool results to elide, ascending.
    pub indices: Vec<usize>,
    /// Characters the prompt loses once the plan is applied (stub text deducted).
    pub chars_freed: usize,
}

impl StubPlan {
    /// How many results the pass would elide.
    pub fn len(&self) -> usize {
        self.indices.len()
    }

    /// Whether the pass has nothing to do.
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// Plans one stub pass over the history, or `None` when there is nothing to do yet.
///
/// Eligible: a `tool` message before the last `keep_tool_rounds` rounds, whose text is
/// not already a stub. The pass is **by blocks** ([`CompactionConfig::stub_block`]): a
/// handful of aged-out results that is not worth one cache break waits for the next.
/// Reasoning, tool calls and `tool_call_id`s are never touched — DeepSeek wants the
/// reasoning of a call echoed back (A39), and the id is what pairs a result with it.
pub fn plan_stub(messages: &[ChatMessage], config: &CompactionConfig) -> Option<StubPlan> {
    let from = kept_rounds_from(messages, config.keep_tool_rounds)?;
    let mut plan = StubPlan {
        indices: Vec::new(),
        chars_freed: 0,
    };
    for (index, message) in messages[..from].iter().enumerate() {
        if message.role != Role::Tool || is_stub(message.content.as_deref()) {
            continue;
        }
        plan.chars_freed += message.content.as_deref().map_or(0, str::len);
        plan.indices.push(index);
    }
    if plan.indices.is_empty() {
        return None;
    }
    if plan.indices.len() < config.stub_block.max(1) {
        return None;
    }
    plan.chars_freed = plan
        .chars_freed
        .saturating_sub(STUB_MARKER.len() * plan.indices.len());
    Some(plan)
}

/// Applies a plan: every listed result becomes [`STUB_MARKER`]. Returns how many were
/// elided — `0` when the plan was already applied, because a stub is never stubbed
/// again. The message list keeps its length and its roles: only text changes, which is
/// what makes the cache break of a pass a declared, bounded one.
pub fn apply_stub(messages: &mut [ChatMessage], plan: &StubPlan) -> usize {
    let mut elided = 0;
    for &index in &plan.indices {
        if let Some(message) = messages.get_mut(index)
            && message.role == Role::Tool
            && !is_stub(message.content.as_deref())
        {
            message.content = Some(STUB_MARKER.to_owned());
            elided += 1;
        }
    }
    elided
}

/// Index where the kept tail starts, or `None` when there is not enough old
/// history to be worth a summary (fewer than two messages).
///
/// The tail never starts on a `tool` message: that would orphan the result of a
/// call whose assistant message was summarised away.
pub fn split_point(messages: &[ChatMessage], keep_recent: usize) -> Option<usize> {
    if messages.len() <= keep_recent {
        return None;
    }
    let mut split = messages.len() - keep_recent;
    while split > 0 && messages[split].role == Role::Tool {
        split -= 1;
    }
    (split >= 2).then_some(split)
}

fn clip(text: &str) -> String {
    if text.chars().count() <= RENDER_LIMIT {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(RENDER_LIMIT).collect();
    clipped.push_str(" […]");
    clipped
}

/// The old messages as plain text for the summarisation prompt.
pub fn render(older: &[ChatMessage]) -> String {
    let mut out = String::new();
    for message in older {
        let label = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool result",
            #[allow(unreachable_patterns)]
            _ => "message",
        };
        out.push_str(&format!("[{label}]"));
        if let Some(content) = &message.content {
            out.push(' ');
            out.push_str(&clip(content));
        }
        for image in &message.images {
            out.push_str(&format!("\n{IMAGE_MARKER} {}]", image.media_type));
        }
        for call in &message.tool_calls {
            out.push_str(&format!(
                "\n(called {} with {})",
                call.name,
                clip(&call.arguments)
            ));
        }
        out.push_str("\n\n");
    }
    out
}

/// The summarisation request: fixed system prompt, the rendered history, no tools.
pub fn summary_request(
    model: &str,
    older: &[ChatMessage],
    instructions: Option<&str>,
    config: &CompactionConfig,
) -> CompletionRequest {
    let mut body = render(older);
    if let Some(extra) = instructions {
        body.push_str("Additional instructions: ");
        body.push_str(extra);
    }
    let mut request = CompletionRequest::new(
        model,
        vec![
            ChatMessage::system(SUMMARY_SYSTEM_PROMPT),
            ChatMessage::user(body),
        ],
    );
    request.max_tokens = Some(config.summary_max_tokens);
    request.temperature = Some(0.0);
    request
}

/// Asks the endpoint for the summary. Returns the text and the usage reported.
///
/// A reasoning model can spend its whole output budget thinking and answer with no
/// text at all — measured on a compaction at 833 000 prompt tokens, where the empty
/// summary was read as a protocol error and took the whole turn down with it. The
/// summary is therefore asked **once more** with twice the output budget, and the
/// usage of both attempts is summed (a call that reasoned is still billed). One
/// retry, never a loop: a summary that stays empty is given up on, saying why.
///
/// Every other failure — the endpoint refused, the stream broke — is returned at
/// once: asking again an endpoint that has just failed would only pay twice for the
/// same answer.
pub async fn summarise(
    endpoint: &dyn ModelEndpoint,
    mut request: CompletionRequest,
) -> Result<(String, Option<Usage>), ProviderError> {
    // The request is cloned for the first attempt only: should a retry be needed,
    // it moves the original (a summary prompt is the whole old history).
    let (text, mut usage, reasoned) = summary_attempt(endpoint, request.clone()).await?;
    if !text.is_empty() {
        return Ok((text, usage));
    }
    if reasoned {
        request.max_tokens = request.max_tokens.map(|max| max.saturating_mul(2));
        let (text, retried, _) = summary_attempt(endpoint, request).await?;
        usage = add_usage(usage, retried);
        if !text.is_empty() {
            return Ok((text, usage));
        }
    }
    Err(ProviderError::protocol(if reasoned {
        "the compaction summary came back empty: the model spent its output budget on \
         reasoning (reasoning came back, no text), and the retry with twice the budget \
         was empty too"
    } else {
        "the compaction summary came back empty"
    }))
}

/// One summary attempt: its text, the usage reported, and whether the model reasoned
/// at all. Reasoning is tracked, never dropped in silence: it is what tells an empty
/// answer caused by a budget spent thinking from an endpoint that answered nothing.
async fn summary_attempt(
    endpoint: &dyn ModelEndpoint,
    request: CompletionRequest,
) -> Result<(String, Option<Usage>, bool), ProviderError> {
    let mut stream = endpoint.complete(request).await?;
    let mut text = String::new();
    let mut usage = None;
    let mut reasoned = false;
    while let Some(chunk) = stream.next().await {
        match chunk? {
            CompletionChunk::Text(delta) => text.push_str(&delta),
            CompletionChunk::Reasoning(_) => reasoned = true,
            CompletionChunk::Usage(reported) => usage = add_usage(usage, Some(reported)),
            _ => {},
        }
    }
    Ok((text.trim().to_owned(), usage, reasoned))
}

/// Sums what two summary attempts reported, so a retried summarisation is counted
/// for both calls. `None` stays `None` when neither reported anything: the caller
/// then falls back on its own estimate.
fn add_usage(total: Option<Usage>, next: Option<Usage>) -> Option<Usage> {
    match (total, next) {
        // Only one side reported: nothing to add to it.
        (total, None) => total,
        (None, next) => next,
        (Some(mut total), Some(next)) => {
            let add = |slot: &mut Option<u64>, value: Option<u64>| {
                if let Some(value) = value {
                    *slot = Some(slot.unwrap_or(0) + value);
                }
            };
            add(&mut total.input_tokens, next.input_tokens);
            add(&mut total.output_tokens, next.output_tokens);
            add(&mut total.cache_read_tokens, next.cache_read_tokens);
            add(&mut total.cache_creation_tokens, next.cache_creation_tokens);
            add(&mut total.reasoning_tokens, next.reasoning_tokens);
            // `context_tokens` is the size of ONE prompt, not a quantity: the latest wins.
            if next.context_tokens.is_some() {
                total.context_tokens = next.context_tokens;
            }
            if !next.by_model.is_empty() {
                total.by_model = next.by_model;
            }
            Some(total)
        },
    }
}

/// The history after compaction: the summary, then the untouched tail.
pub fn apply(mut messages: Vec<ChatMessage>, split: usize, summary: &str) -> Vec<ChatMessage> {
    let tail = messages.split_off(split);
    let mut out = Vec::with_capacity(tail.len() + 1);
    out.push(ChatMessage::user(format!("{SUMMARY_MARKER}\n{summary}")));
    out.extend(tail);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ImagePart, ToolCallChunk};

    #[test]
    fn the_usage_of_a_retried_summary_is_the_sum_of_both_attempts() {
        // A first attempt that reasoned and answered nothing is not free: both calls
        // are counted, or the session budget would under-count a wasted summary.
        let first = Usage {
            input_tokens: Some(10),
            output_tokens: Some(20),
            reasoning_tokens: Some(18),
            context_tokens: Some(4_000),
            ..Usage::default()
        };
        let second = Usage {
            input_tokens: Some(10),
            output_tokens: Some(30),
            ..Usage::default()
        };
        let total = add_usage(Some(first), Some(second)).unwrap();
        assert_eq!(total.input_tokens, Some(20));
        assert_eq!(total.output_tokens, Some(50));
        assert_eq!(total.reasoning_tokens, Some(18));
        assert_eq!(total.cache_read_tokens, None);
        // `context_tokens` is a size, not a quantity: it is never summed, and an
        // attempt that reported none leaves the last reported one in place.
        assert_eq!(total.context_tokens, Some(4_000));
    }

    #[test]
    fn a_usage_nobody_reported_stays_absent() {
        // `None` means "the endpoint said nothing", never a zero: the caller then
        // falls back on its own estimate (see `loop::spend`).
        assert_eq!(add_usage(None, None), None);
        let one = add_usage(
            None,
            Some(Usage {
                output_tokens: Some(5),
                ..Usage::default()
            }),
        )
        .unwrap();
        assert_eq!(one.output_tokens, Some(5));
        assert_eq!(one.input_tokens, None);
    }

    fn history() -> Vec<ChatMessage> {
        let mut call = ChatMessage::assistant_tool_calls(vec![ToolCallChunk {
            id: "c1".into(),
            name: "t".into(),
            arguments: "{}".into(),
        }]);
        call.reasoning = Some("because".into());
        vec![
            ChatMessage::user("one"),
            ChatMessage::assistant("two"),
            ChatMessage::user("three"),
            call,
            ChatMessage::tool("c1", "result"),
            ChatMessage::assistant("done").with_reasoning("kept reasoning"),
        ]
    }

    #[test]
    fn the_tail_never_starts_on_a_tool_result() {
        let messages = history();
        // keep 2 → split at 4 (the tool result): moved back to the assistant call.
        assert_eq!(split_point(&messages, 2), Some(3));
        assert_eq!(split_point(&messages, 3), Some(3));
        assert_eq!(split_point(&messages, 1), Some(5));
    }

    #[test]
    fn too_little_history_is_not_compacted() {
        let messages = history();
        assert_eq!(split_point(&messages, 6), None);
        assert_eq!(split_point(&messages, 5), None); // one old message only
        assert_eq!(split_point(&messages, 4), Some(2));
    }

    #[test]
    fn the_kept_tail_is_intact_with_its_reasoning() {
        let messages = history();
        let split = split_point(&messages, 3).unwrap();
        let compacted = apply(messages.clone(), split, "SUMMARY");
        assert_eq!(compacted.len(), 1 + 3);
        assert!(
            compacted[0]
                .content
                .as_deref()
                .unwrap()
                .starts_with(SUMMARY_MARKER)
        );
        assert!(compacted[0].content.as_deref().unwrap().contains("SUMMARY"));
        assert_eq!(compacted[1..], messages[3..]);
        assert_eq!(compacted[1].reasoning.as_deref(), Some("because"));
        assert_eq!(compacted[3].reasoning.as_deref(), Some("kept reasoning"));
    }

    #[test]
    fn the_threshold_needs_a_known_window() {
        let config = CompactionConfig::default();
        assert!(!should_compact(&config, None, 1_000_000));
        assert!(!should_compact(&config, Some(1000), 799));
        assert!(should_compact(&config, Some(1000), 800));
        let off = CompactionConfig {
            enabled: false,
            ..config
        };
        assert!(!should_compact(&off, Some(1000), 5000));
    }

    #[test]
    fn the_summary_request_has_a_fixed_prompt_and_no_tools() {
        let messages = history();
        let request = summary_request(
            "m",
            &messages[..3],
            Some("keep file names"),
            &CompactionConfig::default(),
        );
        assert_eq!(
            request.messages[0].content.as_deref(),
            Some(SUMMARY_SYSTEM_PROMPT)
        );
        assert!(request.tools.is_empty());
        let body = request.messages[1].content.as_deref().unwrap();
        assert!(body.contains("[user] one") && body.contains("keep file names"));
    }

    #[test]
    fn estimates_count_content_reasoning_and_calls() {
        let messages = vec![ChatMessage::assistant("abcdefgh").with_reasoning("abcd")];
        assert_eq!(estimate_tokens(&messages), 3);
    }

    fn pixel() -> ImagePart {
        ImagePart {
            media_type: "image/png".into(),
            data_base64: "iVBORw0KGgo=".into(),
        }
    }

    #[test]
    fn images_are_estimated_per_image_not_per_base64_byte() {
        let messages = vec![ChatMessage::user_with_images(
            "look",
            vec![pixel(), pixel()],
        )];
        assert_eq!(count_images(&messages), 2);
        assert_eq!(estimate_tokens(&messages), 1 + 2 * IMAGE_TOKEN_ESTIMATE);
    }

    #[test]
    fn a_summarised_image_is_rendered_as_a_marker_never_as_pixels() {
        let older = vec![
            ChatMessage::user_with_images("what is this?", vec![pixel()]),
            ChatMessage::assistant("a pixel"),
        ];
        let text = render(&older);
        assert!(
            text.contains("[user] what is this?\n[image image/png]"),
            "{text}"
        );
        assert!(!text.contains("iVBORw0KGgo="), "{text}");
        let request = summary_request("m", &older, None, &CompactionConfig::default());
        assert!(request.messages[1].images.is_empty());
    }

    #[test]
    fn images_of_the_kept_tail_survive_compaction_whole() {
        let mut messages = history();
        messages.push(ChatMessage::user_with_images("and this?", vec![pixel()]));
        messages.push(ChatMessage::assistant("another pixel"));
        let split = split_point(&messages, 2).unwrap();
        let compacted = apply(messages.clone(), split, "SUMMARY");
        assert_eq!(compacted[1].images, vec![pixel()]);
        assert_eq!(compacted[1..], messages[split..]);
    }

    // -----------------------------------------------------------------------
    // The cheap tier: old tool results become a stub (CTX3)
    // -----------------------------------------------------------------------

    /// `count` rounds of the tool loop: per round an assistant call and a result of
    /// `result_chars` characters. Message `0` is the user message of the turn; round `i`
    /// (counted from `start`) holds its call at `1 + 2i` and its result at `2 + 2i`.
    fn rounds_from(start: usize, count: usize, result_chars: usize) -> Vec<ChatMessage> {
        let mut messages = Vec::new();
        for index in start..start + count {
            let id = format!("c{index}");
            let mut call = ChatMessage::assistant_tool_calls(vec![ToolCallChunk {
                id: id.clone(),
                name: "read".into(),
                arguments: "{\"path\":\"src/lib.rs\"}".into(),
            }]);
            // A39: the reasoning of a call is replayed with it, and it is exactly what a
            // stub pass must not touch.
            call.reasoning = Some(format!("round {index} needs the file"));
            messages.push(call);
            messages.push(ChatMessage::tool(id, "x".repeat(result_chars)));
        }
        messages
    }

    /// A whole history: the user message, then `count` rounds from round 0.
    fn rounds(count: usize, result_chars: usize) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::user("go")];
        messages.extend(rounds_from(0, count, result_chars));
        messages
    }

    fn stub_config(keep_tool_rounds: usize, stub_block: usize) -> CompactionConfig {
        CompactionConfig {
            keep_tool_rounds,
            stub_block,
            ..CompactionConfig::default()
        }
    }

    #[test]
    fn the_results_of_the_kept_rounds_are_never_stubbed() {
        // 8 rounds, the last 3 kept: the 5 older results are eligible, one index each,
        // and the kept ones are not in the plan.
        let messages = rounds(8, 200);
        let plan = plan_stub(&messages, &stub_config(3, 4)).expect("a plan");
        assert_eq!(plan.indices, vec![2, 4, 6, 8, 10]);
        assert_eq!(plan.len(), 5);
        // The characters freed are the result text, the stub text deducted: a stub that
        // cost as much as the text it replaces would free nothing.
        assert_eq!(plan.chars_freed, 5 * 200 - 5 * STUB_MARKER.len());
        // Nothing when the whole history is inside the kept rounds.
        assert_eq!(plan_stub(&messages, &stub_config(8, 1)), None);
        assert_eq!(plan_stub(&messages, &stub_config(9, 1)), None);
    }

    #[test]
    fn an_aged_out_block_too_small_to_pay_a_cache_break_waits() {
        // 5 rounds, 3 kept: 2 old results — below a block of 4, no pass at all.
        let messages = rounds(5, 200);
        assert_eq!(plan_stub(&messages, &stub_config(3, 4)), None);
        // The same history with a block of 2 is worth one cache break.
        assert_eq!(
            plan_stub(&messages, &stub_config(3, 2))
                .expect("a plan")
                .indices,
            vec![2, 4]
        );
        // A block of 0 is read as 1: never a division by nothing, never a free pass.
        assert_eq!(
            plan_stub(&messages, &stub_config(3, 0))
                .expect("a plan")
                .len(),
            2
        );
    }

    #[test]
    fn a_stubbed_result_is_never_stubbed_again_and_each_block_pays_once() {
        let mut messages = rounds(8, 200);
        let config = stub_config(3, 4);
        let plan = plan_stub(&messages, &config).expect("a plan");
        assert_eq!(apply_stub(&mut messages, &plan), 5);
        // Once applied, nothing is left to elide: a second pass is a no-op, so a
        // history that has already paid its cache break never pays another for it.
        assert_eq!(plan_stub(&messages, &config), None);
        assert_eq!(apply_stub(&mut messages, &plan), 0);
        // Four more rounds age four FRESH results out: exactly one new block, the stubs
        // of the first one untouched (they are not in the plan, so not rewritten).
        messages.extend(rounds_from(8, 4, 200));
        let next = plan_stub(&messages, &config).expect("a second block");
        assert_eq!(next.indices, vec![12, 14, 16, 18]);
        assert_eq!(apply_stub(&mut messages, &next), 4);
    }

    #[test]
    fn only_the_text_goes_and_the_pairing_survives() {
        let mut messages = rounds(8, 200);
        let before = messages.clone();
        let plan = plan_stub(&messages, &stub_config(3, 4)).expect("a plan");
        apply_stub(&mut messages, &plan);
        // Same messages, same roles, same order: only the text of the old results moved.
        assert_eq!(messages.len(), before.len());
        for (index, (now, was)) in messages.iter().zip(&before).enumerate() {
            assert_eq!(now.role, was.role, "message {index}");
            assert_eq!(now.tool_call_id, was.tool_call_id, "message {index}");
            assert_eq!(now.tool_calls, was.tool_calls, "message {index}");
            assert_eq!(now.reasoning, was.reasoning, "message {index}");
            let touched = now.role == Role::Tool && now.content != was.content;
            if touched {
                // A touched result is a stub and nothing else.
                assert_eq!(now.content.as_deref(), Some(STUB_MARKER), "message {index}");
            } else {
                assert_eq!(now.content, was.content, "message {index}");
            }
        }
        assert!(is_stub(messages[2].content.as_deref()));
        // The result of a kept round is whole, and so is its call's reasoning.
        assert!(!is_stub(messages[16].content.as_deref()));
        assert_eq!(
            messages[15].reasoning.as_deref(),
            Some("round 7 needs the file")
        );
    }

    #[test]
    fn the_results_of_one_parallel_round_age_together() {
        let messages = vec![
            ChatMessage::user("go"),
            ChatMessage::assistant_tool_calls(vec![
                ToolCallChunk {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                },
                ToolCallChunk {
                    id: "b".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            ]),
            ChatMessage::tool("a", "x".repeat(100)),
            ChatMessage::tool("b", "y".repeat(100)),
            ChatMessage::assistant_tool_calls(vec![ToolCallChunk {
                id: "c".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }]),
            ChatMessage::tool("c", "z".repeat(100)),
        ];
        // One kept round: the boundary is its assistant message, so both results of the
        // parallel round are old together — never one stubbed and its twin whole.
        assert_eq!(kept_rounds_from(&messages, 1), Some(4));
        assert_eq!(
            plan_stub(&messages, &stub_config(1, 2))
                .expect("a plan")
                .indices,
            vec![2, 3]
        );
        // Two kept rounds: the parallel round itself is kept, nothing to do.
        assert_eq!(plan_stub(&messages, &stub_config(2, 1)), None);
        assert_eq!(kept_rounds_from(&messages, 2), Some(1));
    }

    #[test]
    fn the_cheap_tier_needs_a_known_window_and_obeys_the_compaction_switch() {
        let config = CompactionConfig::default();
        // No window: no pressure to relieve, and nothing to relieve it against.
        assert!(!should_stub(&config, None, 1_000_000));
        // Under half the window the session is left alone (its cache stays whole).
        assert!(!should_stub(&config, Some(1000), 499));
        assert!(should_stub(&config, Some(1000), 500));
        // A stub pass happens well before the summary, never instead of it by accident.
        assert!(config.stub_ratio < config.threshold_ratio);
        assert!(should_stub(&config, Some(1000), 700));
        // Switched off, or without a kept window of rounds: nothing is touched.
        let off = CompactionConfig {
            enabled: false,
            ..config
        };
        assert!(!should_stub(&off, Some(1000), 5000));
        assert_eq!(plan_stub(&rounds(8, 10), &stub_config(0, 1)), None);
    }
}
