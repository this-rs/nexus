//! A Claude Code instance on another machine, reached over SSH.
//!
//! No real `ssh`, no real machine: a stand-in `ssh` script plays the sshd side
//! (it runs the remote command line through `sh`, as a login shell would) in front
//! of `fake_claude`. What this proves is what only the remote path adds: the
//! channel carries the stream-json turn, the ssh line pins and switches off what it
//! must, a resume token names its machine, and everything that cannot or must not
//! cross is refused instead of dropped.
//!
//! NOT proved here: a real sshd, a real host key exchange, a real network.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, HealthStatus, McpServerSpec, PolicyMode,
    ProviderError, ResumeToken, SessionSpec, StopReason, ToolPolicy, TurnInput,
};
use nexus_claude::providers::claude_code::{ClaudeCodeConfig, ClaudeCodeProvider};
use nexus_claude::transport::RemoteHost;
use support::{FakeCli, Transcript};

const WAIT: Duration = Duration::from_secs(10);
const KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

/// A stand-in `ssh` in `dir`: records its arguments, exports `env` (what the real
/// remote login would provide), then runs the last argument through `sh -c`.
fn fake_ssh(dir: &Path, env: &[(String, String)]) -> PathBuf {
    let path = dir.join("ssh");
    let exports: String = env
        .iter()
        .map(|(k, v)| format!("export {k}='{}'\n", v.replace('\'', "'\\''")))
        .collect();
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{record}'\n{exports}for last; do :; done\nexec sh -c \"$last\"\n",
        record = dir.join("ssh-args.txt").display(),
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// An `ssh` that fails as ssh does: exit `code`, a message on stderr.
fn failing_ssh(dir: &Path, code: i32, stderr: &str) -> PathBuf {
    let path = dir.join("ssh-fail");
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho '{stderr}' >&2\nexit {code}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Staged {
    fake: FakeCli,
    provider: Arc<ClaudeCodeProvider>,
    spec: SessionSpec,
}

fn stage(transcript: Transcript, tweak: impl FnOnce(&mut RemoteHost)) -> Staged {
    let fake = transcript.build();
    let env: Vec<(String, String)> = fake.options().env.into_iter().collect();
    let mut remote = RemoteHost::new("build-1.example.net", KEY);
    remote.cli = fake.cli_path().display().to_string();
    remote.cwd = Some(fake.dir().display().to_string());
    remote.ssh_program = Some(fake_ssh(fake.dir(), &env));
    tweak(&mut remote);
    let mut config = ClaudeCodeConfig::default();
    config.remote = Some(remote);
    let spec = SessionSpec::new(fake.dir());
    Staged {
        provider: Arc::new(ClaudeCodeProvider::new(config)),
        fake,
        spec,
    }
}

async fn run_turn(session: &Arc<dyn AgentSession>, text: &str) -> Vec<AgentEvent> {
    let mut stream = session.send_turn(TurnInput::text(text)).await.unwrap();
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("in time")
    {
        events.push(event);
    }
    events
}

fn ssh_args(fake: &FakeCli) -> Vec<String> {
    std::fs::read_to_string(fake.dir().join("ssh-args.txt"))
        .expect("the stand-in ssh recorded its arguments")
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn a_turn_runs_through_ssh_and_the_token_names_the_machine() {
    let transcript = Transcript::new()
        .await_stdin_containing("hello remote")
        .init("remote-session-1")
        .assistant_text("hi from far away")
        .result_ok("done")
        .wait_eof_for(10_000);
    let s = stage(transcript, |_| {});
    let session = tokio::time::timeout(WAIT, s.provider.open(s.spec.clone()))
        .await
        .unwrap()
        .expect("a remote session opens");
    let events = run_turn(&session, "hello remote").await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Text { text, .. } if text == "hi from far away")),
        "the text crossed the ssh channel: {events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            stop_reason: StopReason::Completed,
            ..
        })
    ));

    let token = session
        .resume_token()
        .expect("a token after the first turn");
    assert!(
        token.data()["session_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(token.data()["machine"], "build-1.example.net:22");

    // The ssh line: pinned, non-interactive, the host after `--`, one command last.
    let args = ssh_args(&s.fake);
    for needed in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "ClearAllForwardings=yes",
    ] {
        assert!(
            args.iter().any(|a| a == needed),
            "{needed} missing in {args:?}"
        );
    }
    let dd = args.iter().position(|a| a == "--").unwrap();
    assert_eq!(args[dd + 1], "build-1.example.net");
    let command = &args[dd + 2];
    assert!(
        command.starts_with(&format!("cd '{}' && exec '", s.fake.dir().display())),
        "{command}"
    );
    assert!(
        command.contains("'--output-format' 'stream-json'"),
        "{command}"
    );
}

#[tokio::test]
async fn what_the_host_holds_secret_never_reaches_the_remote_command_line() {
    let transcript = Transcript::new()
        .init("s")
        .result_ok("done")
        .wait_eof_for(10_000);
    let mut s = stage(transcript, |_| {});
    s.spec.env.set.insert(
        "ANTHROPIC_API_KEY".into(),
        "sk-never-on-a-remote-argv".into(),
    );
    s.spec
        .env
        .set
        .insert("NEO4J_PASSWORD".into(), "pw-never-on-a-remote-argv".into());
    let session = s.provider.open(s.spec.clone()).await.expect("opens");
    let _ = session.send_turn(TurnInput::text("go")).await;
    // The stand-in ssh records its arguments as its first act: wait for the file, not a delay.
    let record = s.fake.dir().join("ssh-args.txt");
    let deadline = std::time::Instant::now() + WAIT;
    while !record.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let recorded = ssh_args(&s.fake).join("\n");
    assert!(
        !recorded.contains("never-on-a-remote-argv"),
        "a secret reached ssh: {recorded}"
    );
}

#[tokio::test]
async fn an_mcp_server_is_refused_not_dropped() {
    let mut s = stage(Transcript::new().wait_eof_for(10_000), |_| {});
    s.spec.mcp_servers.insert(
        "po".into(),
        McpServerSpec::Stdio {
            command: "/bin/po-mcp".into(),
            args: vec![],
            env: [("NEO4J_PASSWORD".to_string(), "s3cr3t".to_string())].into(),
        },
    );
    let err = s
        .provider
        .open(s.spec.clone())
        .await
        .err()
        .expect("refused");
    assert!(matches!(err, ProviderError::Unsupported { .. }), "{err:?}");
    assert!(
        !s.fake.dir().join("ssh-args.txt").exists(),
        "ssh must not even start"
    );
}

#[tokio::test]
async fn extra_local_directories_are_refused() {
    let mut s = stage(Transcript::new().wait_eof_for(10_000), |_| {});
    s.spec.extra_dirs = vec![PathBuf::from("/Users/me/other")];
    let err = s
        .provider
        .open(s.spec.clone())
        .await
        .err()
        .expect("refused");
    assert!(err.to_string().contains("local directories"), "{err}");
}

#[tokio::test]
async fn the_no_confirmation_mode_needs_the_machine_to_allow_it() {
    let mut s = stage(Transcript::new().wait_eof_for(10_000), |_| {});
    s.spec.policy = ToolPolicy::new(PolicyMode::Trust);
    let err = s
        .provider
        .open(s.spec.clone())
        .await
        .err()
        .expect("refused by default");
    assert!(err.to_string().contains("refused"), "{err}");
    assert!(!s.fake.dir().join("ssh-args.txt").exists());

    let mut allowed = stage(
        Transcript::new()
            .init("s")
            .result_ok("ok")
            .wait_eof_for(10_000),
        |r| r.allow_trust = true,
    );
    allowed.spec.policy = ToolPolicy::new(PolicyMode::Trust);
    allowed
        .provider
        .open(allowed.spec.clone())
        .await
        .expect("allowed for that machine");
}

#[tokio::test]
async fn a_session_belongs_to_its_machine() {
    let s = stage(Transcript::new().wait_eof_for(10_000), |_| {});
    // A token from the local machine, or from another one, is not resumable here.
    let local = ResumeToken::claude_code_session("abc");
    let other = ResumeToken::new(
        nexus_claude::agent::ProviderKind::ClaudeCode,
        1,
        serde_json::json!({"session_id": "abc", "machine": "someone@other.example.net:22"}),
    );
    for token in [local, other] {
        let err = s
            .provider
            .resume(s.spec.clone(), token)
            .await
            .err()
            .expect("refused");
        assert!(err.to_string().contains("belongs to"), "{err}");
    }
    assert!(
        !s.fake.dir().join("ssh-args.txt").exists(),
        "nothing was started"
    );
}

#[tokio::test]
async fn the_capabilities_say_what_a_remote_session_cannot_do() {
    let s = stage(Transcript::new(), |_| {});
    let caps = s.provider.capabilities(None);
    assert!(!caps.per_session_mcp, "no MCP server over SSH in v1");
    assert!(!caps.tool_cancel, "descendants are on the other machine");
    assert!(caps.secret_isolation);
    let local = ClaudeCodeProvider::new(ClaudeCodeConfig::default()).capabilities(None);
    assert!(
        local.per_session_mcp && local.tool_cancel,
        "the local instance is unchanged"
    );
}

#[tokio::test]
async fn health_reads_the_remote_version_through_the_pinned_channel() {
    let s = stage(Transcript::new(), |_| {});
    // Replace the stand-in by one that answers `--version`.
    let ssh = s.fake.dir().join("ssh-version");
    std::fs::write(&ssh, "#!/bin/sh\necho '2.1.287 (Claude Code)'\n").unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = ClaudeCodeConfig::default();
    let mut remote = RemoteHost::new("build-1.example.net", KEY);
    remote.ssh_program = Some(ssh);
    config.remote = Some(remote);
    let health = ClaudeCodeProvider::new(config).health().await;
    assert_eq!(health.status, HealthStatus::Ok, "{health:?}");
    assert_eq!(health.version.as_deref(), Some("2.1.287"));
}

#[tokio::test]
async fn an_unreachable_machine_is_unavailable_with_a_readable_reason_and_no_local_fallback() {
    let s = stage(Transcript::new(), |_| {});
    // The raw stderr carries a path and an address that must not be forwarded.
    let ssh = failing_ssh(
        s.fake.dir(),
        255,
        "Host key verification failed. (/home/secret-user/.ssh/known_hosts line 3, 10.9.8.7)",
    );
    let mut config = ClaudeCodeConfig::default();
    let mut remote = RemoteHost::new("build-1.example.net", KEY);
    remote.ssh_program = Some(ssh);
    config.remote = Some(remote);
    let health = ClaudeCodeProvider::new(config).health().await;
    assert_eq!(health.status, HealthStatus::Unavailable, "{health:?}");
    let shown = serde_json::to_string(&health).unwrap();
    assert!(shown.contains("does not match the pinned key"), "{shown}");
    assert!(
        !shown.contains("secret-user") && !shown.contains("10.9.8.7"),
        "raw stderr leaked: {shown}"
    );
}

#[tokio::test]
async fn a_missing_remote_cli_is_told_apart_from_an_unreachable_machine() {
    let s = stage(Transcript::new(), |_| {});
    let ssh = failing_ssh(s.fake.dir(), 127, "claude: command not found");
    let mut config = ClaudeCodeConfig::default();
    let mut remote = RemoteHost::new("build-1.example.net", KEY);
    remote.ssh_program = Some(ssh);
    config.remote = Some(remote);
    let health = ClaudeCodeProvider::new(config).health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    assert!(
        matches!(health.error, Some(ProviderError::CliNotFound { .. })),
        "{health:?}"
    );
}
