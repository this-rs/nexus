//! The parity bench (N25): every call recorded from the real Claude Code 2.1.287 is replayed
//! on `nexus-tools`, offline, and compared with what the real tool answered.
//!
//! The recordings are in `claude-code-sdk-rs/tests/parity/claude-code-2.1.287/` (README there:
//! how they were made, what was spent). A difference is either fixed or listed in
//! [`DEVIATIONS`] with its reason, and the list is **checked both ways**: an unexplained
//! difference fails, and so does an explained one that has gone away (a stale excuse hides the
//! next real regression).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nexus_tools::files::{FileConfig, Scope};
use nexus_tools::shell::ShellConfig;
use nexus_tools::{CallContext, Profile, SessionState, ToolRegistry};
use serde_json::Value;

/// `(scenario, call index) → why nexus-tools deliberately answers differently`.
const DEVIATIONS: &[(&str, usize, &str)] = &[
    (
        "read_limits",
        3,
        "a single line over the character cap is returned whole, and the empty last line after it is dropped with an announced cut (the real tool's cap is in tokens, a line of 60 000 `y` is cheap to it)",
    ),
    (
        "bash",
        7,
        "no \"You will be notified when it completes\": an MCP server over stdio has no channel for it; the message points at Read and TaskStop instead",
    ),
    (
        "bash_limits",
        3,
        "the shell is bash (or sh), not the user's login shell: `$0` differs by design",
    ),
];

/// Calls whose text depends on the machine (the wording of `ls`'s error): only the first line is
/// compared.
const MACHINE_DEPENDENT: &[(&str, usize)] = &[("bash_limits", 5)];

struct Bench {
    dir: tempfile::TempDir,
    registry: ToolRegistry,
    context: CallContext,
}

fn golden(scenario: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../claude-code-sdk-rs/tests/parity/claude-code-2.1.287")
        .join(format!("{scenario}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

fn put(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

const NOTEBOOK: &str = "{\n \"cells\": [\n  {\n   \"id\": \"c1\",\n   \"cell_type\": \"code\",\n   \"metadata\": {},\n   \"source\": \"print('a')\",\n   \"outputs\": [],\n   \"execution_count\": null\n  },\n  {\n   \"id\": \"c2\",\n   \"cell_type\": \"markdown\",\n   \"metadata\": {},\n   \"source\": \"# title\"\n  }\n ],\n \"metadata\": {},\n \"nbformat\": 4,\n \"nbformat_minor\": 5\n}";

/// The synthetic files the recordings were made on (README of the recordings).
fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let out = dir.path().join("out");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let rows = |n: usize, prefix: &str| {
        (1..=n)
            .map(|i| format!("{prefix} {i}\n"))
            .collect::<String>()
    };
    put(&work, "a.txt", &rows(12, "line"));
    put(&work, "empty.txt", "");
    put(
        &work,
        "long.txt",
        &format!("short first\n{}\nshort last\n", "x".repeat(2500)),
    );
    put(&work, "dup.txt", "alpha\nbeta\nalpha\ngamma\nalpha\n");
    put(&work, "uniq.txt", "one\ntwo\nthree\n");
    put(&work, "unread.txt", "untouched\n");
    put(&work, "nb.ipynb", NOTEBOOK);
    put(&work, "sub/inner.txt", "inner\n");
    put(&work, "a3000.txt", &rows(3000, "row"));
    put(&work, "line60k.txt", &format!("{}\n", "y".repeat(60_000)));
    let big: String = (0..3300)
        .map(|n| format!("word{n:05} word{n:05} word{n:05} word{n:05} \n"))
        .collect();
    put(&work, "big.txt", &big);

    let scope = Arc::new(Scope::new(&work, [out.clone()]).unwrap());
    let files = FileConfig::new(Scope::new(&work, [out.clone()]).unwrap());
    let shell = ShellConfig::new(scope, out).unwrap();
    let registry = nexus_tools::shell::register(
        nexus_tools::files::register(ToolRegistry::new(), &files),
        &shell,
    );
    Bench {
        dir,
        registry,
        context: CallContext::new("s", Arc::new(SessionState::default())),
    }
}

impl Bench {
    fn work(&self) -> String {
        std::fs::canonicalize(self.dir.path().join("work"))
            .unwrap()
            .display()
            .to_string()
    }

    /// The recorded input with `<FIXTURE>` turned into the real directory.
    fn input(&self, input: &Value) -> Value {
        serde_json::from_str(&input.to_string().replace("<FIXTURE>", &self.work())).unwrap()
    }

    /// Our answer, with what legitimately varies replaced by the recording's own placeholders.
    fn normalise(&self, text: &str) -> String {
        let mut text = text.replace(&self.work(), "<FIXTURE>");
        let replace = |text: String, pattern: &str, with: &str| {
            regex::Regex::new(pattern)
                .unwrap()
                .replace_all(&text, with)
                .into_owned()
        };
        text = replace(
            text,
            r"background with ID: [0-9a-f]{9}",
            "background with ID: <TASK>",
        );
        // The output directory may be spelled with or without the system's symlink prefix
        // (`/var` and `/private/var` on macOS): any path ending in `<id>.output`.
        text = replace(
            text,
            r"written to: \S+/[0-9a-f]{9}\.output",
            "written to: <CLAUDE_TMP>/…",
        );
        text = replace(
            text,
            r"saved to: \S+/[0-9a-f]{9}\.output",
            "saved to: <CLAUDE_STATE>/…",
        );
        // Our announced cut of a long Read: the recording has none (it cuts in silence).
        text = replace(
            text,
            r"\n\[output truncated: stopped at line \d+ of \d+; call Read again with offset \d+ to continue\]$",
            "",
        );
        replace(
            text,
            r"Inserted cell [0-9a-f]{8} with",
            "Inserted cell <CELL> with",
        )
    }
}

/// One replayed call that differs from the recording.
#[derive(Debug)]
struct Difference {
    scenario: String,
    index: usize,
    what: String,
}

async fn replay(scenario: &str) -> Vec<Difference> {
    let recording = golden(scenario);
    let bench = bench();
    let profile = Profile::unrestricted("s");
    let mut differences = Vec::new();
    for (index, call) in recording["calls"].as_array().unwrap().iter().enumerate() {
        let tool = call["tool"].as_str().unwrap();
        let tool = bench
            .registry
            .resolve(&profile, tool)
            .unwrap_or_else(|| panic!("{scenario}[{index}]: nexus-tools has no `{tool}`"));
        let ours = tool.call(&bench.context, bench.input(&call["input"])).await;
        let text = bench.normalise(&ours.text);
        let mut diff = |what: String| {
            differences.push(Difference {
                scenario: scenario.to_owned(),
                index,
                what,
            })
        };
        if ours.is_error != call["is_error"].as_bool().unwrap() {
            diff(format!(
                "is_error: recorded {}, ours {}",
                call["is_error"], ours.is_error
            ));
        }
        let machine_dependent = MACHINE_DEPENDENT
            .iter()
            .any(|(s, i)| *s == scenario && *i == index);
        match call["result"].as_str() {
            Some(recorded) if machine_dependent => {
                // The wording and the exit code of `ls` differ by system (BSD 1, GNU 2): only
                // "an error with an exit code" is compared.
                let first = |t: &str| {
                    let line = t.lines().next().unwrap_or_default();
                    if line.starts_with("Exit code ") {
                        "Exit code N".to_owned()
                    } else {
                        line.to_owned()
                    }
                };
                if first(&text) != first(recorded) {
                    diff(format!(
                        "first line:\n  recorded: {:?}\n  ours:     {:?}",
                        first(recorded),
                        first(&text)
                    ));
                }
            },
            Some(recorded) => {
                if text != recorded {
                    diff(format!(
                        "text:\n  recorded: {recorded:?}\n  ours:     {text:?}"
                    ));
                }
            },
            None => {
                // A long answer was recorded as its length, its head and its tail.
                let head = call["result_head"].as_str().unwrap_or_default();
                let tail = call["result_tail"].as_str().unwrap_or_default();
                let length = call["result_len"].as_u64().unwrap_or_default() as usize;
                if !text.starts_with(head) {
                    diff(format!(
                        "head differs: recorded {:?}, ours {:?}",
                        &head[..head.len().min(60)],
                        &text[..text.len().min(60)]
                    ));
                }
                if text.len() != length {
                    diff(format!("length: recorded {length}, ours {}", text.len()));
                } else if !text.ends_with(tail) {
                    diff("tail differs".to_owned());
                }
            },
        }
    }
    // What the files looked like afterwards: a failed call must have changed nothing.
    if let Some(files) = recording["final_files"].as_object() {
        // The id of an inserted notebook cell is random.
        let cell_id = regex::Regex::new(r#""id": "[0-9a-f]{8}""#).unwrap();
        for (name, content) in files {
            let path = Path::new(&bench.work()).join(name);
            let ours =
                std::fs::read_to_string(&path).unwrap_or_else(|e| format!("<unreadable: {e}>"));
            let recorded = content.as_str().unwrap();
            let ours = cell_id
                .replace_all(&ours, "\"id\": \"<CELL>\"")
                .into_owned();
            if ours != recorded {
                differences.push(Difference {
                    scenario: scenario.to_owned(),
                    index: usize::MAX,
                    what: format!(
                        "final file {name}:\n  recorded: {recorded:?}\n  ours:     {ours:?}"
                    ),
                });
            }
        }
    }
    differences
}

async fn check(scenario: &str) {
    let found = replay(scenario).await;
    let mut unexplained = Vec::new();
    for d in &found {
        if !DEVIATIONS
            .iter()
            .any(|(s, i, _)| *s == d.scenario && *i == d.index)
        {
            unexplained.push(format!("{}[{}]: {}", d.scenario, d.index, d.what));
        }
    }
    let stale: Vec<String> = DEVIATIONS
        .iter()
        .filter(|(s, i, _)| *s == scenario && !found.iter().any(|d| d.index == *i))
        .map(|(s, i, why)| {
            format!(
                "{s}[{i}] is listed as deviating ({why}) but now matches the recording: delete it"
            )
        })
        .collect();
    assert!(
        unexplained.is_empty() && stale.is_empty(),
        "\n{}\n{}",
        unexplained.join("\n"),
        stale.join("\n")
    );
}

#[tokio::test]
async fn read_matches_the_recording() {
    check("read").await;
}

#[tokio::test]
async fn read_limits_match_the_recording() {
    check("read_limits").await;
}

#[tokio::test]
async fn edit_and_write_match_the_recording() {
    check("edit").await;
}

#[tokio::test]
async fn bash_matches_the_recording() {
    check("bash").await;
}

#[tokio::test]
async fn bash_limits_match_the_recording() {
    check("bash_limits").await;
}

#[tokio::test]
async fn notebook_edit_matches_the_recording() {
    check("notebook").await;
}
