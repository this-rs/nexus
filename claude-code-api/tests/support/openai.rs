//! One-liners for the OpenAI request/response shapes in `models::openai`.

use claude_code_api::models::openai::{
    ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ContentPart,
    FunctionDefinition, ImageUrl, MessageContent, Tool, Usage,
};
use serde_json::Value;

pub const TEST_MODEL: &str = "claude-sonnet-5";

/// `{"role": "user", "content": text}`
pub fn user(text: &str) -> ChatMessage {
    message("user", text)
}

/// `{"role": "assistant", "content": text}`
pub fn assistant(text: &str) -> ChatMessage {
    message("assistant", text)
}

/// `{"role": "system", "content": text}`
pub fn system(text: &str) -> ChatMessage {
    message("system", text)
}

pub fn message(role: &str, text: &str) -> ChatMessage {
    ChatMessage {
        role: role.to_string(),
        content: Some(MessageContent::Text(text.to_string())),
        name: None,
        tool_calls: None,
    }
}

/// A message with no content at all — legal for tool-call turns.
pub fn contentless(role: &str) -> ChatMessage {
    ChatMessage {
        role: role.to_string(),
        content: None,
        name: None,
        tool_calls: None,
    }
}

/// A multimodal user message: text parts plus `image_url` parts.
pub fn multimodal(role: &str, texts: &[&str], image_urls: &[&str]) -> ChatMessage {
    let mut parts: Vec<ContentPart> = texts
        .iter()
        .map(|t| ContentPart::Text {
            text: (*t).to_string(),
        })
        .collect();
    parts.extend(image_urls.iter().map(|u| ContentPart::ImageUrl {
        image_url: ImageUrl {
            url: (*u).to_string(),
            detail: None,
        },
    }));
    ChatMessage {
        role: role.to_string(),
        content: Some(MessageContent::Array(parts)),
        name: None,
        tool_calls: None,
    }
}

/// An OpenAI function tool declaration.
pub fn tool(name: &str, description: &str, parameters: Value) -> Tool {
    Tool {
        tool_type: "function".to_string(),
        function: FunctionDefinition {
            name: name.to_string(),
            description: Some(description.to_string()),
            parameters,
        },
    }
}

/// The shortest useful chat-completion request: one user turn, non-streaming.
pub fn chat_request(text: &str) -> ChatCompletionRequest {
    request().message(user(text)).build()
}

/// The same, streaming.
pub fn streaming_request(text: &str) -> ChatCompletionRequest {
    request().message(user(text)).stream(true).build()
}

pub fn request() -> ChatRequest {
    ChatRequest {
        inner: ChatCompletionRequest {
            model: TEST_MODEL.to_string(),
            ..Default::default()
        },
    }
}

/// Builder over [`ChatCompletionRequest`].
pub struct ChatRequest {
    inner: ChatCompletionRequest,
}

impl ChatRequest {
    pub fn model(mut self, model: &str) -> Self {
        self.inner.model = model.to_string();
        self
    }

    pub fn message(mut self, message: ChatMessage) -> Self {
        self.inner.messages.push(message);
        self
    }

    pub fn messages(mut self, messages: Vec<ChatMessage>) -> Self {
        self.inner.messages.extend(messages);
        self
    }

    pub fn stream(mut self, stream: bool) -> Self {
        self.inner.stream = Some(stream);
        self
    }

    pub fn conversation_id(mut self, id: &str) -> Self {
        self.inner.conversation_id = Some(id.to_string());
        self
    }

    pub fn tools(mut self, tools: Vec<Tool>) -> Self {
        self.inner.tools = Some(tools);
        self
    }

    pub fn build(self) -> ChatCompletionRequest {
        self.inner
    }
}

/// The assistant text of the first choice, if any.
pub fn first_text(response: &ChatCompletionResponse) -> Option<String> {
    text_of(&first_choice(response)?.message)
}

pub fn first_choice(response: &ChatCompletionResponse) -> Option<&ChatChoice> {
    response.choices.first()
}

pub fn text_of(message: &ChatMessage) -> Option<String> {
    match message.content.as_ref()? {
        MessageContent::Text(text) => Some(text.clone()),
        MessageContent::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.clone()),
                    ContentPart::ImageUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        ),
    }
}

/// A minimal response value, for cache and storage tests.
pub fn response(id: &str, model: &str) -> ChatCompletionResponse {
    ChatCompletionResponse {
        id: id.to_string(),
        object: "chat.completion".to_string(),
        created: 0,
        model: model.to_string(),
        choices: vec![ChatChoice {
            index: 0,
            message: assistant("cached"),
            finish_reason: Some("stop".to_string()),
        }],
        usage: Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
        conversation_id: None,
    }
}
