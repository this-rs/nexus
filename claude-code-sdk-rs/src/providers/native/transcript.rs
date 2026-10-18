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
//! **The resume transcript is faithful** (decision CTX6): the file store writes
//! the messages as they are, so a resumed session replays the very request the
//! live one sent (the endpoint's prefix cache still serves it, and a tool-call
//! argument comes back as the JSON the model wrote). Masking by SHAPE (a field
//! named like a credential, a long word) corrupted the history and is gone from
//! this path. Secrets at rest are protected by the FILE: `0600` files in a `0700`
//! directory, the modes enforced at every save and repaired at load. The only
//! masking is by VALUE: the values the host declares in a [`KnownSecrets`]
//! ([`FileTranscriptStore::with_known_secrets`]) are replaced in contents and
//! reasoning, never inside tool-call arguments (they are replayed as JSON). What
//! is shown or logged goes through [`masked_for_display`], which still masks by
//! shape: that copy is never replayed.
//!
//! **Images are kept as they are**: a base64 payload is a blob, not prose.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::agent::{KnownSecrets, ProviderError, redact};
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

/// Transcripts kept as `<dir>/<id>.json`, files `0600` in a directory `0700`,
/// written as they are (byte-faithful resume) except for the values of
/// [`KnownSecrets`] (see the module documentation).
#[derive(Debug, Clone)]
pub struct FileTranscriptStore {
    dir: PathBuf,
    known: KnownSecrets,
}

impl FileTranscriptStore {
    /// A store under `dir`; the directory is created (`0700`) at the first save.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            known: KnownSecrets::new(),
        }
    }

    /// Masks the values of `known` (shared with the host, which fills it, e.g.
    /// through [`KnownSecrets::recording`]) in contents and reasoning before
    /// writing. Tool-call arguments are never touched: they are replayed as JSON.
    pub fn with_known_secrets(mut self, known: KnownSecrets) -> Self {
        self.known = known;
        self
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
        // A file left readable by others (copied, restored, written by an older
        // build) is made owner-only again.
        make_owner_only(&path).map_err(|e| io_error("permission repair", &e))?;
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
            messages: messages
                .iter()
                .map(|message| mask_known_values(message, &self.known))
                .collect(),
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
        // `mode` applies only when the file is created: a leftover temporary
        // file keeps its own mode unless it is set here, before any byte.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| io_error("write", &e))?;
        }
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

fn make_owner_only(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// The message as written to the resume transcript: the known secret values are
/// masked in the contents and the reasoning; everything else, tool-call
/// arguments first, is kept byte for byte.
fn mask_known_values(message: &ChatMessage, known: &KnownSecrets) -> ChatMessage {
    let mut message = message.clone();
    if known.is_empty() {
        return message;
    }
    message.content = message.content.as_deref().map(|text| known.mask(text));
    message.reasoning = message.reasoning.as_deref().map(|text| known.mask(text));
    if let Some(echo) = &mut message.wire_echo {
        // The echo holds the same texts: masked the same way. A reasoning block whose
        // text was masked no longer matches its signature and would be refused when
        // replayed: the echo goes on without its reasoning.
        let mut masked = echo.content.clone();
        mask_strings(&mut masked, known);
        if masked != echo.content {
            echo.content = masked;
            *echo = echo.without_reasoning();
        }
    }
    message
}

/// Masks every string of an echo but the opaque ones (`signature`, `data`, which carry
/// no text of the conversation) and the tool-call `input`, kept byte for byte like the
/// arguments it repeats.
fn mask_strings(value: &mut serde_json::Value, known: &KnownSecrets) {
    use serde_json::Value;
    match value {
        Value::String(text) => *text = known.mask(text),
        Value::Array(items) => items.iter_mut().for_each(|item| mask_strings(item, known)),
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if !matches!(key.as_str(), "signature" | "data" | "input") {
                    mask_strings(item, known);
                }
            }
        },
        _ => {},
    }
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

/// A copy of `messages` fit to SHOW or LOG: credential-shaped fragments are
/// masked ([`scrub_text`], by shape, failing closed) in contents, reasoning and
/// tool-call arguments. Never replay it: it is not the conversation (an argument
/// can come out as invalid JSON); the resume transcript is the store's.
pub fn masked_for_display(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages.iter().map(scrub_message).collect()
}

fn scrub_message(message: &ChatMessage) -> ChatMessage {
    let mut message = message.clone();
    message.content = message.content.as_deref().map(scrub_text);
    message.reasoning = message.reasoning.as_deref().map(scrub_text);
    for call in &mut message.tool_calls {
        call.arguments = scrub_text(&call.arguments);
    }
    // The wire echo repeats the fields above for the endpoint alone: not shown.
    message.wire_echo = None;
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

    /// Text that only LOOKS like a credential, a long word and a tool-call argument
    /// all come back byte for byte: the resume transcript is the conversation.
    fn shaped_like_secrets() -> Vec<ChatMessage> {
        let blob = format!("/{}", "segment/".repeat(60));
        let arguments = serde_json::json!({ "text": blob, "input_tokens": 12, "api_key": "k" });
        vec![
            ChatMessage::user(r#"{"input_tokens":12,"max_tokens":3} Bearer abc.def"#),
            ChatMessage::assistant_tool_calls(vec![ToolCallChunk {
                id: "c1".into(),
                name: "mcp__fake__echo".into(),
                arguments: arguments.to_string(),
            }]),
            ChatMessage::tool("c1", format!("{blob} postgres://admin:pw@db/x")),
        ]
    }

    #[test]
    fn the_file_store_writes_the_conversation_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTranscriptStore::new(dir.path());
        store.save("t", &shaped_like_secrets()).unwrap();
        assert_eq!(store.load("t").unwrap(), Some(shaped_like_secrets()));
        let back = store.load("t").unwrap().unwrap();
        serde_json::from_str::<serde_json::Value>(&back[1].tool_calls[0].arguments)
            .expect("the argument is still JSON");
    }

    #[test]
    fn the_file_store_masks_the_known_values_but_never_inside_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let known = KnownSecrets::new();
        let store = FileTranscriptStore::new(dir.path()).with_known_secrets(known.clone());
        known.register(&crate::agent::Secret::new("vault-value-0c4f2e"));
        let arguments = r#"{"key":"vault-value-0c4f2e"}"#;
        let mut call = ChatMessage::assistant_tool_calls(vec![ToolCallChunk {
            id: "c1".into(),
            name: "mcp__fake__echo".into(),
            arguments: arguments.into(),
        }]);
        call.reasoning = Some("the key is vault-value-0c4f2e".into());
        let messages = vec![
            ChatMessage::user("use vault-value-0c4f2e, input_tokens=12"),
            call,
            ChatMessage::tool("c1", "echo vault-value-0c4f2e"),
        ];
        store.save("t", &messages).unwrap();
        let back = store.load("t").unwrap().unwrap();
        assert_eq!(
            back[0].content.as_deref(),
            Some("use <redacted>, input_tokens=12")
        );
        assert_eq!(back[1].reasoning.as_deref(), Some("the key is <redacted>"));
        assert_eq!(back[2].content.as_deref(), Some("echo <redacted>"));
        assert_eq!(back[1].tool_calls[0].arguments, arguments);
    }

    #[test]
    fn the_display_copy_masks_by_shape_and_the_store_does_not() {
        let shown = masked_for_display(&shaped_like_secrets());
        let text = serde_json::to_string(&shown).unwrap();
        assert!(text.contains("<redacted"), "{text}");
        assert!(!text.contains("abc.def"), "{text}");
        assert!(!text.contains("admin:pw"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_leftover_or_restored_file_is_made_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FileTranscriptStore::new(dir.path().join("t"));
        store.save("x", &sample()).unwrap();
        let wide = std::fs::Permissions::from_mode(0o644);
        // A temporary file left readable by an interrupted write is narrowed before use.
        std::fs::write(store.dir().join(".x.tmp"), b"").unwrap();
        std::fs::set_permissions(store.dir().join(".x.tmp"), wide.clone()).unwrap();
        std::fs::set_permissions(store.dir(), std::fs::Permissions::from_mode(0o755)).unwrap();
        store.save("x", &sample()).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&store.dir().join("x.json")), 0o600);
        assert_eq!(mode(store.dir()), 0o700);
        // A file restored with wide modes is narrowed when it is read.
        std::fs::set_permissions(store.dir().join("x.json"), wide).unwrap();
        assert_eq!(store.load("x").unwrap(), Some(sample()));
        assert_eq!(mode(&store.dir().join("x.json")), 0o600);
    }

    #[test]
    fn a_corrupt_file_is_invalid_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.json"), b"{not json").unwrap();
        let store = FileTranscriptStore::new(dir.path());
        assert_eq!(store.load("bad").unwrap_err().kind(), "invalid_request");
    }
}
