//! The conversation of a native session, and where it is kept (contract §8).
//!
//! A [`ChatMessage`] list is the whole state of a native session: replaying it
//! to the endpoint continues the conversation. It is stored behind a
//! [`TranscriptStore`] so that `resume(spec, token)` can reload it. The resume
//! token is `{"transcript_id": "<id>"}`.
//!
//! **Reasoning is kept** (decision A39): an assistant message keeps its
//! `reasoning`, because DeepSeek rejects a tool-calling transcript that dropped
//! `reasoning_content`. The wire layer (`model::wire`) decides whether to send
//! it back; the transcript never discards it, and neither does compaction.
//!
//! **No credential is persisted**: the file store masks credential-shaped
//! fragments (`agent::redact`, applied by chunks so that long texts are not
//! truncated) before writing. The in-memory store keeps the exact text (it
//! never leaves the process). **Images are kept as they are**: a base64 payload
//! is a blob, not prose, and the masking of prose would mangle it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::agent::{ProviderError, redact};
use crate::model::ChatMessage;

/// Version written in a persisted transcript.
pub const TRANSCRIPT_FORMAT_VERSION: u32 = 1;

/// Where transcripts are kept. Implementations are synchronous: a transcript is
/// a small JSON document, written once per turn.
pub trait TranscriptStore: Send + Sync {
    /// The messages of `id`; `None` when the transcript is unknown.
    fn load(&self, id: &str) -> Result<Option<Vec<ChatMessage>>, ProviderError>;

    /// Replaces the messages of `id`.
    fn save(&self, id: &str, messages: &[ChatMessage]) -> Result<(), ProviderError>;
}

/// A transcript identifier is a file name: letters, digits, `-` and `_`.
pub fn valid_transcript_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// A fresh transcript identifier.
pub fn new_transcript_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Transcripts kept in memory (the default).
#[derive(Debug, Default)]
pub struct MemoryTranscriptStore {
    transcripts: Mutex<HashMap<String, Vec<ChatMessage>>>,
}

impl MemoryTranscriptStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl TranscriptStore for MemoryTranscriptStore {
    fn load(&self, id: &str) -> Result<Option<Vec<ChatMessage>>, ProviderError> {
        Ok(self
            .transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned())
    }

    fn save(&self, id: &str, messages: &[ChatMessage]) -> Result<(), ProviderError> {
        self.transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_owned(), messages.to_vec());
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    version: u32,
    messages: Vec<ChatMessage>,
}

/// Transcripts kept as `<dir>/<id>.json`, files `0600` in a directory `0700`.
#[derive(Debug, Clone)]
pub struct FileTranscriptStore {
    dir: PathBuf,
}

impl FileTranscriptStore {
    /// A store under `dir`; the directory is created (`0700`) at the first save.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory the transcripts live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str) -> Result<PathBuf, ProviderError> {
        if !valid_transcript_id(id) {
            return Err(ProviderError::invalid("malformed transcript id"));
        }
        Ok(self.dir.join(format!("{id}.json")))
    }
}

fn io_error(what: &str, error: &std::io::Error) -> ProviderError {
    // The path may name a user; keep only the kind of failure.
    ProviderError::protocol(format!("transcript {what} failed: {:?}", error.kind()))
}

impl TranscriptStore for FileTranscriptStore {
    fn load(&self, id: &str) -> Result<Option<Vec<ChatMessage>>, ProviderError> {
        let path = self.path(id)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("read", &error)),
        };
        let persisted: Persisted = serde_json::from_slice(&bytes)
            .map_err(|_| ProviderError::invalid("the transcript file is not valid"))?;
        if persisted.version != TRANSCRIPT_FORMAT_VERSION {
            return Err(ProviderError::invalid(
                "the transcript file has an unknown version",
            ));
        }
        Ok(Some(persisted.messages))
    }

    fn save(&self, id: &str, messages: &[ChatMessage]) -> Result<(), ProviderError> {
        use std::io::Write;
        let path = self.path(id)?;
        create_private_dir(&self.dir).map_err(|e| io_error("directory creation", &e))?;
        let persisted = Persisted {
            version: TRANSCRIPT_FORMAT_VERSION,
            messages: messages.iter().map(scrub_message).collect(),
        };
        let bytes = serde_json::to_vec(&persisted)
            .map_err(|_| ProviderError::protocol("the transcript is not serialisable"))?;
        let temporary = self.dir.join(format!(".{id}.tmp"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|e| io_error("write", &e))?;
        file.write_all(&bytes).map_err(|e| io_error("write", &e))?;
        file.sync_all().map_err(|e| io_error("write", &e))?;
        drop(file);
        std::fs::rename(&temporary, &path).map_err(|e| io_error("write", &e))
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Longest run of text handed to `redact` at once (it truncates at 512 bytes).
const SCRUB_CHUNK: usize = 400;

/// Masks credential-shaped fragments of a text without truncating it. The text
/// is cut at line ends and spaces into chunks `redact` keeps whole; a single
/// word longer than a chunk is replaced (it is a blob, not prose).
pub fn scrub_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if line.len() <= SCRUB_CHUNK {
            out.push_str(&redact(line));
            continue;
        }
        let mut chunk = String::new();
        let flush = |chunk: &mut String, out: &mut String| {
            if !chunk.is_empty() {
                out.push_str(&redact(chunk));
                chunk.clear();
            }
        };
        for word in line.split(' ') {
            if word.len() > SCRUB_CHUNK {
                flush(&mut chunk, &mut out);
                out.push_str("<redacted:long>");
                chunk.push(' ');
                continue;
            }
            if !chunk.is_empty() && chunk.len() + 1 + word.len() > SCRUB_CHUNK {
                flush(&mut chunk, &mut out);
                out.push(' ');
            } else if !chunk.is_empty() {
                chunk.push(' ');
            }
            chunk.push_str(word);
        }
        flush(&mut chunk, &mut out);
    }
    out
}

fn scrub_message(message: &ChatMessage) -> ChatMessage {
    let mut message = message.clone();
    message.content = message.content.as_deref().map(scrub_text);
    message.reasoning = message.reasoning.as_deref().map(scrub_text);
    for call in &mut message.tool_calls {
        call.arguments = scrub_text(&call.arguments);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolCallChunk;

    fn sample() -> Vec<ChatMessage> {
        let mut call = ChatMessage::assistant_tool_calls(vec![ToolCallChunk {
            id: "c1".into(),
            name: "mcp__fake__echo".into(),
            arguments: r#"{"text":"hi"}"#.into(),
        }]);
        call.reasoning = Some("thinking about echo".into());
        vec![
            ChatMessage::user("hello"),
            call,
            ChatMessage::tool("c1", "echo: hi"),
        ]
    }

    #[test]
    fn the_memory_store_round_trips_and_knows_what_it_does_not_have() {
        let store = MemoryTranscriptStore::new();
        assert_eq!(store.load("nope").unwrap(), None);
        store.save("t1", &sample()).unwrap();
        assert_eq!(store.load("t1").unwrap(), Some(sample()));
    }

    #[test]
    fn the_file_store_keeps_reasoning_and_writes_private_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTranscriptStore::new(dir.path().join("transcripts"));
        store.save("abc-1", &sample()).unwrap();
        assert_eq!(store.load("abc-1").unwrap(), Some(sample()));
        assert_eq!(
            store.load("abc-1").unwrap().unwrap()[1]
                .reasoning
                .as_deref(),
            Some("thinking about echo")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = std::fs::metadata(store.dir().join("abc-1.json")).unwrap();
            assert_eq!(file.permissions().mode() & 0o777, 0o600);
            let folder = std::fs::metadata(store.dir()).unwrap();
            assert_eq!(folder.permissions().mode() & 0o777, 0o700);
        }
        assert_eq!(store.load("unknown").unwrap(), None);
    }

    #[test]
    fn a_transcript_id_cannot_escape_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTranscriptStore::new(dir.path());
        for bad in ["../x", "a/b", "", "a.b", "x\0y"] {
            assert_eq!(
                store.load(bad).unwrap_err().kind(),
                "invalid_request",
                "{bad}"
            );
            assert_eq!(
                store.save(bad, &sample()).unwrap_err().kind(),
                "invalid_request"
            );
        }
    }

    #[test]
    fn the_file_store_masks_credentials_and_keeps_long_texts_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTranscriptStore::new(dir.path());
        let long = "word ".repeat(600);
        let messages = vec![
            ChatMessage::user("curl -H 'Authorization: Bearer sk-live-SECRETVALUE1234' now"),
            ChatMessage::tool(
                "c1",
                format!("db at postgres://admin:hunter2pass@db/x\n{long}"),
            ),
        ];
        store.save("t", &messages).unwrap();
        let raw = std::fs::read_to_string(store.dir().join("t.json")).unwrap();
        assert!(!raw.contains("SECRETVALUE1234"), "{raw}");
        assert!(!raw.contains("hunter2pass"), "{raw}");
        let back = store.load("t").unwrap().unwrap();
        // The long text is not truncated to 512 bytes.
        assert!(back[1].content.as_deref().unwrap().len() > 2500);
    }

    #[test]
    fn a_corrupt_file_is_invalid_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.json"), b"{not json").unwrap();
        let store = FileTranscriptStore::new(dir.path());
        assert_eq!(store.load("bad").unwrap_err().kind(), "invalid_request");
    }
}
