//! `CodexProvider::catalog()` lists the models Codex serves (`model/list`) through a
//! short-lived `codex app-server`. Three promises are checked against `fake_codex`:
//!
//! * an abandoned listing (the caller's timeout drops the future) kills the
//!   `app-server` and its children: nothing is left running;
//! * a listing that does not finish within the internal budget yields the configured
//!   models in full (`default_model` included), never an error and never a wait the
//!   caller cannot afford;
//! * a codex older than `MIN_APP_SERVER_VERSION` is not started as `app-server`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nexus_claude::agent::AgentProvider;
use nexus_claude::providers::codex::{CodexConfig, CodexProvider};

const FAKE: &str = env!("CARGO_BIN_EXE_fake_codex");

/// Handshake answered, `model/list` never answered: the fake waits for its stdin to
/// close, and it records the `sleep` child it started.
const HANGING_LISTING: &str = r#"
{"op":"expect","method":"initialize","reply":{"userAgent":"fake_codex/0.130.0","codexHome":"/fake","platformFamily":"unix","platformOs":"linux"}}
{"op":"expect_notification","method":"initialized"}
{"op":"spawn_child","own_group":false}
{"op":"wait_eof"}
"#;

/// As `HANGING_LISTING`, but the `sleep` child is in its own process group: only
/// the descendant walk of the process table reaches it.
const HANGING_LISTING_OWN_GROUP: &str = r#"
{"op":"expect","method":"initialize","reply":{"userAgent":"fake_codex/0.130.0","codexHome":"/fake","platformFamily":"unix","platformOs":"linux"}}
{"op":"expect_notification","method":"initialized"}
{"op":"spawn_child","own_group":true}
{"op":"wait_eof"}
"#;

/// Handshake answered, `model/list` answered with one visible and one hidden model.
const ANSWERED_LISTING: &str = r#"
{"op":"expect","method":"initialize","reply":{"userAgent":"fake_codex/0.130.0","codexHome":"/fake","platformFamily":"unix","platformOs":"linux"}}
{"op":"expect_notification","method":"initialized"}
{"op":"expect","method":"model/list","reply":{"data":[{"model":"gpt-live-1"},{"model":"gpt-hidden","hidden":true}]}}
{"op":"wait_eof"}
"#;

fn provider(dir: &Path, transcript: &str, version: &str) -> (CodexProvider, PathBuf) {
    let transcript_path = dir.join("transcript.jsonl");
    let record = dir.join("record.jsonl");
    std::fs::write(&transcript_path, transcript).expect("write transcript");
    let mut config = CodexConfig::new("codex-list");
    config.program = PathBuf::from(FAKE);
    config.codex_home = dir.join("home");
    config.default_model = Some("gpt-default".to_owned());
    config.models = vec!["gpt-configured".to_owned()];
    config.env_set.insert(
        "FAKE_CODEX_TRANSCRIPT".into(),
        transcript_path.display().to_string(),
    );
    config
        .env_set
        .insert("FAKE_CODEX_RECORD".into(), record.display().to_string());
    config
        .env_set
        .insert("FAKE_CODEX_VERSION".into(), version.to_owned());
    (CodexProvider::new(config), record)
}

/// The pids the fake recorded for `kind` (`start` = the app-server, `child` = its sleep).
fn recorded_pids(record: &Path, kind: &str) -> Vec<i32> {
    let text = std::fs::read_to_string(record).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry.get("kind").and_then(|k| k.as_str()) == Some(kind))
        .filter_map(|entry| entry.get("pid").and_then(|p| p.as_i64()))
        .map(|pid| pid as i32)
        .collect()
}

async fn wait_for_pid(record: &Path, kind: &str) -> Vec<i32> {
    for _ in 0..100 {
        let pids = recorded_pids(record, kind);
        if !pids.is_empty() {
            return pids;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the fake never recorded a `{kind}` pid");
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes the pid; it sends nothing.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn wait_until_gone(pid: i32) -> bool {
    for _ in 0..250 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn ids(models: &[nexus_claude::agent::ModelInfo]) -> Vec<&str> {
    models.iter().map(|m| m.id.as_str()).collect()
}

#[tokio::test]
async fn an_abandoned_listing_kills_the_app_server_and_its_children() {
    let dir = tempfile::tempdir().unwrap();
    let (provider, record) = provider(dir.path(), HANGING_LISTING, "0.130.0");

    // The caller gives up while `model/list` is pending.
    let outcome = tokio::time::timeout(Duration::from_secs(1), provider.catalog()).await;
    assert!(outcome.is_err(), "the listing should still be pending");

    let server = wait_for_pid(&record, "start").await;
    let children = wait_for_pid(&record, "child").await;
    let mut leaked = Vec::new();
    for pid in server.iter().chain(children.iter()).copied() {
        if !wait_until_gone(pid).await {
            // Do not leave the leak behind the failing test.
            // SAFETY: the pid was recorded by our own fake and is still alive.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            leaked.push(pid);
        }
    }
    assert!(
        leaked.is_empty(),
        "processes outlived the abandoned listing: {leaked:?}"
    );
}

#[test]
fn an_abandoned_listing_outside_any_runtime_kills_a_child_in_its_own_group() {
    let dir = tempfile::tempdir().unwrap();
    let (provider, record) = provider(dir.path(), HANGING_LISTING_OWN_GROUP, "0.130.0");
    let runtime = tokio::runtime::Runtime::new().unwrap();

    // The caller gives up while `model/list` is pending, and the listing is dropped
    // outside any runtime: no `Handle` is current when the listing's guard runs.
    let mut listing = Box::pin(provider.catalog());
    runtime.block_on(async {
        let outcome = tokio::time::timeout(Duration::from_secs(1), listing.as_mut()).await;
        assert!(outcome.is_err(), "the listing should still be pending");
        wait_for_pid(&record, "child").await;
    });
    drop(listing);

    let (server, children) = runtime.block_on(async {
        (
            wait_for_pid(&record, "start").await,
            wait_for_pid(&record, "child").await,
        )
    });
    let mut leaked = Vec::new();
    for pid in server.iter().chain(children.iter()).copied() {
        if !runtime.block_on(wait_until_gone(pid)) {
            // SAFETY: the pid was recorded by our own fake and is still alive.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            leaked.push(pid);
        }
    }
    assert!(
        leaked.is_empty(),
        "processes outlived the abandoned listing: {leaked:?}"
    );
}

#[tokio::test]
async fn a_listing_over_budget_yields_the_configured_models_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let (provider, record) = provider(dir.path(), HANGING_LISTING, "0.130.0");

    // The caller's own budget is 2 s: the listing must answer inside it, with
    // the configured names (default included), not with an empty list.
    let started = Instant::now();
    let models = tokio::time::timeout(Duration::from_secs(2), provider.catalog())
        .await
        .expect("catalog() must answer within 2 s")
        .expect("catalog() must not error");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(ids(&models), vec!["gpt-configured", "gpt-default"]);
    let default = models.iter().find(|m| m.id == "gpt-default").unwrap();
    assert!(default.is_default);

    // The fallback leaves nothing running either.
    for pid in wait_for_pid(&record, "start").await {
        assert!(wait_until_gone(pid).await, "app-server {pid} still runs");
    }
}

#[tokio::test]
async fn an_old_codex_is_not_started_as_app_server_for_its_models() {
    let dir = tempfile::tempdir().unwrap();
    let (provider, record) = provider(dir.path(), ANSWERED_LISTING, "0.120.0");

    let models = provider.catalog().await.expect("catalog() must not error");
    assert_eq!(ids(&models), vec!["gpt-configured", "gpt-default"]);
    assert!(
        recorded_pids(&record, "start").is_empty(),
        "an old codex must not start `app-server`"
    );
}

#[tokio::test]
async fn a_listing_adds_the_visible_models_to_the_configured_ones() {
    let dir = tempfile::tempdir().unwrap();
    let (provider, _record) = provider(dir.path(), ANSWERED_LISTING, "0.130.0");

    let models = provider.catalog().await.expect("catalog() must not error");
    assert_eq!(
        ids(&models),
        vec!["gpt-configured", "gpt-default", "gpt-live-1"]
    );
}
