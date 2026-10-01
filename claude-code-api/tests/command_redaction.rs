//! Secrets must never reach a log when the gateway spawns the CLI (bug S2).
//!
//! # The defect these tests pin
//!
//! `ClaudeManager::create_session_with_message` passed the MCP configuration
//! to the CLI as `--mcp-config <json>` and then logged the whole command with
//! `info!("... with command: {:?}", cmd)`. `Debug` for a `Command` prints
//! every argument and every environment value, so that JSON — which holds
//! each MCP server's `env` and headers, in practice a database password, a
//! search key and a session token — was written verbatim at `info` level, the
//! default for this service.
//!
//! The same line existed in `ClaudeManager::create_interactive_session`, in
//! `InteractiveSessionManager::create_session`, and in the SDK's own
//! `query.rs`. The repair in `dc510d5` had fixed only the SDK's subprocess
//! transport, which is why the lesson is written down here: a
//! "secrets out of logs" fix is not done until every place that logs a
//! `Command` has been found.
//!
//! # What is asserted
//!
//! Not "the code calls the redacting helper" — that would pass if the helper
//! were a no-op. These build commands shaped exactly as the three gateway
//! sites build them, with a recognisable secret inside, and assert the secret
//! is absent from the string that gets logged while the detail a person needs
//! for debugging survives.

use nexus_claude::{SECRET_BEARING_ARGS, describe_command_redacted};
use std::process::Command;

/// A value no legitimate description would contain by accident.
const SECRET: &str = "pg-password-cEaa41f6Qv-and-token-sk-ant-9z8y7x";

/// The shape `ClaudeManager::create_session_with_message` builds: the MCP
/// configuration inline as JSON, which is the documented way to configure
/// servers and the form that carries credentials.
fn command_with_inline_mcp_config() -> Command {
    let config_json = format!(
        r#"{{"mcpServers":{{"orchestrator":{{"command":"po","env":{{"DATABASE_URL":"{SECRET}"}}}}}}}}"#
    );
    let mut cmd = Command::new("claude");
    cmd.arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--mcp-config")
        .arg(config_json)
        .arg("--strict-mcp-config");
    cmd
}

#[test]
fn an_inline_mcp_config_never_reaches_the_log() {
    let cmd = command_with_inline_mcp_config();
    let described = describe_command_redacted(&cmd);
    assert!(
        !described.contains(SECRET),
        "the MCP configuration reached the log: {described}"
    );
    assert!(
        described.contains("<redacted"),
        "the value should be reported as redacted, not dropped silently: {described}"
    );
}

/// The control that proves the test above is not vacuous.
///
/// `Debug` is what the three sites used before the fix. If this ever stops
/// exposing the secret, the assertion above has stopped meaning anything and
/// the shape of this fixture needs revisiting — not the assertion relaxing.
#[test]
fn debug_formatting_is_what_leaked_and_still_would() {
    let cmd = command_with_inline_mcp_config();
    let via_debug = format!("{cmd:?}");
    assert!(
        via_debug.contains(SECRET),
        "if Debug no longer prints arguments, re-examine why the redacted \
         description is still needed rather than deleting these tests"
    );
}

#[test]
fn the_redacted_description_keeps_what_debugging_needs() {
    let cmd = command_with_inline_mcp_config();
    let described = describe_command_redacted(&cmd);
    for expected in ["claude", "--output-format", "stream-json", "--mcp-config"] {
        assert!(
            described.contains(expected),
            "{expected:?} is needed to debug a launch and must survive redaction: {described}"
        );
    }
}

/// The shape `InteractiveSessionManager::create_session` builds: the MCP
/// configuration as a path rather than inline.
///
/// A path is not a credential, but it is still the value of a
/// secret-bearing argument, so it is redacted too. Treating the argument
/// rather than guessing at the value is what keeps the rule simple enough to
/// hold.
#[test]
fn a_config_path_is_redacted_as_well_as_inline_json() {
    let mut cmd = Command::new("claude");
    cmd.arg("--mcp-config").arg("/tmp/mcp-config-abc123.json");
    let described = describe_command_redacted(&cmd);
    assert!(
        !described.contains("mcp-config-abc123.json"),
        "the value after a secret-bearing argument must not be logged: {described}"
    );
}

#[test]
fn environment_values_are_replaced_by_their_names() {
    // `Debug` for a Command prints env values too, so a token passed through
    // the environment leaked by the same line.
    let mut cmd = Command::new("claude");
    cmd.env("ANTHROPIC_API_KEY", SECRET);
    let described = describe_command_redacted(&cmd);
    assert!(
        !described.contains(SECRET),
        "an environment value reached the log: {described}"
    );
    assert!(
        described.contains("ANTHROPIC_API_KEY"),
        "the NAME is useful and safe, and should survive: {described}"
    );
}

#[test]
fn a_command_carrying_no_secret_is_described_in_full() {
    // Redaction must not be so broad that the description stops being useful.
    let mut cmd = Command::new("claude");
    cmd.arg("--model").arg("claude-opus-5");
    let described = describe_command_redacted(&cmd);
    assert!(described.contains("claude-opus-5"), "{described}");
    assert!(!described.contains("<redacted"), "{described}");
}

#[test]
fn the_secret_bearing_argument_list_is_published_and_not_empty() {
    // Exported so both crates share one list. An empty list would silently
    // disable every redaction above.
    assert!(SECRET_BEARING_ARGS.contains(&"--mcp-config"));
    assert!(!SECRET_BEARING_ARGS.is_empty());
}
