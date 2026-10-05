//! Drift gate of the ACP wire types (`providers::acp::wire`) against the versioned
//! schema in `tests/transcripts/acp/<version>/schema/` and against the session
//! transcripts replayed by `fake_acp`.
//!
//! The schema is **derived from the public specification** of the Agent Client
//! Protocol (see its `PROVENANCE.md`), not generated from a real agent: what this file
//! proves is that the adapter's types, the schema and the fixtures say the same thing.
//! It fails when:
//!
//! - a message kind exists in the types and not in the schema, or the reverse;
//! - an example of the schema is refused by the types;
//! - a path the schema calls required is not (the types accept the message without
//!   it), or a path it calls optional is (the types refuse the message without it);
//! - a `not_verified` entry names a path the schema does not list;
//! - a message of a transcript is refused by the types, or names a kind the schema
//!   does not list.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use nexus_claude::providers::acp::SUPPORTED_PROTOCOL_VERSION;
use nexus_claude::providers::acp::wire::{self, MESSAGE_KINDS};
use serde_json::Value;

fn version_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/acp")
        .join(SUPPORTED_PROTOCOL_VERSION.to_string())
}

fn schema() -> Value {
    let path = version_dir().join("schema/messages.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    serde_json::from_str(&text).expect("messages.json is valid JSON")
}

fn messages(schema: &Value) -> &serde_json::Map<String, Value> {
    schema["messages"]
        .as_object()
        .expect("`messages` is an object")
}

/// Dotted paths of every object member of `value` (arrays are leaves).
fn paths(value: &Value, prefix: &str, out: &mut Vec<String>) {
    if let Value::Object(map) = value {
        for (key, inner) in map {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            out.push(path.clone());
            paths(inner, &path, out);
        }
    }
}

fn remove(value: &mut Value, path: &str) {
    let mut parts: Vec<&str> = path.split('.').collect();
    let last = parts.pop().expect("a non-empty path");
    let mut cursor = value;
    for part in parts {
        cursor = cursor.get_mut(part).expect("the parent exists");
    }
    cursor.as_object_mut().expect("an object").remove(last);
}

fn strings(entry: &Value, key: &str) -> BTreeSet<String> {
    entry[key]
        .as_array()
        .unwrap_or_else(|| panic!("`{key}` is a list"))
        .iter()
        .map(|item| item.as_str().expect("a string").to_owned())
        .collect()
}

#[test]
fn the_schema_and_the_types_know_the_same_message_kinds() {
    let schema = schema();
    let in_schema: BTreeSet<&str> = messages(&schema).keys().map(String::as_str).collect();
    let in_types: BTreeSet<&str> = MESSAGE_KINDS.iter().copied().collect();
    assert_eq!(
        in_schema, in_types,
        "wire::MESSAGE_KINDS and schema/messages.json list different kinds"
    );
    for kind in MESSAGE_KINDS {
        let unknown = wire::validate_kind(kind, &Value::Null)
            .err()
            .is_some_and(|reason| reason.starts_with("unknown message kind"));
        assert!(!unknown, "validate_kind does not know `{kind}`");
    }
}

#[test]
fn every_example_of_the_schema_is_accepted_by_the_types() {
    let schema = schema();
    for (kind, entry) in messages(&schema) {
        let result = wire::validate_kind(kind, &entry["example"]);
        assert!(
            result.is_ok(),
            "the example of `{kind}` is refused: {result:?}"
        );
    }
}

#[test]
fn required_and_optional_paths_are_the_ones_the_types_enforce() {
    let schema = schema();
    for (kind, entry) in messages(&schema) {
        let required = strings(entry, "required");
        let conditional = strings(entry, "conditional");
        let optional = strings(entry, "optional");
        let mut listed: BTreeSet<String> = BTreeSet::new();
        for list in [&required, &conditional, &optional] {
            for path in list {
                assert!(
                    listed.insert(path.clone()),
                    "`{kind}`: `{path}` is listed twice"
                );
            }
        }
        let mut found = Vec::new();
        paths(&entry["example"], "", &mut found);
        let found: BTreeSet<String> = found.into_iter().collect();
        assert_eq!(
            found, listed,
            "`{kind}`: the paths of the example and the paths the schema classifies differ"
        );
        for path in &found {
            let mut without = entry["example"].clone();
            remove(&mut without, path);
            let accepted = wire::validate_kind(kind, &without).is_ok();
            let must_fail = required.contains(path) || conditional.contains(path);
            assert_eq!(
                !accepted,
                must_fail,
                "`{kind}`: without `{path}` the types {} the message, the schema says it is {}",
                if accepted { "accept" } else { "refuse" },
                if required.contains(path) {
                    "required"
                } else if conditional.contains(path) {
                    "conditional"
                } else {
                    "optional"
                }
            );
        }
        for item in strings(entry, "not_verified") {
            assert!(
                item == "*" || listed.contains(&item),
                "`{kind}`: `not_verified` names `{item}`, which the schema does not list"
            );
        }
    }
}

#[test]
fn the_types_refuse_what_is_not_the_documented_shape() {
    let refused = |kind: &str, value: Value| wire::validate_kind(kind, &value).is_err();
    assert!(refused("session/prompt.params", serde_json::json!({})));
    assert!(refused(
        "session/update:tool_call",
        serde_json::json!({"update": {"sessionUpdate": "tool_call"}})
    ));
    // The discriminator decides the kind.
    assert!(refused(
        "session/update:plan",
        serde_json::json!({"update": {"sessionUpdate": "agent_message_chunk", "content": {}}})
    ));
    assert!(refused(
        "session/request_permission.response",
        serde_json::json!({"outcome": {"outcome": "maybe"}})
    ));
    assert!(refused(
        "session/request_permission.response",
        serde_json::json!({"outcome": {"outcome": "selected"}})
    ));
    assert!(
        wire::validate_kind(
            "session/request_permission.response",
            &serde_json::json!({"outcome": {"outcome": "cancelled"}})
        )
        .is_ok()
    );
    assert!(refused("no/such/kind", serde_json::json!({})));
}

#[test]
fn every_message_of_every_transcript_is_accepted_by_the_types() {
    let schema = schema();
    let known = messages(&schema);
    let dir = version_dir().join("sessions");
    let mut checked = 0;
    let mut files = 0;
    // Fragments (`turn1_end.jsonl`) reply to a request deferred in another file: the
    // names of deferred requests are global to the directory.
    let mut deferred: BTreeMap<String, String> = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).expect("the sessions directory") {
        let text = std::fs::read_to_string(entry.expect("an entry").path()).unwrap_or_default();
        for line in text.lines() {
            if let Ok(directive) = serde_json::from_str::<Value>(line.trim())
                && let Some(name) = directive["defer"].as_str()
            {
                let method = directive["method"].as_str().unwrap_or_default();
                let previous = deferred.insert(name.to_owned(), method.to_owned());
                assert!(
                    previous.is_none_or(|previous| previous == method),
                    "`{name}` is deferred for two methods"
                );
            }
        }
    }
    for entry in std::fs::read_dir(&dir).expect("the sessions directory") {
        let path = entry.expect("an entry").path();
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        files += 1;
        let text = std::fs::read_to_string(&path).expect("a readable transcript");
        let mut request_methods: BTreeMap<String, String> = BTreeMap::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            let where_ = format!("{}:{}", path.display(), number + 1);
            let directive: Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("{where_}: not JSON: {error}"));
            let method = directive["method"].as_str().unwrap_or_default();
            let mut check = |kind: String, value: &Value| {
                assert!(
                    known.contains_key(&kind),
                    "{where_}: `{kind}` is not in the schema"
                );
                if let Err(reason) = wire::validate_kind(&kind, value) {
                    panic!("{where_}: the types refuse this `{kind}`: {reason}");
                }
                checked += 1;
            };
            match directive["op"].as_str().unwrap_or_default() {
                "expect" => {
                    if directive["defer"].is_string() {
                        // Answered by a later `reply`.
                    } else if directive.get("reply").is_some() && method != "session/set_mode" {
                        // `session/set_mode` answers an empty object: no kind of its own.
                        check(format!("{method}.result"), &directive["reply"]);
                    }
                },
                "reply" if directive.get("result").is_some() => {
                    let name = directive["to"].as_str().unwrap_or_default();
                    let method = deferred
                        .get(name)
                        .unwrap_or_else(|| panic!("{where_}: a reply to `{name}`, never deferred"));
                    check(format!("{method}.result"), &directive["result"]);
                },
                "notify" if directive["unsupported"] == true => {
                    let kind = format!(
                        "{method}:{}",
                        directive["params"]["update"]["sessionUpdate"]
                            .as_str()
                            .unwrap_or_default()
                    );
                    assert!(!known.contains_key(&kind), "{where_}: `{kind}` is listed");
                },
                "notify" => {
                    let kind = match directive["params"]["update"]["sessionUpdate"].as_str() {
                        Some(update) => format!("{method}:{update}"),
                        None => method.to_owned(),
                    };
                    check(kind, &directive["params"]);
                },
                "server_request" if directive["unsupported"] == true => {
                    // A request the adapter does not serve: the schema must not list it.
                    assert!(
                        !known.contains_key(&format!("{method}.params")),
                        "{where_}: `{method}` is listed"
                    );
                },
                "server_request" => {
                    request_methods.insert(directive["id"].to_string(), method.to_owned());
                    check(format!("{method}.params"), &directive["params"]);
                },
                "await_response" => {
                    if let Some(expected) = directive.get("expect") {
                        let key = directive["id"].to_string();
                        let request = request_methods.get(&key).unwrap_or_else(|| {
                            panic!("{where_}: the answer to an unknown request")
                        });
                        check(format!("{request}.response"), expected);
                    }
                },
                _ => {},
            }
        }
    }
    assert!(
        files >= 25,
        "found only {files} transcripts in {}",
        dir.display()
    );
    assert!(checked >= 80, "only {checked} messages were checked");
}

#[test]
fn the_schema_says_where_it_comes_from_and_that_it_was_not_generated() {
    let schema = schema();
    assert_eq!(schema["acp_protocol_version"], SUPPORTED_PROTOCOL_VERSION);
    assert_eq!(schema["generated_by_agent"], false);
    assert_eq!(schema["derived_from"], "specification");
    let provenance = std::fs::read_to_string(version_dir().join("schema/PROVENANCE.md"))
        .expect("PROVENANCE.md sits next to the schema");
    assert!(provenance.contains("NOT from a session with a real"));
    assert!(provenance.contains("agentclientprotocol.com"));
    assert!(provenance.contains("NOT VERIFIED"));
    assert!(provenance.contains("opencode and Gemini CLI were not executed"));
}
