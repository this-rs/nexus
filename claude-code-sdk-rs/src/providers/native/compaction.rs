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
//! **Images** of the kept tail stay whole. Those of the summarised part are
//! rendered as `[image <media type>]` in the summarisation prompt (never the
//! pixels: the summary is text) and the loop says so with
//! `provider_notice { images_compacted }`; nothing is lost in silence.

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

/// When and how much to compact.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionConfig {
    /// Automatic compaction on or off.
    pub enabled: bool,
    /// Fraction of the context window at which compaction starts.
    pub threshold_ratio: f64,
    /// Number of last messages kept intact.
    pub keep_recent: usize,
    /// Output cap of the summarisation call, in tokens.
    pub summary_max_tokens: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_ratio: 0.8,
            keep_recent: 6,
            summary_max_tokens: 2048,
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
pub async fn summarise(
    endpoint: &dyn ModelEndpoint,
    request: CompletionRequest,
) -> Result<(String, Option<Usage>), ProviderError> {
    let mut stream = endpoint.complete(request).await?;
    let mut text = String::new();
    let mut usage = None;
    while let Some(chunk) = stream.next().await {
        match chunk? {
            CompletionChunk::Text(delta) => text.push_str(&delta),
            CompletionChunk::Usage(reported) => usage = Some(reported),
            _ => {},
        }
    }
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err(ProviderError::protocol(
            "the compaction summary came back empty",
        ));
    }
    Ok((text, usage))
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
}
