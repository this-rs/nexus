//! What the adapter SENDS, checked against the REAL schema of `codex app-server`
//! 0.160.0, offline; and what the real server ANSWERED, read by our types.
//!
//! `codex_schema_drift.rs` compares our types to a schema that was derived from the
//! documentation (0.130.0). That is how `sandbox: "workspaceWrite"` stayed wrong: the
//! types, the schema and the fake agreed with each other and with nothing real. A real
//! `app-server` refused it (`tests/codex_real.rs` found it). The files under
//! `tests/transcripts/codex/0.160.0/` come from a REAL Codex:
//!
//! - `schema/*.json`: `codex app-server generate-json-schema`, the request parameters
//!   the adapter builds (see `PROVENANCE.md` there);
//! - `real/*.json`: the real answers to `initialize` and `thread/start`, with paths and
//!   identifiers normalised.
//!
//! This file needs no Codex installed: it is the offline half of `codex_real.rs`.

use std::path::PathBuf;

use nexus_claude::providers::codex::wire::{
    ApprovalPolicy, InitializeParams, InitializeResult, SandboxMode, SandboxPolicy, ThreadResult,
    ThreadResumeParams, ThreadStartParams, TurnInterruptParams, TurnStartParams, UserInput,
};
use serde_json::{Value, json};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/transcripts/codex/0.160.0")
}

fn read(path: &str) -> Value {
    let full = dir().join(path);
    let text = std::fs::read_to_string(&full)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", full.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", full.display()))
}

// ---------------------------------------------------------------------------
// A small JSON-schema checker: just what the real schema uses for these messages
// ---------------------------------------------------------------------------

fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(Value::as_str) {
        Some(reference) => {
            let name = reference.rsplit('/').next().unwrap_or_default();
            root["definitions"]
                .get(name)
                .or_else(|| root["$defs"].get(name))
                .unwrap_or_else(|| panic!("unresolved {reference}"))
        },
        None => schema,
    }
}

/// `Ok` when `instance` satisfies `schema`, else why not.
fn check(root: &Value, schema: &Value, instance: &Value, path: &str) -> Result<(), String> {
    let schema = resolve(root, schema);
    for key in ["allOf"] {
        if let Some(all) = schema.get(key).and_then(Value::as_array) {
            for part in all {
                check(root, part, instance, path)?;
            }
        }
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(any) = schema.get(key).and_then(Value::as_array) {
            let mut reasons = Vec::new();
            for part in any {
                match check(root, part, instance, path) {
                    Ok(()) => return Ok(()),
                    Err(reason) => reasons.push(reason),
                }
            }
            return Err(format!(
                "{path}: none of the alternatives accept it: {reasons:?}"
            ));
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(instance)
    {
        return Err(format!("{path}: {instance} is not one of {allowed:?}"));
    }
    if let Some(constant) = schema.get("const")
        && constant != instance
    {
        return Err(format!("{path}: {instance} is not {constant}"));
    }
    if let Some(ty) = schema.get("type") {
        let types: Vec<&str> = match ty {
            Value::String(one) => vec![one.as_str()],
            Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let fits = |t: &&str| match *t {
            "string" => instance.is_string(),
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "boolean" => instance.is_boolean(),
            "integer" => instance.is_i64() || instance.is_u64(),
            "number" => instance.is_number(),
            "null" => instance.is_null(),
            _ => true,
        };
        if !types.is_empty() && !types.iter().any(fits) {
            return Err(format!("{path}: {instance} is not of type {types:?}"));
        }
    }
    if let Some(object) = instance.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return Err(format!("{path}: required `{name}` is missing"));
                }
            }
        }
        for (name, value) in object {
            match properties.and_then(|p| p.get(name)) {
                Some(sub) => check(root, sub, value, &format!("{path}.{name}"))?,
                None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    return Err(format!(
                        "{path}: `{name}` is not a field of the real schema"
                    ));
                },
                None => {},
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), instance.as_array()) {
        for (index, element) in array.iter().enumerate() {
            check(root, items, element, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

fn must_satisfy(schema_file: &str, sent: &Value) {
    let root = read(&format!("schema/{schema_file}"));
    if let Err(reason) = check(&root, &root, sent, "$") {
        panic!("what the adapter sends does not satisfy the real {schema_file}: {reason}\n{sent}");
    }
}

fn must_not_satisfy(schema_file: &str, sent: &Value) {
    let root = read(&format!("schema/{schema_file}"));
    assert!(
        check(&root, &root, sent, "$").is_err(),
        "the checker accepted {sent} against {schema_file}: it cannot fail, so it proves nothing"
    );
}

// ---------------------------------------------------------------------------
// The checker can fail — and it fails on exactly the bug the real server found
// ---------------------------------------------------------------------------

#[test]
fn the_checker_rejects_the_spelling_the_real_server_refused() {
    must_not_satisfy(
        "ThreadStartParams.json",
        &json!({"cwd": "/w", "approvalPolicy": "on-request", "sandbox": "workspaceWrite"}),
    );
    must_not_satisfy(
        "ThreadStartParams.json",
        &json!({"cwd": "/w", "approvalPolicy": "onRequest", "sandbox": "workspace-write"}),
    );
    // And it accepts the spelling the real server accepted.
    must_satisfy(
        "ThreadStartParams.json",
        &json!({"cwd": "/w", "approvalPolicy": "on-request", "sandbox": "workspace-write"}),
    );
}

// ---------------------------------------------------------------------------
// What the adapter sends
// ---------------------------------------------------------------------------

#[test]
fn every_thread_start_the_adapter_can_send_satisfies_the_real_schema() {
    for approval in [ApprovalPolicy::OnRequest, ApprovalPolicy::Never] {
        for sandbox in [SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite] {
            let params = ThreadStartParams {
                cwd: "/work".into(),
                model: Some("gpt-test".into()),
                approval_policy: approval,
                sandbox,
                base_instructions: Some("Be terse.".into()),
                developer_instructions: Some("Small diffs.".into()),
            };
            must_satisfy(
                "ThreadStartParams.json",
                &serde_json::to_value(params).unwrap(),
            );
        }
    }
}

#[test]
fn thread_resume_turn_start_and_interrupt_satisfy_the_real_schema() {
    let resume = ThreadResumeParams {
        thread_id: "thr_1".into(),
        cwd: Some("/work".into()),
        model: Some("gpt-test".into()),
        approval_policy: Some(ApprovalPolicy::OnRequest),
        sandbox: Some(SandboxMode::WorkspaceWrite),
    };
    must_satisfy(
        "ThreadResumeParams.json",
        &serde_json::to_value(resume).unwrap(),
    );

    for policy in [
        SandboxPolicy::ReadOnly,
        SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
        },
        SandboxPolicy::WorkspaceWrite {
            writable_roots: vec!["/extra".into()],
        },
    ] {
        let turn = TurnStartParams {
            thread_id: "thr_1".into(),
            input: vec![UserInput::Text {
                text: "hello".into(),
            }],
            model: Some("gpt-test".into()),
            approval_policy: Some(ApprovalPolicy::Never),
            sandbox_policy: Some(policy),
        };
        must_satisfy("TurnStartParams.json", &serde_json::to_value(turn).unwrap());
    }

    let interrupt = TurnInterruptParams {
        thread_id: "thr_1".into(),
        turn_id: "turn_1".into(),
    };
    must_satisfy(
        "TurnInterruptParams.json",
        &serde_json::to_value(interrupt).unwrap(),
    );
}

#[test]
fn the_initialize_the_adapter_sends_satisfies_the_real_schema() {
    must_satisfy(
        "InitializeParams.json",
        &serde_json::to_value(InitializeParams::for_adapter()).unwrap(),
    );
}

// ---------------------------------------------------------------------------
// What the real server answered
// ---------------------------------------------------------------------------

#[test]
fn our_types_read_the_real_initialize_answer() {
    let real = read("real/initialize_result.json");
    let result: InitializeResult = serde_json::from_value(real).expect("the real answer parses");
    assert!(result.user_agent.contains("0.160.0"), "{result:?}");
    assert!(result.codex_home.is_some());
}

#[test]
fn our_types_read_the_real_thread_start_answer() {
    let real = read("real/thread_start_result.json");
    let result: ThreadResult =
        serde_json::from_value(real.clone()).expect("the real answer parses");
    assert!(!result.thread.id.is_empty());
    assert_eq!(result.model.as_deref(), real["model"].as_str());
    // The response reports the sandbox as a tagged object, not the request's string: a
    // type that read the response with the request's enum would have refused it.
    assert_eq!(real["sandbox"]["type"], "workspaceWrite");
    assert_eq!(real["approvalPolicy"], "on-request");
}
