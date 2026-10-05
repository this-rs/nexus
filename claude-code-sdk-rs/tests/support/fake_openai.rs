//! Test support: runs the `fake_openai` binary (see `src/bin/fake_openai.rs`), reads
//! the port it announces, and kills it on drop.
//!
//! Include it from a test file with
//! `#[path = "support/fake_openai.rs"] mod fake_openai;`.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

/// A running fake server. Killed when dropped.
pub struct FakeOpenAi {
    child: Child,
    port: u16,
    dir: TempDir,
}

impl FakeOpenAi {
    /// Starts the server with `routes` (the JSON list described in `fake_openai.rs`).
    pub fn start(routes: Value) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let script = dir.path().join("script.json");
        std::fs::write(&script, routes.to_string()).expect("write script");
        let mut child = Command::new(env!("CARGO_BIN_EXE_fake_openai"))
            .env("FAKE_OPENAI_SCRIPT", &script)
            .env(
                "FAKE_OPENAI_REQUESTS_OUT",
                dir.path().join("requests.jsonl"),
            )
            .env("FAKE_OPENAI_MAX_RUNTIME_MS", "60000")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fake_openai");
        let stdout = child.stdout.take().expect("stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("read port");
        let port = line
            .trim()
            .strip_prefix("LISTENING ")
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("unexpected first line from fake_openai: {line:?}"));
        Self { child, port, dir }
    }

    /// Port the server listens on (127.0.0.1).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Base URL ending in `/v1`.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn requests_path(&self) -> PathBuf {
        self.dir.path().join("requests.jsonl")
    }

    /// Requests received so far, in order.
    pub fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.requests_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Requests received for `method path`.
    pub fn requests_to(&self, method: &str, path: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r["method"] == method && r["path"] == path)
            .collect()
    }

    /// The whole request log as text (to search for leaked secrets).
    pub fn raw_log(&self) -> String {
        std::fs::read_to_string(self.requests_path()).unwrap_or_default()
    }
}

impl Drop for FakeOpenAi {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
