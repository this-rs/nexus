//! Every control JSON line the adapter writes to the Claude Code CLI, and the
//! parsing of the control requests the CLI sends (contract §15).
//!
//! This module is the **only** place where that JSON is written. The shapes are
//! the ones the orchestrator wrote by hand before the contract existed; the
//! conformance tests compare them byte for byte with what a fake CLI receives.
//! Key order is `serde_json`'s without `preserve_order`: alphabetical.

use serde_json::{Value, json};

use crate::errors::SdkError;
use crate::interactive::{InteractiveClient, build_hook_response_json, is_hook_callback};
use crate::types::HookJSONOutput;

/// Message of a denial when the caller gives none (§15.2).
pub const DEFAULT_DENY_MESSAGE: &str = "User denied the permission request";

/// Name of the CLI's "ask the user a question" tool.
pub const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";

fn control_response(request_id: &str, response: Value) -> String {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        }
    })
    .to_string()
}

/// §15.1 — allows a `can_use_tool` request. `updated_input` is the input the tool
/// runs with: the caller's replacement, or the original input replayed.
///
/// Also the form of §15.6 (automatic answer to `AskUserQuestion`), with the
/// question's own input.
pub fn permission_allow(request_id: &str, updated_input: &Value) -> String {
    control_response(
        request_id,
        json!({ "behavior": "allow", "updatedInput": updated_input }),
    )
}

/// §15.1 with `updatedPermissions`: an approval that outlives the call (scopes
/// `session` and `always`). `updated_permissions` is the list of permission
/// updates to apply, see [`scoped_permission_updates`].
pub fn permission_allow_with_updates(
    request_id: &str,
    updated_input: &Value,
    updated_permissions: &Value,
) -> String {
    control_response(
        request_id,
        json!({
            "behavior": "allow",
            "updatedInput": updated_input,
            "updatedPermissions": updated_permissions,
        }),
    )
}

/// Rewrites the CLI's `permission_suggestions` for a destination (`session` for
/// the scope `session`, `localSettings` for `always`). `None` when the CLI
/// suggested nothing usable: the approval is then a plain §15.1.
pub fn scoped_permission_updates(suggestions: Option<&Value>, destination: &str) -> Option<Value> {
    let updates: Vec<Value> = suggestions?
        .as_array()?
        .iter()
        .filter(|suggestion| suggestion.is_object())
        .map(|suggestion| {
            let mut update = suggestion.clone();
            update["destination"] = json!(destination);
            update
        })
        .collect();
    (!updates.is_empty()).then_some(Value::Array(updates))
}

/// §15.2 — denies a `can_use_tool` request. `None` gives [`DEFAULT_DENY_MESSAGE`].
pub fn permission_deny(request_id: &str, message: Option<&str>) -> String {
    control_response(
        request_id,
        json!({
            "behavior": "deny",
            "message": message.unwrap_or(DEFAULT_DENY_MESSAGE),
        }),
    )
}

/// §15.3 — asks the CLI to change permission mode. `mode` is the CLI's own mode
/// name (see [`super::policy_map`]).
pub fn set_permission_mode(mode: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": uuid::Uuid::new_v4().to_string(),
        "request": { "subtype": "set_permission_mode", "mode": mode },
    })
    .to_string()
}

/// §15.4 — asks the CLI to change model.
pub fn set_model(model: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": uuid::Uuid::new_v4().to_string(),
        "request": { "subtype": "set_model", "model": model },
    })
    .to_string()
}

/// §15.5 — interrupts the running turn. The shape is
/// [`InteractiveClient::build_interrupt_json`]'s, reused as is.
pub fn interrupt() -> String {
    InteractiveClient::build_interrupt_json()
}

/// §15.7 — answers a `hook_callback`. The shape is
/// [`build_hook_response_json`]'s, reused as is.
pub fn hook_response(request_id: &str, output: &Result<HookJSONOutput, SdkError>) -> String {
    build_hook_response_json(request_id, output)
}

/// A `can_use_tool` control request of the CLI, typed.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    /// Identifier to answer with. Read at the **root** of the control message
    /// (`request_id` or `requestId`); empty when the CLI gave none.
    pub request_id: String,
    /// Tool the CLI wants to run (`tool_name` or `toolName`; `"unknown"` when absent).
    pub tool_name: String,
    /// Input of the tool (`{}` when absent).
    pub input: Value,
    /// The `tool_use` block concerned (`tool_use_id` or `toolUseId`).
    pub tool_use_id: Option<String>,
    /// Permission updates the CLI suggests for a lasting approval
    /// (`permission_suggestions` or `permissionSuggestions`).
    pub permission_suggestions: Option<Value>,
}

impl PermissionRequest {
    /// Whether the request is the CLI's question tool, which is answered by the
    /// adapter itself (§15.6) and surfaced as a `question` event.
    pub fn is_question(&self) -> bool {
        self.tool_name == ASK_USER_QUESTION_TOOL
    }
}

/// What a control message received from the CLI is.
#[derive(Debug, Clone, PartialEq)]
pub enum InboundControl {
    /// A permission prompt (or the question tool).
    CanUseTool(PermissionRequest),
    /// A hook to run; answered with [`hook_response`].
    HookCallback {
        /// Identifier to answer with.
        request_id: String,
    },
    /// Anything else: the acknowledgement of one of our own requests, or a
    /// subtype this adapter does not handle.
    Other,
}

/// The payload of a control message: its `request` object when there is one,
/// the message itself otherwise (flat variant of the protocol).
fn request_data(message: &Value) -> &Value {
    message
        .get("request")
        .filter(|request| request.is_object())
        .unwrap_or(message)
}

fn string_at(value: &Value, snake: &str, camel: &str) -> Option<String> {
    value
        .get(snake)
        .or_else(|| value.get(camel))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Identifier of a control request: at the root (`request_id` / `requestId`),
/// else under `request`. Empty when absent.
pub fn request_id_of(message: &Value) -> String {
    string_at(message, "request_id", "requestId")
        .or_else(|| {
            message
                .get("request")
                .and_then(|request| string_at(request, "request_id", "requestId"))
        })
        .unwrap_or_default()
}

/// Parses a `can_use_tool` control request; `None` for any other message.
pub fn parse_can_use_tool(message: &Value) -> Option<PermissionRequest> {
    let data = request_data(message);
    if data.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
        return None;
    }
    Some(PermissionRequest {
        request_id: string_at(message, "request_id", "requestId").unwrap_or_default(),
        tool_name: string_at(data, "tool_name", "toolName").unwrap_or_else(|| "unknown".to_owned()),
        input: data.get("input").cloned().unwrap_or_else(|| json!({})),
        tool_use_id: string_at(data, "tool_use_id", "toolUseId"),
        permission_suggestions: data
            .get("permission_suggestions")
            .or_else(|| data.get("permissionSuggestions"))
            .filter(|suggestions| !suggestions.is_null())
            .cloned(),
    })
}

/// Classifies a control message received from the CLI.
pub fn parse_inbound(message: &Value) -> InboundControl {
    if is_hook_callback(message) {
        return InboundControl::HookCallback {
            request_id: request_id_of(message),
        };
    }
    match parse_can_use_tool(message) {
        Some(request) => InboundControl::CanUseTool(request),
        None => InboundControl::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replaces the random `request_id` of a control request with `<uuid>`,
    /// after checking that it is one.
    fn mask_uuid(line: &str) -> String {
        let value: Value = serde_json::from_str(line).unwrap();
        let id = value
            .get("request_id")
            .or_else(|| value["request"].get("request_id"))
            .and_then(Value::as_str)
            .expect("a control request carries a request_id")
            .to_owned();
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id} is not a UUID");
        line.replace(&id, "<uuid>")
    }

    #[test]
    fn permission_allow_is_byte_for_byte_the_documented_shape() {
        assert_eq!(
            permission_allow("req-1", &json!({"command": "ls", "a": 1})),
            r#"{"response":{"request_id":"req-1","response":{"behavior":"allow","updatedInput":{"a":1,"command":"ls"}},"subtype":"success"},"type":"control_response"}"#
        );
        // An unknown original input is replayed as an empty object.
        assert_eq!(
            permission_allow("req-2", &json!({})),
            r#"{"response":{"request_id":"req-2","response":{"behavior":"allow","updatedInput":{}},"subtype":"success"},"type":"control_response"}"#
        );
    }

    #[test]
    fn permission_deny_is_byte_for_byte_the_documented_shape() {
        assert_eq!(
            permission_deny("req-1", None),
            r#"{"response":{"request_id":"req-1","response":{"behavior":"deny","message":"User denied the permission request"},"subtype":"success"},"type":"control_response"}"#
        );
        assert_eq!(
            permission_deny("req-1", Some("not on my watch")),
            r#"{"response":{"request_id":"req-1","response":{"behavior":"deny","message":"not on my watch"},"subtype":"success"},"type":"control_response"}"#
        );
    }

    #[test]
    fn set_permission_mode_is_byte_for_byte_the_documented_shape() {
        assert_eq!(
            mask_uuid(&set_permission_mode("acceptEdits")),
            r#"{"request":{"mode":"acceptEdits","subtype":"set_permission_mode"},"request_id":"<uuid>","type":"control_request"}"#
        );
    }

    #[test]
    fn set_model_is_byte_for_byte_the_documented_shape() {
        assert_eq!(
            mask_uuid(&set_model("claude-opus-4")),
            r#"{"request":{"model":"claude-opus-4","subtype":"set_model"},"request_id":"<uuid>","type":"control_request"}"#
        );
    }

    #[test]
    fn interrupt_is_byte_for_byte_the_documented_shape() {
        assert_eq!(
            mask_uuid(&interrupt()),
            r#"{"request":{"request_id":"<uuid>","type":"interrupt"},"type":"control_request"}"#
        );
        assert_ne!(interrupt(), interrupt(), "each interrupt has its own id");
    }

    #[test]
    fn a_lasting_approval_carries_the_suggestions_for_its_destination() {
        let suggestions = json!([
            {"type": "addRules", "rules": [{"toolName": "Bash"}], "behavior": "allow", "destination": "localSettings"},
            "not an update"
        ]);
        let updates = scoped_permission_updates(Some(&suggestions), "session").unwrap();
        assert_eq!(
            permission_allow_with_updates("r", &json!({"x": 1}), &updates),
            r#"{"response":{"request_id":"r","response":{"behavior":"allow","updatedInput":{"x":1},"updatedPermissions":[{"behavior":"allow","destination":"session","rules":[{"toolName":"Bash"}],"type":"addRules"}]},"subtype":"success"},"type":"control_response"}"#
        );
        assert_eq!(scoped_permission_updates(None, "session"), None);
        assert_eq!(scoped_permission_updates(Some(&json!([])), "session"), None);
        assert_eq!(
            scoped_permission_updates(Some(&json!("nope")), "session"),
            None
        );
    }

    #[test]
    fn hook_response_is_the_existing_builder() {
        let output = Ok(HookJSONOutput::Sync(Default::default()));
        assert_eq!(
            hook_response("h1", &output),
            build_hook_response_json("h1", &output)
        );
        assert_eq!(
            hook_response("h1", &output),
            r#"{"response":{"request_id":"h1","response":{},"subtype":"success"},"type":"control_response"}"#
        );
    }

    #[test]
    fn can_use_tool_is_read_nested_with_the_id_at_the_root() {
        let message = json!({
            "type": "control_request",
            "request_id": "root-id",
            "request": {
                "subtype": "can_use_tool",
                "request_id": "nested-id-is-not-read",
                "tool_name": "Bash",
                "input": {"command": "ls"},
                "tool_use_id": "toolu_1",
                "permission_suggestions": [{"type": "addRules"}],
            }
        });
        assert_eq!(
            parse_can_use_tool(&message),
            Some(PermissionRequest {
                request_id: "root-id".into(),
                tool_name: "Bash".into(),
                input: json!({"command": "ls"}),
                tool_use_id: Some("toolu_1".into()),
                permission_suggestions: Some(json!([{"type": "addRules"}])),
            })
        );
        assert!(matches!(
            parse_inbound(&message),
            InboundControl::CanUseTool(_)
        ));
    }

    #[test]
    fn can_use_tool_is_read_flat_and_in_camel_case_with_defaults() {
        let message = json!({
            "requestId": "r9",
            "subtype": "can_use_tool",
            "toolName": "AskUserQuestion",
            "toolUseId": "toolu_q",
        });
        let request = parse_can_use_tool(&message).unwrap();
        assert_eq!(request.request_id, "r9");
        assert_eq!(request.tool_name, "AskUserQuestion");
        assert_eq!(request.input, json!({}));
        assert_eq!(request.tool_use_id.as_deref(), Some("toolu_q"));
        assert_eq!(request.permission_suggestions, None);
        assert!(request.is_question());

        let bare = parse_can_use_tool(&json!({"request": {"subtype": "can_use_tool"}})).unwrap();
        assert_eq!(bare.request_id, "");
        assert_eq!(bare.tool_name, "unknown");
        assert!(!bare.is_question());
    }

    #[test]
    fn other_control_messages_are_classified() {
        assert_eq!(
            parse_can_use_tool(&json!({"request": {"subtype": "mcp_message"}})),
            None
        );
        assert_eq!(
            parse_inbound(&json!({"type": "control_response", "response": {"subtype": "success"}})),
            InboundControl::Other
        );
        assert_eq!(
            parse_inbound(&json!({
                "type": "control_request",
                "request_id": "h7",
                "request": {"subtype": "hook_callback", "callback_id": "cb"}
            })),
            InboundControl::HookCallback {
                request_id: "h7".into()
            }
        );
        assert_eq!(
            request_id_of(&json!({"request": {"requestId": "nested"}})),
            "nested"
        );
        assert_eq!(request_id_of(&json!({})), "");
    }
}
