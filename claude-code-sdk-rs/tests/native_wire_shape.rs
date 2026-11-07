//! GEM-400 — the exact JSON the native harness puts on the wire for a turn that
//! contains a tool call followed by its result (the shape Google's OpenAI-compatible
//! layer refused with a 400 when the assistant message carried `"content": null`).
//!
//! A real `NativeProvider` + `OpenAiEndpoint` drive a session through the whole
//! round trip (model asks for a tool, the tool runs, the result goes back) against
//! the scripted `fake_openai`; the SECOND chat request is read back as sent. The
//! same turn is replayed under every dialect that was already accepted, and each
//! is pinned byte for byte: the only thing a dialect may change is what its quirks
//! declare. No network beyond 127.0.0.1, no Google.

#[path = "support/fake_openai.rs"]
mod fake_openai;
#[path = "support/native.rs"]
mod native;

use std::sync::Arc;

use fake_openai::FakeOpenAi;
use native::*;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, ReasoningEffort, SessionHooks, SessionSpec,
    StopReason, SystemPromptMode, SystemPromptSpec, TurnContext, TurnDirective, TurnInput,
};
use nexus_claude::model::EndpointQuirks;
use nexus_claude::providers::native::{MemoryTranscriptStore, NativeConfig, NativeProvider};
use serde_json::{Value, json};

/// Runs "look" → tool call (no text at all) → tool result → final text, and returns
/// the body of the request that carries the tool result back to the model.
async fn wire_of_the_tool_round(quirks: EndpointQuirks) -> Value {
    wire_of_the_tool_round_with(quirks, None).await
}

/// The same round, the session opened with `hooks`.
async fn wire_of_the_tool_round_with(
    quirks: EndpointQuirks,
    hooks: Option<Arc<dyn SessionHooks>>,
) -> Value {
    let server = FakeOpenAi::start(json!([
        probe_route(false),
        models_route(128_000),
        tool_reply(
            Some("look"),
            &[("c1", "mcp__fake__echo", json!({"text": "hi"}))],
            None,
            None
        ),
        text_reply(Some("\"role\":\"tool\""), "done", None, None),
    ]));
    let mut config = NativeConfig::new("native-test");
    config.default_model = Some("m".to_owned());
    let provider = NativeProvider::new(config, endpoint(server.base_url(), quirks))
        .with_transcript_store(Arc::new(MemoryTranscriptStore::new()));
    let cwd = tempfile::tempdir().unwrap();
    let mut spec = SessionSpec::new(cwd.path());
    spec.model = Some("m".to_owned());
    spec.mcp_servers.insert("fake".to_owned(), stdio_mcp(None));
    spec.system_prompt = Some(SystemPromptSpec {
        text: "be brief".to_owned(),
        mode: SystemPromptMode::Replace,
    });
    spec.hooks = hooks;
    let session: Arc<dyn AgentSession> = provider.open(spec).await.expect("open");
    let events = collect(
        session
            .send_turn(TurnInput::text("look"))
            .await
            .expect("send_turn"),
    )
    .await;
    assert!(
        matches!(
            terminal(&events),
            AgentEvent::Done {
                stop_reason: StopReason::Completed,
                ..
            }
        ),
        "{events:?}"
    );
    let chat: Vec<Value> = server
        .requests_to("POST", "/v1/chat/completions")
        .into_iter()
        .map(|r| r["body"].clone())
        .filter(|body| !body.to_string().contains("Call the ping tool now"))
        .collect();
    assert_eq!(chat.len(), 2, "one request per model round: {chat:?}");
    chat[1].clone()
}

/// The four messages every dialect sends back for this turn. `assistant_extra` is
/// what a dialect adds to the assistant message (DeepSeek's reasoning echo).
fn expected_messages(assistant_extra: Option<(&str, &str)>) -> Value {
    let mut assistant = json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [{
            "id": "c1",
            "type": "function",
            "function": {"name": "mcp__fake__echo", "arguments": "{\"text\":\"hi\"}"}
        }]
    });
    if let Some((key, value)) = assistant_extra {
        assistant[key] = json!(value);
    }
    json!([
        {"role": "system", "content": "be brief"},
        {"role": "user", "content": "look"},
        assistant,
        {"role": "tool", "tool_call_id": "c1", "content": "echo: hi"}
    ])
}

/// Top-level keys of the request body, sorted, `tools` included.
fn top_level_keys(body: &Value) -> Vec<String> {
    let mut keys: Vec<String> = body.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

/// The rules Google's OpenAI-compatible layer is documented to enforce on a
/// tool round, as far as this offline test can state them: no `null` content on
/// any message, an assistant `content` that is a string, every `tool` message
/// answering a `tool_calls` id of the message before it through `tool_call_id`,
/// and no field outside the OpenAI chat schema on a message.
fn assert_openai_strict(body: &Value) {
    const MESSAGE_KEYS: [&str; 5] = [
        "role",
        "content",
        "tool_calls",
        "tool_call_id",
        "reasoning_content",
    ];
    let messages = body["messages"].as_array().unwrap();
    let mut open: Vec<String> = Vec::new();
    for message in messages {
        for key in message.as_object().unwrap().keys() {
            assert!(MESSAGE_KEYS.contains(&key.as_str()), "unknown field {key}");
        }
        assert!(!message["content"].is_null(), "null content: {message}");
        match message["role"].as_str().unwrap() {
            "assistant" => {
                assert!(message["content"].is_string(), "{message}");
                open = message["tool_calls"]
                    .as_array()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|c| c["id"].as_str().unwrap().to_owned())
                            .collect()
                    })
                    .unwrap_or_default();
            },
            "tool" => {
                let id = message["tool_call_id"].as_str().expect("tool_call_id");
                assert!(open.contains(&id.to_owned()), "orphan tool result {id}");
                // `name` is optional in the OpenAI schema and Google resolves the
                // function from `tool_call_id`: we send none, on purpose.
                assert!(message.get("name").is_none());
            },
            _ => {},
        }
    }
}

const BASE_KEYS: [&str; 6] = [
    "messages",
    "model",
    "stream",
    "stream_options",
    "tool_choice",
    "tools",
];

fn keys_of(extra: &[&str], without: &[&str]) -> Vec<String> {
    let mut keys: Vec<String> = BASE_KEYS
        .iter()
        .chain(extra)
        .filter(|key| !without.contains(key))
        .map(|key| (*key).to_owned())
        .collect();
    keys.sort();
    keys
}

/// GEM-400, criterion 3: the exact shape of a tool call followed by its result.
/// Generic = what Google receives (it has no preset: plain OpenAI dialect).
#[tokio::test]
async fn a_tool_call_without_text_and_its_result_go_out_with_an_empty_string_content() {
    let body = wire_of_the_tool_round(EndpointQuirks::generic()).await;
    assert_eq!(body["messages"], expected_messages(None));
    assert_eq!(top_level_keys(&body), keys_of(&[], &[]));
    assert_eq!(body["model"], "m");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(body["tool_choice"], "auto");
    // Each tool is {type, function:{name, description, parameters}} and nothing else.
    for tool in body["tools"].as_array().unwrap() {
        assert_eq!(top_level_keys(tool), ["function", "type"]);
        assert_eq!(tool["type"], "function");
        assert_eq!(
            top_level_keys(&tool["function"]),
            ["description", "name", "parameters"]
        );
    }
    assert_openai_strict(&body);
}

/// GEM-400, criterion 4: the dialects that were already accepted keep their
/// shape. The messages are identical but for what the quirks declare, and the top
/// level differs only by the declared flags. (The one thing that moved for all of
/// them, `content: null` → `""` on a text-less tool call, is the OpenAI-schema-valid
/// form; the assertion on the assistant message pins it for each dialect.)
#[tokio::test]
async fn every_accepted_dialect_keeps_its_shape_on_the_same_tool_round() {
    type Case = (
        &'static str,
        Option<(&'static str, &'static str)>,
        Vec<String>,
    );
    let cases: Vec<Case> = vec![
        (
            "deepseek",
            Some(("reasoning_content", "")),
            keys_of(&[], &[]),
        ),
        ("vllm", None, keys_of(&[], &[])),
        ("ollama", None, keys_of(&[], &["tool_choice"])),
        ("llama_server", None, keys_of(&["parallel_tool_calls"], &[])),
        ("nim", None, keys_of(&["parallel_tool_calls"], &[])),
        ("generic", None, keys_of(&[], &[])),
    ];
    assert_eq!(cases.len(), EndpointQuirks::PRESETS.len());
    for (name, extra, keys) in cases {
        let body = wire_of_the_tool_round(EndpointQuirks::preset(name).unwrap()).await;
        assert_eq!(body["messages"], expected_messages(extra), "{name}");
        assert_eq!(top_level_keys(&body), keys, "{name}");
        match name {
            "llama_server" | "nim" => assert_eq!(body["parallel_tool_calls"], false, "{name}"),
            "ollama" => assert!(body.get("tool_choice").is_none()),
            _ => {},
        }
        assert_openai_strict(&body);
    }
}

/// Hooks whose `before_turn` asks the effort given, or nothing.
struct EffortHooks(Option<ReasoningEffort>);

#[async_trait::async_trait]
impl SessionHooks for EffortHooks {
    async fn before_turn(&self, _ctx: &TurnContext) -> TurnDirective {
        match self.0 {
            Some(effort) => TurnDirective::reasoning_effort(effort),
            None => TurnDirective::none(),
        }
    }
}

/// Contract v10: a turn whose hook asks no effort puts EXACTLY the body of today on the
/// wire, byte for byte, under every dialect; one that asks an effort adds the one key
/// `reasoning_effort` and nothing else (DeepSeek: nothing at all).
#[tokio::test]
async fn a_turn_without_an_effort_is_byte_identical_and_one_with_adds_the_single_key() {
    for name in EndpointQuirks::PRESETS {
        let quirks = EndpointQuirks::preset(name).unwrap();
        let today = wire_of_the_tool_round(quirks.clone()).await;
        let silent =
            wire_of_the_tool_round_with(quirks.clone(), Some(Arc::new(EffortHooks(None)))).await;
        assert_eq!(silent.to_string(), today.to_string(), "{name}");
        let asked = wire_of_the_tool_round_with(
            quirks.clone(),
            Some(Arc::new(EffortHooks(Some(ReasoningEffort::Medium)))),
        )
        .await;
        if name == "deepseek" {
            assert_eq!(asked.to_string(), today.to_string(), "{name}");
            continue;
        }
        assert_eq!(asked["reasoning_effort"], "medium", "{name}");
        let mut stripped = asked.clone();
        stripped.as_object_mut().unwrap().remove("reasoning_effort");
        assert_eq!(stripped.to_string(), today.to_string(), "{name}");
        assert_openai_strict(&asked);
    }
}
