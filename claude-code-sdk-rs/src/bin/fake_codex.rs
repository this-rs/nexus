//! `fake_codex` — a dependency-free stand-in for `codex` (`--version` and
//! `app-server`), for the tests of `providers::codex`.
//!
//! The installed `codex` is 0.38.0 and has no `app-server`, so the adapter is tested
//! against this executable, which replays a JSONL **transcript** of a session. Cargo
//! hands integration tests its path as `env!("CARGO_BIN_EXE_fake_codex")`. The
//! transcripts (`tests/transcripts/codex/<version>/sessions/*.jsonl`) are written
//! from the documentation of `codex app-server`, not recorded from a real session
//! (see `PROVENANCE.md` next to the schema).
//!
//! # Invocation
//!
//! * `fake_codex --version` prints `codex-cli <version>`; the version is
//!   `FAKE_CODEX_VERSION`, else the first line of `<exe>.version`, else `0.130.0`.
//! * `fake_codex [-c key=value]… app-server` plays the transcript. Without
//!   `app-server` in its arguments it fails like an old `codex` (exit 2).
//!
//! # Environment
//!
//! | Variable | Meaning |
//! |---|---|
//! | `FAKE_CODEX_TRANSCRIPT` | path of the transcript to play (required for `app-server`) |
//! | `FAKE_CODEX_RECORD` | path of the recording (JSON lines), written as things happen |
//! | `FAKE_CODEX_CANARY` | a secret the test knows: the fake never writes it, it records whether it **saw** it (argv, stdin line, an environment value other than `CODEX_API_KEY`) |
//! | `FAKE_CODEX_WAIT_MS` | default timeout of the waiting directives (default 10000) |
//! | `FAKE_CODEX_MAX_RUNTIME_MS` | watchdog: kill the children and exit 98 after this long (default 30000) |
//!
//! # What is recorded (never a secret)
//!
//! `start` (argv with the canary masked, environment variable **names**, the value
//! of `CODEX_HOME` only, the length of `CODEX_API_KEY`), `in` (method and
//! parameters of every request or notification, values under credential-shaped
//! names masked), `response` (the raw line of every answer to a server request,
//! verbatim: it holds an approval decision, nothing else), `child`, `exit`.
//!
//! # Directives
//!
//! One JSON object per line; blank lines and lines starting with `#` or `//` are
//! ignored.
//!
//! * `{"op":"include","file":"prelude.jsonl"}` — splice a file of the same directory.
//! * `{"op":"expect","method":"turn/start","reply":{…}}` — wait for a request of that
//!   method, answer it (`"error":{"code":…,"message":…}` answers an error instead).
//! * `{"op":"expect_notification","method":"initialized"}`
//! * `{"op":"notify","method":"item/started","params":{…}}`
//! * `{"op":"server_request","id":"s1","method":"mcpServer/elicitation/request","params":{…}}`
//! * `{"op":"await_response","id":"s1"}` — wait for the answer to a server request.
//! * `{"op":"emit","line":"raw text"}`, `{"op":"stderr","line":"…"}`, `{"op":"sleep","ms":N}`
//! * `{"op":"spawn_child","own_group":true}` — start a `sleep 600` child (in its own
//!   process group when asked) and record its pid.
//! * `{"op":"wait_eof"}`, `{"op":"exit","code":N}`
//!
//! Waiting directives take `"timeout_ms"`. When the transcript runs out the fake
//! waits for stdin to close (the session closing), then exits 0. Diagnostic exit
//! codes: 94 unusable directive, 95 stdin closed while waiting, 96 wait timed out,
//! 97 transcript unreadable, 98 watchdog, 99 unexpected answer.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

const DEFAULT_VERSION: &str = "0.130.0";
const EXIT_BAD_DIRECTIVE: i32 = 94;
const EXIT_STDIN_CLOSED: i32 = 95;
const EXIT_TIMEOUT: i32 = 96;
const EXIT_NO_TRANSCRIPT: i32 = 97;
const EXIT_WATCHDOG: i32 = 98;
const EXIT_UNEXPECTED: i32 = 99;

/// Names that make a recorded value unreadable.
const SECRET_MARKERS: [&str; 8] = [
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "authorization",
    "bearer",
];

type Children = Arc<Mutex<Vec<Child>>>;

struct Fake {
    lines: Receiver<Option<String>>,
    record: Option<std::fs::File>,
    canary: Option<String>,
    children: Children,
    default_wait: Duration,
    /// Responses to server requests that arrived while waiting for something else.
    answers: BTreeMap<String, Value>,
}

fn kill_children(children: &Children) {
    if let Ok(mut children) = children.lock() {
        for child in children.iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        children.clear();
    }
}

fn die(children: &Children, code: i32, message: &str) -> ! {
    eprintln!("fake_codex: {message}");
    kill_children(children);
    std::process::exit(code);
}

fn mask(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, inner)| {
                    let lower = key.to_ascii_lowercase();
                    if SECRET_MARKERS.iter().any(|marker| lower.contains(marker)) {
                        (key.clone(), json!("<redacted>"))
                    } else {
                        (key.clone(), mask(inner))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(mask).collect()),
        other => other.clone(),
    }
}

impl Fake {
    fn record(&mut self, entry: &Value) {
        if let Some(file) = &mut self.record {
            let _ = writeln!(file, "{entry}");
            let _ = file.flush();
        }
    }

    fn has_canary(&self, text: &str) -> bool {
        self.canary
            .as_deref()
            .is_some_and(|canary| !canary.is_empty() && text.contains(canary))
    }

    fn write_line(&self, value: &Value) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    }

    /// Records an inbound line and returns it parsed.
    fn read_line(&mut self, timeout: Duration) -> Result<Value, i32> {
        let line = match self.lines.recv_timeout(timeout) {
            Ok(Some(line)) => line,
            Ok(None) | Err(RecvTimeoutError::Disconnected) => return Err(EXIT_STDIN_CLOSED),
            Err(RecvTimeoutError::Timeout) => return Err(EXIT_TIMEOUT),
        };
        let canary = self.has_canary(&line);
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            self.record(&json!({"kind": "in", "malformed": true, "canary": canary}));
            return Ok(Value::Null);
        };
        if value.get("method").is_some() {
            let entry = json!({
                "kind": "in",
                "method": value["method"],
                "has_id": value.get("id").is_some(),
                "params": if canary { json!("<masked: canary>") } else { mask(&value["params"]) },
                "canary": canary,
            });
            self.record(&entry);
        } else {
            // An answer to a server request: kept verbatim (it holds a decision).
            let raw = if canary {
                "<masked: canary>".to_owned()
            } else {
                line
            };
            self.record(
                &json!({"kind": "response", "id": value["id"], "raw": raw, "canary": canary}),
            );
            if let Some(id) = value.get("id") {
                self.answers.insert(id.to_string(), value.clone());
            }
        }
        Ok(value)
    }

    fn wait(&self, directive: &Value) -> Duration {
        directive
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(self.default_wait)
    }

    fn run(&mut self, directives: &[Value]) {
        for directive in directives {
            let op = directive.get("op").and_then(Value::as_str).unwrap_or("");
            match op {
                "expect" | "expect_notification" => {
                    let method = directive["method"].as_str().unwrap_or("").to_owned();
                    let timeout = self.wait(directive);
                    loop {
                        let message = match self.read_line(timeout) {
                            Ok(message) => message,
                            Err(code) => die(
                                &self.children,
                                code,
                                &format!("waiting for `{method}` failed (code {code})"),
                            ),
                        };
                        if message["method"].as_str() != Some(method.as_str()) {
                            continue;
                        }
                        if op == "expect" {
                            let id = message["id"].clone();
                            if let Some(error) = directive.get("error") {
                                self.write_line(&json!({"id": id, "error": error}));
                            } else {
                                let reply = directive.get("reply").cloned().unwrap_or(json!({}));
                                self.write_line(&json!({"id": id, "result": reply}));
                            }
                        }
                        break;
                    }
                },
                "notify" => self.write_line(&json!({
                    "method": directive["method"],
                    "params": directive.get("params").cloned().unwrap_or(json!({})),
                })),
                "server_request" => self.write_line(&json!({
                    "id": directive["id"],
                    "method": directive["method"],
                    "params": directive.get("params").cloned().unwrap_or(json!({})),
                })),
                "await_response" => {
                    let key = directive["id"].to_string();
                    let timeout = self.wait(directive);
                    while !self.answers.contains_key(&key) {
                        if let Err(code) = self.read_line(timeout) {
                            die(
                                &self.children,
                                code,
                                &format!("waiting for the answer to {key} failed (code {code})"),
                            );
                        }
                    }
                    if let Some(expected) = directive.get("expect")
                        && self.answers[&key].get("result") != Some(expected)
                    {
                        die(
                            &self.children,
                            EXIT_UNEXPECTED,
                            "the answer is not the expected one",
                        );
                    }
                },
                "emit" => {
                    let mut out = std::io::stdout().lock();
                    let _ = writeln!(out, "{}", directive["line"].as_str().unwrap_or(""));
                    let _ = out.flush();
                },
                "stderr" => eprintln!("{}", directive["line"].as_str().unwrap_or("")),
                "sleep" => std::thread::sleep(Duration::from_millis(
                    directive.get("ms").and_then(Value::as_u64).unwrap_or(0),
                )),
                "spawn_child" => self.spawn_child(directive),
                "wait_eof" => self.wait_eof(),
                "exit" => {
                    let code = directive
                        .get("code")
                        .and_then(Value::as_i64)
                        .and_then(|code| i32::try_from(code).ok())
                        .unwrap_or(0);
                    self.record(&json!({"kind": "exit", "code": code}));
                    kill_children(&self.children);
                    std::process::exit(code);
                },
                other => die(
                    &self.children,
                    EXIT_BAD_DIRECTIVE,
                    &format!("unusable directive `{other}`"),
                ),
            }
        }
    }

    fn spawn_child(&mut self, directive: &Value) {
        let program = directive
            .get("program")
            .and_then(Value::as_str)
            .unwrap_or("sleep");
        let args: Vec<String> = directive
            .get("args")
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_else(|| vec!["600".to_owned()]);
        let own_group = directive
            .get("own_group")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut command = Command::new(program);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        if own_group {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        match command.spawn() {
            Ok(child) => {
                self.record(&json!({"kind": "child", "pid": child.id(), "own_group": own_group}));
                if let Ok(mut children) = self.children.lock() {
                    children.push(child);
                }
            },
            Err(error) => die(
                &self.children,
                EXIT_BAD_DIRECTIVE,
                &format!("cannot spawn a child: {error}"),
            ),
        }
    }

    fn wait_eof(&mut self) {
        loop {
            match self.lines.recv_timeout(Duration::from_secs(3600)) {
                Ok(Some(line)) => {
                    // Still recorded: a late request is worth seeing.
                    let canary = self.has_canary(&line);
                    if let Ok(value) = serde_json::from_str::<Value>(&line)
                        && value.get("method").is_some()
                    {
                        let entry = json!({"kind": "in", "method": value["method"], "has_id": value.get("id").is_some(), "canary": canary});
                        self.record(&entry);
                    }
                },
                _ => return,
            }
        }
    }
}

/// Reads a transcript, splicing its `include`s.
fn load(path: &Path, depth: u32) -> Result<Vec<Value>, String> {
    if depth > 8 {
        return Err("includes nest too deep".to_owned());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut directives = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|error| format!("{}:{}: {error}", path.display(), number + 1))?;
        if value.get("op").and_then(Value::as_str) == Some("include") {
            let file = value["file"].as_str().ok_or("include without a file")?;
            let base = path.parent().unwrap_or_else(|| Path::new("."));
            directives.extend(load(&base.join(file), depth + 1)?);
        } else {
            directives.push(value);
        }
    }
    Ok(directives)
}

fn version() -> String {
    if let Ok(version) = std::env::var("FAKE_CODEX_VERSION")
        && !version.is_empty()
    {
        return version;
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut sidecar = exe.into_os_string();
        sidecar.push(".version");
        if let Ok(text) = std::fs::read_to_string(PathBuf::from(sidecar))
            && let Some(first) = text.lines().next()
        {
            return first.trim().to_owned();
        }
    }
    DEFAULT_VERSION.to_owned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("codex-cli {}", version());
        return;
    }
    if !args.iter().any(|arg| arg == "app-server") {
        eprintln!("error: unrecognized subcommand (this codex has no `app-server`)");
        std::process::exit(2);
    }
    let children: Children = Arc::new(Mutex::new(Vec::new()));
    let max_runtime = std::env::var("FAKE_CODEX_MAX_RUNTIME_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30_000);
    {
        let children = Arc::clone(&children);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(max_runtime));
            die(
                &children,
                EXIT_WATCHDOG,
                "watchdog: the transcript did not finish in time",
            );
        });
    }
    let Some(transcript) = std::env::var_os("FAKE_CODEX_TRANSCRIPT") else {
        die(
            &children,
            EXIT_NO_TRANSCRIPT,
            "FAKE_CODEX_TRANSCRIPT is not set",
        );
    };
    let directives = match load(Path::new(&transcript), 0) {
        Ok(directives) => directives,
        Err(error) => die(&children, EXIT_NO_TRANSCRIPT, &error),
    };
    let canary = std::env::var("FAKE_CODEX_CANARY").ok();
    let record = std::env::var_os("FAKE_CODEX_RECORD")
        .and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok());

    let (sender, lines) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in BufReader::new(stdin.lock()).lines() {
            match line {
                Ok(line) => {
                    if sender.send(Some(line)).is_err() {
                        return;
                    }
                },
                Err(_) => break,
            }
        }
        let _ = sender.send(None);
    });

    let mut fake = Fake {
        lines,
        record,
        canary,
        children: Arc::clone(&children),
        default_wait: Duration::from_millis(
            std::env::var("FAKE_CODEX_WAIT_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(10_000),
        ),
        answers: BTreeMap::new(),
    };

    // The start record: argv and environment, never a secret.
    let seen_canary_in_argv = args.iter().any(|arg| fake.has_canary(arg));
    let masked_argv: Vec<String> = args
        .iter()
        .map(|arg| {
            if fake.has_canary(arg) {
                "<masked: canary>".to_owned()
            } else {
                arg.clone()
            }
        })
        .collect();
    let mut env_names: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .collect();
    env_names.sort();
    let canary_in_env: Vec<String> = std::env::vars()
        .filter(|(name, value)| {
            name != "CODEX_API_KEY" && name != "FAKE_CODEX_CANARY" && fake.has_canary(value)
        })
        .map(|(name, _)| name)
        .collect();
    let entry = json!({
        "kind": "start",
        "argv": masked_argv,
        "canary_in_argv": seen_canary_in_argv,
        "env_names": env_names,
        "codex_home": std::env::var("CODEX_HOME").ok(),
        "api_key_len": std::env::var("CODEX_API_KEY").ok().map(|key| key.len()),
        "canary_in_env_of": canary_in_env,
        "cwd": std::env::current_dir().ok().map(|dir| dir.display().to_string()),
        "pid": std::process::id(),
    });
    fake.record(&entry);

    fake.run(&directives);
    // The transcript ran out: stay up until the session closes stdin.
    fake.wait_eof();
    fake.record(&json!({"kind": "exit", "code": 0}));
    kill_children(&children);
}
