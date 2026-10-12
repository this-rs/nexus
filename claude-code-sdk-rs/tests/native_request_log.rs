//! The per-request log line of the native harness (`native model request`, target
//! `nexus_claude::native::request`): one INFO line per model request, with what a
//! cache-hit ratio is read from in production (prompt, cached part, completion),
//! the latency, the host's session id and the fingerprint of the cacheable prefix.
//!
//! Its own test binary with ONE test: the subscriber is the process-wide default
//! (a thread-local one races with the callsite cache of `tracing` when other tests
//! of the binary run in parallel).

#[path = "support/fake_openai.rs"]
mod fake_openai;
#[path = "support/native.rs"]
mod native;

use std::sync::{Arc, Mutex};

use fake_openai::FakeOpenAi;
use native::*;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, SessionSpec, StopReason, SystemPromptMode, SystemPromptSpec,
    TurnInput,
};
use nexus_claude::model::EndpointQuirks;
use nexus_claude::providers::native::{MemoryTranscriptStore, NativeConfig, NativeProvider};
use serde_json::{Value, json};

/// A `MakeWriter` that appends every formatted event to a shared buffer.
#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl CapturedLog {
    fn lines(&self, needle: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|line| line.contains(needle))
            .map(str::to_owned)
            .collect()
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for CapturedLog {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

fn with_usage(mut route: Value, prompt: u64, hit: u64) -> Value {
    let events = route["sse"].as_array_mut().unwrap();
    events.insert(
        events.len() - 1,
        json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": 7,
            "total_tokens": prompt + 7, "prompt_cache_hit_tokens": hit,
            "prompt_cache_miss_tokens": prompt - hit}}),
    );
    route
}

fn field(line: &str, name: &str) -> String {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{name}=")).map(str::to_owned))
        .unwrap_or_else(|| panic!("{name} missing in {line}"))
}

async fn run_turn(
    server: &FakeOpenAi,
    log_dir: &tempfile::TempDir,
    host_session_id: &str,
) -> Vec<AgentEvent> {
    let mut config = NativeConfig::new("native-test");
    config.default_model = Some("m".to_owned());
    let provider = NativeProvider::new(
        config,
        endpoint(server.base_url(), EndpointQuirks::deepseek()),
    )
    .with_transcript_store(Arc::new(MemoryTranscriptStore::new()));
    let cwd = tempfile::tempdir().unwrap();
    let mut spec = SessionSpec::new(cwd.path());
    spec.model = Some("m".to_owned());
    spec.mcp_servers.insert(
        "fake".to_owned(),
        stdio_mcp(Some(&log_dir.path().join("mcp.jsonl"))),
    );
    spec.system_prompt = Some(SystemPromptSpec {
        text: "a stable system prompt".to_owned(),
        mode: SystemPromptMode::Replace,
    });
    spec.extensions.insert(
        "native".to_owned(),
        json!({ "host_session_id": host_session_id }),
    );
    let session = provider.open(spec).await.expect("open");
    collect(
        session
            .send_turn(TurnInput::text("look"))
            .await
            .expect("send_turn"),
    )
    .await
}

#[tokio::test]
async fn every_model_request_is_logged_with_its_usage_cache_latency_and_prefix_fingerprint() {
    let logs = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("first statement of the only test in this binary");

    let echo = ("c1", "mcp__fake__echo", json!({"text": "hi"}));
    let server = FakeOpenAi::start(json!([
        probe_route(true),
        models_route(128_000),
        with_usage(tool_reply(Some("look"), &[echo], None, None), 1000, 0),
        with_usage(
            text_reply(Some("\"role\":\"tool\""), "done", None, None),
            1100,
            960
        ),
    ]));
    let log_dir = tempfile::tempdir().unwrap();
    let events = run_turn(&server, &log_dir, "36d623c8-c7f6-4818-bcb3-26da85a97380").await;
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

    let lines = logs.lines("native model request");
    assert_eq!(lines.len(), 2, "{lines:?}");
    for (line, (request, prompt, cached)) in lines.iter().zip([(1, 1000, 0), (2, 1100, 960)]) {
        for expected in [
            " INFO nexus_claude::native::request".to_owned(),
            "host_session_id=\"36d623c8-c7f6-4818-bcb3-26da85a97380\"".to_owned(),
            "model=\"m\"".to_owned(),
            format!("request={request}"),
            format!("prompt_tokens={prompt}"),
            format!("cached_tokens={cached}"),
            "completion_tokens=7".to_owned(),
            "usage_reported=true".to_owned(),
            "tools=10".to_owned(),
        ] {
            assert!(line.contains(&expected), "{expected} missing in {line}");
        }
        for name in ["latency_ms", "first_chunk_ms", "transcript_id", "messages"] {
            field(line, name);
        }
    }
    // Same system prompt, same tools: the prefix fingerprint does not move.
    for name in ["system_hash", "tools_hash"] {
        assert_eq!(field(&lines[0], name), field(&lines[1], name), "{name}");
    }
    assert_ne!(field(&lines[0], "system_hash"), "0000000000000000");
    // Counts, hashes and ids only: no prompt text, no tool output.
    let all = logs.lines("").join("\n");
    assert!(!all.contains("a stable system prompt"), "{all}");
    assert!(!all.contains("echo: hi"), "{all}");

    // A host session id that is not a plain identifier is left out (no forged line).
    logs.clear();
    let server = FakeOpenAi::start(json!([
        probe_route(true),
        models_route(128_000),
        text_reply(None, "ok", None, Some((10, 2))),
    ]));
    run_turn(&server, &log_dir, "id\nINFO forged line").await;
    let lines = logs.lines("native model request");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("host_session_id=\"-\""), "{}", lines[0]);
    assert!(lines[0].contains("usage_reported=true"), "{}", lines[0]);
    assert!(logs.lines("forged").is_empty());
}
