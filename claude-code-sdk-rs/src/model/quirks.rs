//! Per-instance dialect flags (contract §12, decision A39).
//!
//! "OpenAI-compatible" servers differ in small ways that break an agent loop.
//! The differences are data, not code paths: a flag set per instance, with presets
//! for the known servers. The flags act on both directions: building the request
//! JSON (`wire::build_request`) and reading the stream (`wire::StreamParser`).

use serde::{Deserialize, Serialize};

/// Name of the JSON field that carries the model's reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReasoningField {
    /// `reasoning_content` (DeepSeek, llama-server, NIM, Qwen via vLLM <= 0.9).
    #[default]
    ReasoningContent,
    /// `reasoning` (vLLM recent versions, OpenRouter).
    Reasoning,
}

impl ReasoningField {
    /// The JSON key.
    pub fn key(self) -> &'static str {
        match self {
            Self::ReasoningContent => "reasoning_content",
            Self::Reasoning => "reasoning",
        }
    }

    /// The other spelling, tried as a fallback when reading.
    pub fn other(self) -> Self {
        match self {
            Self::ReasoningContent => Self::Reasoning,
            Self::Reasoning => Self::ReasoningContent,
        }
    }
}

/// Dialect flags of one endpoint instance.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointQuirks {
    /// Send back the `reasoning` of assistant messages (under [`Self::reasoning_field`])
    /// when the request offers tools, and only then. DeepSeek rejects a
    /// tool-calling transcript that dropped it.
    pub echo_reasoning_with_tools: bool,
    /// Never send `tool_choice` (Ollama's compatibility layer rejects or ignores it).
    pub omit_tool_choice: bool,
    /// Never FORCE a tool (`tool_choice` naming a function): send `auto` instead.
    /// DeepSeek V4 thinks by default and answers a forced or `required` choice
    /// with HTTP 400 (\"Thinking mode does not support this tool_choice\").
    pub no_forced_tool_choice: bool,
    /// Value of `parallel_tool_calls` to send when the request does not choose one
    /// and offers tools; `None` sends nothing.
    pub explicit_parallel_tool_calls: Option<bool>,
    /// Field carrying the reasoning, on the way in and on the way out. Reading also
    /// accepts the other spelling.
    pub reasoning_field: ReasoningField,
    /// Move system messages that come after the first non-system message into the
    /// leading system message (chat templates that refuse a late `system` role).
    pub fold_late_system: bool,
    /// Send assistant tool-call `arguments` as a JSON object instead of a string.
    pub tool_args_as_object: bool,
}

impl EndpointQuirks {
    /// Names accepted by [`EndpointQuirks::preset`].
    pub const PRESETS: [&'static str; 6] = [
        "deepseek",
        "vllm",
        "ollama",
        "llama_server",
        "nim",
        "generic",
    ];

    /// No quirk: plain OpenAI behaviour.
    pub fn generic() -> Self {
        Self::default()
    }

    /// DeepSeek: reasoning must come back with tool calls, and thinking mode
    /// (the default) refuses a forced `tool_choice`.
    pub fn deepseek() -> Self {
        Self {
            echo_reasoning_with_tools: true,
            no_forced_tool_choice: true,
            ..Self::default()
        }
    }

    /// vLLM: `reasoning` field; strict chat templates.
    pub fn vllm() -> Self {
        Self {
            reasoning_field: ReasoningField::Reasoning,
            fold_late_system: true,
            ..Self::default()
        }
    }

    /// Ollama: no `tool_choice`; strict chat templates.
    pub fn ollama() -> Self {
        Self {
            omit_tool_choice: true,
            fold_late_system: true,
            ..Self::default()
        }
    }

    /// llama-server: explicit `parallel_tool_calls: false`; strict chat templates.
    pub fn llama_server() -> Self {
        Self {
            explicit_parallel_tool_calls: Some(false),
            fold_late_system: true,
            ..Self::default()
        }
    }

    /// NVIDIA NIM: explicit `parallel_tool_calls: false`.
    pub fn nim() -> Self {
        Self {
            explicit_parallel_tool_calls: Some(false),
            ..Self::default()
        }
    }

    /// Preset by name (case-insensitive, `-` accepted for `_`); `None` when unknown.
    pub fn preset(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "deepseek" => Some(Self::deepseek()),
            "vllm" => Some(Self::vllm()),
            "ollama" => Some(Self::ollama()),
            "llama_server" | "llamacpp" | "llama.cpp" => Some(Self::llama_server()),
            "nim" => Some(Self::nim()),
            "generic" | "openai" => Some(Self::generic()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_preset_resolves() {
        for name in EndpointQuirks::PRESETS {
            assert!(EndpointQuirks::preset(name).is_some(), "{name}");
        }
        assert!(EndpointQuirks::preset("nope").is_none());
        assert_eq!(
            EndpointQuirks::preset("Llama-Server"),
            Some(EndpointQuirks::llama_server())
        );
    }

    #[test]
    fn presets_carry_their_documented_flags() {
        assert!(EndpointQuirks::deepseek().echo_reasoning_with_tools);
        assert!(EndpointQuirks::deepseek().no_forced_tool_choice);
        assert!(!EndpointQuirks::generic().no_forced_tool_choice);
        assert!(EndpointQuirks::ollama().omit_tool_choice);
        assert_eq!(
            EndpointQuirks::vllm().reasoning_field,
            ReasoningField::Reasoning
        );
        assert_eq!(
            EndpointQuirks::nim().explicit_parallel_tool_calls,
            Some(false)
        );
        assert_eq!(EndpointQuirks::generic(), EndpointQuirks::default());
        assert!(!EndpointQuirks::generic().echo_reasoning_with_tools);
    }

    #[test]
    fn quirks_round_trip_and_default_missing_fields() {
        let quirks: EndpointQuirks = serde_json::from_str(r#"{"omit_tool_choice":true}"#).unwrap();
        assert!(quirks.omit_tool_choice);
        assert_eq!(quirks.reasoning_field, ReasoningField::ReasoningContent);
        let json = serde_json::to_string(&EndpointQuirks::vllm()).unwrap();
        assert_eq!(
            serde_json::from_str::<EndpointQuirks>(&json).unwrap(),
            EndpointQuirks::vllm()
        );
    }
}
