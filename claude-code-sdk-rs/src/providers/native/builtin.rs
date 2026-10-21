//! The tools the harness itself offers (P18, CTX2): `AskUserQuestion`, `Agent` and
//! `ToolSearch`, under the names and input shapes Claude Code gives them. None touches the
//! machine: one puts a question to the user, one hands a task to the host, one loads the
//! definitions of deferred tools.
//!
//! | Tool | Offered when | A call |
//! |---|---|---|
//! | `AskUserQuestion` | `NativeConfig::ask_user_question` and a model with tools (`native_question`) | emits `question { reply: turn }`, answers the model that the reply comes as the next user message, and **ends the turn** once the step's calls are done (decision A45: the answer is a user turn) |
//! | `Agent` | the session's hooks say `runs_subagents()`, a model with tools, and the session is not a child (`lineage.depth == 0` or no lineage) | awaits `SessionHooks::run_subagent` (decision A18: the host's child session), whose answer is the `tool_result`; with `runs_background_subagents()` the schema also offers `run_in_background` and the host answers at once (ISO2) |
//! | `ToolSearch` | `NativeConfig::deferred_tools` defers at least one tool the session has (see [`super::tools::is_deferred`]) | answers with the full definitions of the deferred tools the query names (`select:A,B`) or matches (keywords), which become **loaded** for the rest of the session: sent with every later request, appended after the tools already sent, callable in the same turn |
//!
//! Policy, as on Claude Code:
//! - a `deny` refuses them: a bare `Agent` (or `Task`, its other name) / `AskUserQuestion` /
//!   `ToolSearch` hides the tool; a pattern with an argument is judged at the call on Claude
//!   Code's argument, the `subagent_type` (`Agent(Explore)`), and fails closed when the call
//!   names none ([`denied`]); the input judged is the one `before_tool` left;
//! - an allow-list does not hide them (on Claude Code it only approves, and these tools ask
//!   nothing), and `plan_only` keeps them (neither changes anything by itself);
//! - `before_tool` / `after_tool` apply; a `before_tool` deny means no question and no sub-agent.
//!
//! Neither asks for a permission: the question is a question, and the sub-agent is bounded by
//! the host, whose child session is never more permissive than its parent (A17). The host also
//! bounds how many sub-agents run at once and what they spend: the harness passes every call
//! on, concurrently within a step, and does not count a sub-agent's cost in the parent's budget.

use serde_json::{Value, json};

use super::tools::ToolEntry;
use crate::agent::{QuestionSpec, SubagentRequest, ToolCategory, ToolPolicy};

/// Name of the question tool, as Claude Code names it.
pub const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";
/// Name of the sub-agent tool, as Claude Code names it.
pub const AGENT_TOOL: &str = "Agent";
/// The other name Claude Code gives the sub-agent tool: a policy pattern may use it.
pub const AGENT_ALIAS: &str = "Task";
/// Name of the tool that loads deferred tools, as Claude Code names it (CTX2).
pub const TOOL_SEARCH_TOOL: &str = "ToolSearch";

/// What the model reads back from a question it asked.
pub const QUESTION_ASKED: &str = "The question was shown to the user. Their answer will \
arrive as the next user message: end your turn now and do not guess it.";

/// A tool of the harness itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinTool {
    /// `AskUserQuestion`.
    AskUserQuestion,
    /// `Agent`.
    Agent,
    /// `ToolSearch`.
    ToolSearch,
}

/// The entry of `AskUserQuestion`.
pub fn ask_user_question_entry() -> ToolEntry {
    ToolEntry::builtin(
        BuiltinTool::AskUserQuestion,
        ASK_USER_QUESTION_TOOL,
        "Ask the user one to four questions when you need their decision or a fact only \
         they know (a preference, a choice between approaches, a missing requirement). Each \
         question has a short header and two to four options; the user may also answer in \
         their own words. The answer arrives as the next user message: after calling this \
         tool, end your turn."
            .to_owned(),
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string", "description": "The full question"},
                            "header": {"type": "string", "description": "A short label (12 characters at most)"},
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string"}
                                    },
                                    "required": ["label"]
                                }
                            },
                            "multiSelect": {"type": "boolean", "description": "Several options may be chosen"}
                        },
                        "required": ["question", "options"]
                    }
                }
            },
            "required": ["questions"]
        }),
        ToolCategory::Other,
    )
}

/// The entry of `Agent`.
/// `background`: the host runs sub-agents in the background too (ISO2), so `run_in_background`
/// is offered.
pub fn agent_entry(background: bool) -> ToolEntry {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "description": {"type": "string", "description": "A short (3-5 word) description of the task"},
            "prompt": {"type": "string", "description": "The task for the sub-agent, in full"},
            "subagent_type": {"type": "string", "description": "The kind of agent, if a particular one is wanted"}
        },
        "required": ["description", "prompt"]
    });
    if background {
        schema["properties"]["run_in_background"] = json!({
            "type": "boolean",
            "description": "Run the sub-agent in the background: the call answers at once that it was launched, you keep working, and its end reaches you as a message in this conversation"
        });
    }
    ToolEntry::builtin(
        BuiltinTool::Agent,
        AGENT_TOOL,
        "Hand a self-contained task to a sub-agent and get its final answer back. The \
         sub-agent starts with no memory of this conversation: the prompt must say everything \
         it needs (goal, files, constraints, what to report). It works in the same project with \
         permissions no wider than yours and cannot start sub-agents of its own. Use it for \
         searches or investigations that would fill your context, or for independent pieces of \
         work."
            .to_owned(),
        schema,
        ToolCategory::Agent,
    )
}

/// The entry of `ToolSearch` (CTX2), the same name and shape as Claude Code's.
pub fn tool_search_entry() -> ToolEntry {
    ToolEntry::builtin(
        BuiltinTool::ToolSearch,
        TOOL_SEARCH_TOOL,
        "Fetches full schema definitions for deferred tools so they can be called.\n\n\
         Deferred tools exist but their definitions are not loaded — they cannot be \
         called until this tool returns them. Once a tool's definition appears in a \
         result, it is loaded for the rest of the session and callable exactly like \
         any other tool.\n\n\
         Query forms:\n\
         - \"select:ToolA,ToolB\" — fetch these exact tools by name\n\
         - anything else — keyword search over tool names and descriptions, \
         up to max_results best matches"
            .to_owned(),
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Query to find deferred tools. Use \"select:<tool_name>\" for direct selection, or keywords to search."
                },
                "max_results": {
                    "type": "number",
                    "description": "Maximum number of results to return (default: 5)"
                }
            },
            "required": ["query"]
        }),
        ToolCategory::Other,
    )
}

/// The argument a policy pattern is matched against, in Claude Code's form: the
/// `subagent_type` of an `Agent` call (`Agent(Explore)`), nothing for a question
/// or a tool search.
pub fn primary_argument(kind: BuiltinTool, input: &Value) -> Option<String> {
    match kind {
        BuiltinTool::Agent => input
            .get("subagent_type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
            .map(str::to_owned),
        BuiltinTool::AskUserQuestion | BuiltinTool::ToolSearch => None,
    }
}

/// Whether a `deny` pattern of `policy` refuses this call: any of the tool's names (`Agent`
/// and its alias `Task`), the argument of [`primary_argument`], and fail closed when there is
/// none (`Agent(Explore)` refuses an `Agent` call that names no type). The rest of the policy
/// does not apply: these tools never ask for a permission, and `plan_only` keeps them.
pub fn denied(policy: &ToolPolicy, entry: &ToolEntry, input: &Value) -> bool {
    let Some(kind) = entry.builtin else {
        return false;
    };
    let arg = primary_argument(kind, input);
    entry.names().any(|name| {
        policy
            .deny
            .iter()
            .any(|pattern| pattern.matches_closed(name, arg.as_deref()))
    })
}

/// The questions of an `AskUserQuestion` input, or why the call is refused.
pub fn questions(input: &Value) -> Result<Vec<QuestionSpec>, String> {
    let questions = QuestionSpec::parse_list(input);
    if questions.is_empty() {
        return Err(
            "AskUserQuestion needs `questions`: a list of objects with a `question` string \
             and `options`"
                .to_owned(),
        );
    }
    Ok(questions)
}

/// The sub-agent request of an `Agent` input, or why the call is refused.
pub fn subagent_request(
    tool_call_id: &str,
    input: &Value,
    background_allowed: bool,
) -> Result<SubagentRequest, String> {
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let prompt = text("prompt")
        .ok_or_else(|| "Agent needs a `prompt`: the task for the sub-agent, in full".to_owned())?;
    let mut request = SubagentRequest::new(tool_call_id, prompt);
    request.description = text("description");
    request.subagent_type = text("subagent_type");
    // Only a host that said it runs them in the background gets the flag; a call that sets it
    // anyway (the schema did not offer it) runs in the foreground.
    request.run_in_background = background_allowed
        && input
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    request.input = input.clone();
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_call_needs_at_least_one_question() {
        assert!(questions(&json!({})).is_err());
        assert!(questions(&json!({"questions": [{"header": "no text"}]})).is_err());
        let parsed = questions(&json!({"questions": [{
            "question": "Which database?", "header": "DB",
            "options": [{"label": "Postgres"}, "SQLite"], "multiSelect": true
        }]}))
        .unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].options.len(), 2);
        assert!(parsed[0].multi_select);
    }

    #[test]
    fn an_agent_call_needs_a_prompt() {
        assert!(subagent_request("c1", &json!({"description": "x"}), false).is_err());
        assert!(subagent_request("c1", &json!({"prompt": "   "}), false).is_err());
        let request = subagent_request(
            "c1",
            &json!({"description": "find it", "prompt": "Find the spec", "subagent_type": "Explore"}),
            false,
        )
        .unwrap();
        assert_eq!(request.tool_call_id, "c1");
        assert_eq!(request.prompt, "Find the spec");
        assert_eq!(request.description.as_deref(), Some("find it"));
        assert_eq!(request.subagent_type.as_deref(), Some("Explore"));
    }

    #[test]
    fn the_builtin_tools_keep_claude_codes_names_and_are_read_only() {
        let ask = ask_user_question_entry();
        let agent = agent_entry(false);
        let search = tool_search_entry();
        assert_eq!(ask.name, "AskUserQuestion");
        assert_eq!(agent.name, "Agent");
        assert_eq!(search.name, "ToolSearch");
        assert_eq!(agent.category, ToolCategory::Agent);
        assert!(ask.read_only && agent.read_only && search.read_only);
        assert_eq!(ask.builtin, Some(BuiltinTool::AskUserQuestion));
        assert_eq!(agent.builtin, Some(BuiltinTool::Agent));
        assert_eq!(search.builtin, Some(BuiltinTool::ToolSearch));
        assert_eq!(agent.canonical.as_deref(), Some("Agent"));
        // The same shape as Claude Code: `query` required, `max_results` a number.
        assert_eq!(search.schema["required"], json!(["query"]));
        assert_eq!(search.schema["properties"]["query"]["type"], "string");
        assert_eq!(search.schema["properties"]["max_results"]["type"], "number");
    }
}
