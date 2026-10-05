//! Recording, normalisation and replay of turns.
//!
//! A [`Transcript`] is what a provider emitted, turn by turn, as canonical JSON:
//! pretty-printed, object keys sorted, one trailing newline. [`normalize`]
//! replaces what changes from one run to the next (identifiers, durations, the
//! working directory), so that two recordings of the same scenario are identical
//! byte for byte. [`Script::from_transcript`](super::Script::from_transcript)
//! turns a transcript back into something a
//! [`ScriptedProvider`](super::ScriptedProvider) plays.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::agent::{
    AgentEvent, AgentSession, CONTRACT_VERSION, EventStream, PermissionDecision, PermissionScope,
    ProviderError, ProviderKind, TurnInput,
};

/// What a provider emitted during a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// [`CONTRACT_VERSION`] the events were recorded under.
    pub contract_version: u32,
    /// Family of the provider recorded.
    pub provider_kind: ProviderKind,
    /// Events of each turn, in order, terminal event included.
    pub turns: Vec<Vec<AgentEvent>>,
    /// Events received on the out-of-band stream.
    #[serde(default)]
    pub out_of_band: Vec<AgentEvent>,
}

impl Transcript {
    /// An empty transcript for a provider family, at the current contract version.
    pub fn new(provider_kind: ProviderKind) -> Self {
        Self {
            contract_version: CONTRACT_VERSION,
            provider_kind,
            turns: Vec::new(),
            out_of_band: Vec::new(),
        }
    }

    /// The canonical JSON form: pretty-printed, keys sorted at every depth, one
    /// trailing newline. Equal transcripts give equal bytes.
    pub fn to_json_string(&self) -> String {
        // A transcript is made of plain data: it always serialises.
        let value = serde_json::to_value(self).unwrap_or(Value::Null);
        let mut text = serde_json::to_string_pretty(&sort_keys(value)).unwrap_or_default();
        text.push('\n');
        text
    }

    /// Parses a transcript. Refuses one recorded under another contract version:
    /// its events may not mean the same thing.
    pub fn from_json_str(text: &str) -> Result<Self, ProviderError> {
        let transcript: Self = serde_json::from_str(text)
            .map_err(|error| ProviderError::invalid(format!("malformed transcript: {error}")))?;
        if transcript.contract_version != CONTRACT_VERSION {
            return Err(ProviderError::invalid(format!(
                "transcript recorded under contract version {}, this is version {CONTRACT_VERSION}",
                transcript.contract_version
            )));
        }
        Ok(transcript)
    }

    /// Writes the canonical JSON form to `path`.
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        std::fs::write(path, self.to_json_string())
    }

    /// Reads a transcript written by [`Transcript::save`].
    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::from_json_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }
}

/// Rebuilds a value with the keys of every object in lexicographic order, so the
/// output does not depend on how `serde_json` was built.
fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sort_keys(value)))
                    .collect::<Map<String, Value>>(),
            )
        },
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// Reads a turn stream to its end and returns everything it carried.
///
/// The stream is read until it **closes**, not until its terminal event, so that
/// a provider emitting after the terminal event is recorded as it behaved. The
/// caller bounds the wait (`tokio::time::timeout`) and answers the permission
/// requests from another task, or uses [`Recorder::drive_turn`].
pub async fn record_turn(stream: EventStream) -> Vec<AgentEvent> {
    stream.collect().await
}

/// Accumulates the turns of a session into a [`Transcript`].
#[derive(Debug, Clone)]
pub struct Recorder {
    transcript: Transcript,
}

impl Recorder {
    /// A recorder for a provider family.
    pub fn new(provider_kind: ProviderKind) -> Self {
        Self {
            transcript: Transcript::new(provider_kind),
        }
    }

    /// Records a turn from its stream; see [`record_turn`].
    pub async fn record_turn(&mut self, stream: EventStream) -> Vec<AgentEvent> {
        let events = record_turn(stream).await;
        self.transcript.turns.push(events.clone());
        events
    }

    /// Sends a turn and records it, allowing every permission request met on the
    /// way (scope `once` when offered, else the first scope offered).
    pub async fn drive_turn(
        &mut self,
        session: &dyn AgentSession,
        input: TurnInput,
    ) -> Result<Vec<AgentEvent>, ProviderError> {
        let mut stream = session.send_turn(input).await?;
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            if let AgentEvent::PermissionAsk {
                request_id, scopes, ..
            } = &event
            {
                let scope = if scopes.is_empty() || scopes.contains(&PermissionScope::Once) {
                    PermissionScope::Once
                } else {
                    scopes[0]
                };
                let decision = PermissionDecision::Allow {
                    scope,
                    updated_input: None,
                };
                session.answer_permission(request_id, decision).await?;
            }
            events.push(event);
        }
        self.transcript.turns.push(events.clone());
        Ok(events)
    }

    /// Adds events received on the out-of-band stream.
    pub fn push_out_of_band(&mut self, events: impl IntoIterator<Item = AgentEvent>) {
        self.transcript.out_of_band.extend(events);
    }

    /// The transcript recorded so far.
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// Ends the recording.
    pub fn finish(self) -> Transcript {
        self.transcript
    }
}

/// Stable placeholders for the identifiers met in a transcript.
#[derive(Default)]
struct Placeholders {
    by_raw: HashMap<String, String>,
    counters: HashMap<&'static str, usize>,
}

impl Placeholders {
    fn assign(&mut self, class: &'static str, raw: Option<&Value>) {
        let Some(raw) = raw.and_then(Value::as_str) else {
            return;
        };
        if raw.is_empty() || self.by_raw.contains_key(raw) {
            return;
        }
        let counter = self.counters.entry(class).or_insert(0);
        *counter += 1;
        self.by_raw
            .insert(raw.to_owned(), format!("<{class}-{counter}>"));
    }
}

fn background_tasks(event: &Value) -> &[Value] {
    event
        .get("tasks")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Replaces what changes from one run to the next, in place.
///
/// - Identifiers become stable placeholders numbered by first appearance (turns
///   first, then the out-of-band events): `provider_session_id` → `<session-n>`,
///   `request_id` → `<req-n>`, the `id` of a tool call and every `tool_call_id` →
///   `<tool-n>`, `question_id` → `<question-n>`, `task_id` and the `id` of a
///   background task → `<task-n>`, `event_id` → `<event-n>`, a `parent` that is
///   not a tool call → `<thread-n>`. The correspondences are kept: a
///   `tool_result.id` points to the same `<tool-n>` as its `tool_call`, and an
///   identifier repeated inside a payload (`input`, `data`) is replaced too.
/// - `duration_ms`, `duration_api_ms`, `started_at_ms` and `pid` become 0.
/// - The working directory announced by `session_started.cwd` becomes `<cwd>`
///   wherever it appears in a string.
///
/// Normalising twice gives the same result as normalising once.
pub fn normalize(transcript: &mut Transcript) {
    normalize_inner(transcript, None);
}

/// Like [`normalize`], also replacing `cwd` (for providers that never announce
/// their working directory).
pub fn normalize_with_cwd(transcript: &mut Transcript, cwd: &str) {
    normalize_inner(transcript, Some(cwd));
}

fn normalize_inner(transcript: &mut Transcript, cwd: Option<&str>) {
    let mut events: Vec<Value> = transcript
        .turns
        .iter()
        .flatten()
        .chain(&transcript.out_of_band)
        .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
        .collect();

    // Tool calls first: a `parent` naming a tool call must resolve to `<tool-n>`
    // even when it is met before the call in the recording.
    let mut ids = Placeholders::default();
    for event in &events {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "tool_call" || kind == "tool_result" {
            ids.assign("tool", event.get("id"));
        }
        ids.assign("tool", event.get("tool_call_id"));
        for task in background_tasks(event) {
            ids.assign("tool", task.get("tool_call_id"));
        }
    }
    let mut cwds: Vec<String> = cwd.map(str::to_owned).into_iter().collect();
    for event in &events {
        ids.assign("session", event.get("provider_session_id"));
        ids.assign("req", event.get("request_id"));
        ids.assign("question", event.get("question_id"));
        ids.assign("task", event.get("task_id"));
        ids.assign("event", event.get("event_id"));
        for task in background_tasks(event) {
            ids.assign("task", task.get("id"));
            ids.assign("thread", task.get("parent"));
        }
        ids.assign("thread", event.get("parent"));
        if event.get("type").and_then(Value::as_str) == Some("session_started")
            && let Some(announced) = event.get("cwd").and_then(Value::as_str)
        {
            cwds.push(announced.to_owned());
        }
    }
    // Longest first, so a directory is replaced before one of its ancestors. A
    // one-character path (`/`) would match everywhere: left alone.
    cwds.retain(|path| path.len() > 1 && path != "<cwd>");
    cwds.sort_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));
    cwds.dedup();

    for event in &mut events {
        rewrite_strings(event, &ids.by_raw, &cwds);
        zero_volatile_numbers(event);
    }

    let rewritten = events.into_iter();
    let slots = transcript
        .turns
        .iter_mut()
        .flatten()
        .chain(&mut transcript.out_of_band);
    for (slot, value) in slots.zip(rewritten) {
        // A rewritten event has the same shape; should it not parse, the
        // original is kept rather than lost.
        if let Ok(event) = serde_json::from_value(value) {
            *slot = event;
        }
    }
}

fn rewrite_strings(value: &mut Value, ids: &HashMap<String, String>, cwds: &[String]) {
    match value {
        Value::String(text) => {
            if let Some(placeholder) = ids.get(text.as_str()) {
                *text = placeholder.clone();
                return;
            }
            for cwd in cwds {
                if text.contains(cwd.as_str()) {
                    *text = text.replace(cwd.as_str(), "<cwd>");
                }
            }
        },
        Value::Array(items) => {
            for item in items {
                rewrite_strings(item, ids, cwds);
            }
        },
        Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                rewrite_strings(item, ids, cwds);
            }
        },
        _ => {},
    }
}

fn zero_field(object: &mut Value, key: &str) {
    if let Some(slot) = object.get_mut(key)
        && slot.is_number()
    {
        *slot = Value::from(0u64);
    }
}

fn zero_volatile_numbers(event: &mut Value) {
    zero_field(event, "duration_ms");
    zero_field(event, "duration_api_ms");
    if let Some(tasks) = event.get_mut("tasks").and_then(Value::as_array_mut) {
        for task in tasks {
            zero_field(task, "started_at_ms");
            zero_field(task, "pid");
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::{
        BackgroundTask, BackgroundTaskKind, BackgroundTaskStatus, Cost, StopReason, TaskPhase,
        ToolCategory, ToolOutput, Usage,
    };

    fn sample(suffix: &str, cwd: &str, duration_ms: u64) -> Transcript {
        let tool = format!("toolu_{suffix}");
        let mut transcript = Transcript::new(ProviderKind::ClaudeCode);
        transcript.turns.push(vec![
            AgentEvent::SessionStarted {
                provider_session_id: Some(format!("sess-{suffix}")),
                model: Some("model-a".into()),
                policy_mode: None,
                native_mode: None,
                tools: vec!["Read".into()],
                mcp_servers: Vec::new(),
                cwd: Some(cwd.into()),
            },
            AgentEvent::Text {
                text: "child speaks first".into(),
                seq: Some(1),
                parent: Some(tool.clone()),
            },
            AgentEvent::ToolCall {
                id: tool.clone(),
                name: "Read".into(),
                input: json!({ "file_path": format!("{cwd}/src/lib.rs"), "self": tool }),
                category: ToolCategory::Read,
                canonical: None,
                input_complete: true,
                seq: Some(2),
                parent: None,
            },
            AgentEvent::PermissionAsk {
                request_id: format!("req-{suffix}"),
                tool_name: "Read".into(),
                input: json!({}),
                category: ToolCategory::Read,
                canonical: None,
                tool_call_id: Some(tool.clone()),
                scopes: vec![PermissionScope::Once],
                parent: None,
            },
            AgentEvent::ToolResult {
                id: tool.clone(),
                output: Some(ToolOutput::Text(format!("read {cwd}/src/lib.rs"))),
                is_error: false,
                seq: Some(3),
                parent: None,
            },
            AgentEvent::TaskUpdate {
                phase: TaskPhase::Started,
                task_id: Some(format!("task-{suffix}")),
                tool_call_id: Some(tool.clone()),
                description: None,
                status: None,
                summary: None,
                event_id: Some(format!("uuid-{suffix}")),
                data: json!({ "task_id": format!("task-{suffix}") }),
            },
            AgentEvent::Done {
                stop_reason: StopReason::Completed,
                subtype: Some("success".into()),
                is_error: false,
                result_text: None,
                usage: Usage::default(),
                cost: Cost::unknown(),
                duration_ms,
                duration_api_ms: Some(duration_ms / 2),
                num_turns: 1,
                model: None,
                provider_session_id: Some(format!("sess-{suffix}")),
                structured_output: None,
                error: None,
            },
        ]);
        transcript.out_of_band.push(AgentEvent::BackgroundTasks {
            tasks: vec![BackgroundTask {
                id: format!("task-{suffix}"),
                kind: BackgroundTaskKind::Shell,
                description: "sleep".into(),
                status: BackgroundTaskStatus::Running,
                started_at_ms: Some(duration_ms * 1000),
                tool_call_id: Some(tool),
                parent: Some(format!("thread-{suffix}")),
                pid: Some(duration_ms as u32),
            }],
        });
        transcript
    }

    #[test]
    fn two_recordings_of_one_scenario_normalise_to_the_same_bytes() {
        let mut first = sample("abc", "/Users/alice/project", 812);
        let mut second = sample("zzz9", "/home/bob/checkout/project", 40_031);
        assert_ne!(first.to_json_string(), second.to_json_string());
        normalize(&mut first);
        normalize(&mut second);
        assert_eq!(first.to_json_string(), second.to_json_string());

        let json = first.to_json_string();
        for expected in [
            "<session-1>",
            "<req-1>",
            "<tool-1>",
            "<task-1>",
            "<event-1>",
            "<thread-1>",
            "<cwd>/src/lib.rs",
            "\"duration_ms\": 0",
            "\"duration_api_ms\": 0",
            "\"started_at_ms\": 0",
            "\"pid\": 0",
        ] {
            assert!(json.contains(expected), "{expected} missing from {json}");
        }
        for leak in ["abc", "alice", "812"] {
            assert!(!json.contains(leak), "{leak} survived in {json}");
        }
    }

    #[test]
    fn correspondences_survive_normalisation() {
        let mut transcript = sample("abc", "/work/dir", 5);
        normalize(&mut transcript);
        let turn = &transcript.turns[0];
        // The sub-agent text met BEFORE its tool call still points to the call.
        assert!(
            matches!(&turn[1], AgentEvent::Text { parent: Some(parent), .. } if parent == "<tool-1>")
        );
        assert!(matches!(&turn[2], AgentEvent::ToolCall { id, input, .. }
            if id == "<tool-1>" && input["self"] == "<tool-1>"));
        assert!(
            matches!(&turn[3], AgentEvent::PermissionAsk { tool_call_id: Some(id), .. } if id == "<tool-1>")
        );
        assert!(matches!(&turn[4], AgentEvent::ToolResult { id, .. } if id == "<tool-1>"));
        assert!(
            matches!(&turn[5], AgentEvent::TaskUpdate { task_id: Some(id), data, .. }
            if id == "<task-1>" && data["task_id"] == "<task-1>")
        );
        assert!(
            matches!(&transcript.out_of_band[0], AgentEvent::BackgroundTasks { tasks }
            if tasks[0].id == "<task-1>" && tasks[0].tool_call_id.as_deref() == Some("<tool-1>"))
        );
    }

    #[test]
    fn normalising_twice_changes_nothing() {
        let mut transcript = sample("abc", "/work/dir", 5);
        normalize(&mut transcript);
        let once = transcript.to_json_string();
        normalize(&mut transcript);
        assert_eq!(transcript.to_json_string(), once);
    }

    #[test]
    fn canonical_json_round_trips_and_has_sorted_keys() {
        let transcript = sample("abc", "/work/dir", 5);
        let json = transcript.to_json_string();
        assert!(json.ends_with("}\n"));
        assert_eq!(Transcript::from_json_str(&json).unwrap(), transcript);
        let positions: Vec<usize> = [
            "\"contract_version\"",
            "\"out_of_band\"",
            "\"provider_kind\"",
            "\"turns\"",
        ]
        .iter()
        .map(|key| json.find(key).unwrap())
        .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{json}");
    }

    #[test]
    fn a_transcript_of_another_contract_version_is_refused() {
        let mut transcript = Transcript::new(ProviderKind::Scripted);
        transcript.contract_version = CONTRACT_VERSION + 1;
        let error = Transcript::from_json_str(&transcript.to_json_string()).unwrap_err();
        assert!(matches!(error, ProviderError::InvalidRequest { .. }));
        assert!(Transcript::from_json_str("{").is_err());
    }

    #[test]
    fn save_and_load_use_the_canonical_form() {
        let transcript = sample("abc", "/work/dir", 5);
        let path = std::env::temp_dir().join(format!(
            "nexus-testkit-transcript-{}.json",
            uuid::Uuid::new_v4()
        ));
        transcript.save(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            transcript.to_json_string()
        );
        assert_eq!(Transcript::load(&path).unwrap(), transcript);
        std::fs::remove_file(&path).unwrap();
    }
}
