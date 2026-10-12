//! A real secret given to a native session (decision CTX6): it never appears in
//! clear in a log, nor in a file others can read.
//!
//! The secret is the endpoint key, resolved through a host resolver wrapped by
//! `KnownSecrets::recording` (what the backend does with its vault), so the file
//! transcript store knows its VALUE. The session then sees the key everywhere a
//! conversation can carry it: the user's message, a tool-call argument, the tool's
//! result and the model's answer.
//!
//! Its own test binary with ONE test: the subscriber is the process-wide default
//! (a thread-local one races with the callsite cache of `tracing` when other tests
//! of the binary run in parallel, and captures nothing).

#[path = "support/fake_openai.rs"]
mod fake_openai;
#[path = "support/native.rs"]
mod native;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fake_openai::FakeOpenAi;
use native::*;
use nexus_claude::agent::{
    AgentProvider, CredentialRef, CredentialResolver, KnownSecrets, PolicyMode, ProviderError,
    Secret, SessionSpec, StopReason, ToolPolicy, TurnInput,
};
use nexus_claude::model::{EndpointQuirks, OpenAiEndpoint, OpenAiEndpointConfig};
use nexus_claude::providers::native::{
    FileTranscriptStore, NativeConfig, NativeProvider, TranscriptStore,
};
use serde_json::json;

const KEY: &str = "vault-key-5d0c8a1f9e";

/// A `MakeWriter` that appends every formatted event to a shared buffer.
#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

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

/// A resolver that hands out one fixed key, as the host's vault would.
struct VaultLike;

#[async_trait::async_trait]
impl CredentialResolver for VaultLike {
    async fn resolve(
        &self,
        _instance: &str,
        _reference: &CredentialRef,
    ) -> Result<Option<Secret>, ProviderError> {
        Ok(Some(Secret::new(KEY)))
    }
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test]
async fn a_real_secret_never_reaches_a_log_nor_a_file_others_can_read() {
    let logs = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    let known = KnownSecrets::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native-transcripts");
    let store = Arc::new(FileTranscriptStore::new(&root).with_known_secrets(known.clone()));
    let server = FakeOpenAi::start(json!([
        probe_route(true),
        models_route(128_000),
        tool_reply(
            None,
            &[("c1", "mcp__fake__echo", json!({ "text": KEY }))],
            Some(&format!("the user gave {KEY}")),
            None
        ),
        text_reply(None, &format!("done with {KEY}"), None, None),
    ]));
    let mut config = OpenAiEndpointConfig::new("native-test", server.base_url());
    config.credential = CredentialRef::Vault("deepseek".into());
    config.quirks = EndpointQuirks::deepseek();
    config.response_timeout = std::time::Duration::from_secs(15);
    config.idle_timeout = std::time::Duration::from_secs(15);
    let endpoint = Arc::new(OpenAiEndpoint::new(
        config,
        Arc::new(known.recording(Arc::new(VaultLike))),
    ));
    let mut native = NativeConfig::new("native-test");
    native.default_model = Some("m".to_owned());
    let provider = NativeProvider::new(native, endpoint).with_transcript_store(store.clone());

    let log_dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut spec = SessionSpec::new(cwd.path());
    spec.model = Some("m".to_owned());
    spec.mcp_servers.insert(
        "fake".to_owned(),
        stdio_mcp(Some(&log_dir.path().join("mcp.jsonl"))),
    );
    spec.policy =
        ToolPolicy::from_patterns(PolicyMode::Trust, &["mcp__fake__echo"], &[] as &[&str]).unwrap();
    let session = provider.open(spec).await.expect("open");
    let events = collect(
        session
            .send_turn(TurnInput::text(format!("my key is {KEY}, echo it")))
            .await
            .expect("send_turn"),
    )
    .await;
    assert!(
        matches!(terminal(&events), nexus_claude::agent::AgentEvent::Done { stop_reason, .. } if *stop_reason == StopReason::Completed),
        "{events:?}"
    );

    // Not vacuous: the key was resolved, sent, and went through the tool.
    assert!(!known.is_empty(), "the resolver was never asked");
    let sent = server.requests_to("POST", "/v1/chat/completions");
    let bodies: Vec<String> = sent.iter().map(|r| r["body"].to_string()).collect();
    assert!(
        bodies.iter().any(|b| b.contains(&format!("echo: {KEY}"))),
        "the tool result carried the key"
    );

    // 1. No log line, at any level and any target, carries the key.
    let captured = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(
        captured.contains("native model request"),
        "the subscriber captured the harness's own lines"
    );
    let leaks: Vec<&str> = captured.lines().filter(|line| line.contains(KEY)).collect();
    assert!(leaks.is_empty(), "the key reached a log: {leaks:#?}");

    // 2. Every file that holds it is owner-only, in owner-only directories.
    let files = files_under(&root);
    assert!(!files.is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        for file in &files {
            let raw = std::fs::read(file).unwrap();
            if String::from_utf8_lossy(&raw).contains(KEY) {
                assert_eq!(mode(file), 0o600, "{}", file.display());
                assert_eq!(mode(file.parent().unwrap()), 0o700);
            }
        }
    }

    // 3. In the transcript, masked by value where it is prose, kept where it is a
    //    replayed argument (the JSON the model wrote).
    let token = session.resume_token().expect("a token");
    let id = token.data()["transcript_id"].as_str().unwrap().to_owned();
    let messages = store.load(&id).unwrap().expect("the transcript");
    for message in &messages {
        for text in [&message.content, &message.reasoning].into_iter().flatten() {
            assert!(!text.contains(KEY), "{text}");
        }
    }
    assert!(
        messages
            .iter()
            .flat_map(|m| &m.tool_calls)
            .any(|call| call.arguments.contains(KEY)),
        "the argument is replayed as written"
    );
    session.close().await.unwrap();
}
