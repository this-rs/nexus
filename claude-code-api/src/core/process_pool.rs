//! A process pool for Claude CLI children — **which nothing in the gateway
//! currently uses as a pool**.
//!
//! The original header read `移除 dead_code，激活进程池` ("remove dead_code,
//! activate the process pool"). That never happened, and the tests at the bottom
//! of this file pin down exactly how far it did not happen:
//!
//! * [`ProcessPool::get_or_create`] is the only method reached from production
//!   code ([`crate::api::chat::chat_completions`], in the
//!   `use_interactive_sessions == false` branch). It does not look at
//!   `inner.pool` at all: it forwards straight to
//!   `ClaudeManager::create_session_with_message` and spawns a fresh CLI for
//!   every request. A warm process sitting in `idle` is never consulted, and the
//!   new process is never recorded in `active`.
//! * [`ProcessPool::acquire`] — the method that *does* read the pool — has no
//!   caller anywhere in the workspace. On a pool hit it returns the receiving
//!   half of a channel whose sender is dropped on the spot (the
//!   `TODO: 需要重新连接到现有进程的输出流` below), so the caller would get a
//!   stream that is closed on arrival. Wiring `get_or_create` to `acquire` as it
//!   stands would therefore not speed the gateway up, it would make every pooled
//!   request answer with nothing.
//! * [`ProcessPool::release`] — the only way a process ever gets *into* `idle`
//!   from the serving path — has no caller either.
//! * [`ProcessPool::maintain_min_idle`], spawned by [`ProcessPool::new`],
//!   nevertheless spawns `min_idle` **real** `claude` children and parks them in
//!   `idle`. `ProcessPoolConfig::default` sets `min_idle = 2` and
//!   `config/optimized.toml` sets 3, so a default deployment keeps two CLI
//!   processes resident that no request can reach; `cleanup_loop` kills them
//!   after `idle_timeout_secs` and `maintain_min_idle` immediately replaces them.
//!   Only `config/fast.toml` escapes it, with `min_idle = 0`.
//!
//! None of this is fixed here: whether to finish the pool, delete it or merely
//! stop pre-warming is an architecture decision. The tests below exist so that
//! the inertia is asserted rather than assumed, and so that it cannot quietly
//! change meaning.

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

use super::claude_manager::ClaudeManager;
use crate::models::claude::ClaudeCodeOutput;

#[derive(Clone)]
pub struct ProcessPool {
    inner: Arc<ProcessPoolInner>,
}

struct ProcessPoolInner {
    manager: Arc<ClaudeManager>,
    pool: Mutex<Pool>,
    config: PoolConfig,
}

struct Pool {
    idle: VecDeque<PooledProcess>,
    #[allow(dead_code)]
    active: Vec<ActiveProcess>,
}

struct PooledProcess {
    session_id: String,
    #[allow(dead_code)]
    model: String,
    created_at: std::time::Instant,
}

struct ActiveProcess {
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    in_use_since: std::time::Instant,
}

#[derive(Clone)]
pub struct PoolConfig {
    pub min_idle: usize,
    #[allow(dead_code)]
    pub max_idle: usize,
    #[allow(dead_code)]
    pub max_active: usize,
    pub idle_timeout_secs: u64,
    pub default_model: String,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min_idle: 2,
            max_idle: 5,
            max_active: 20,
            idle_timeout_secs: 300, // 5 minutes
            default_model: "claude-opus-5".to_string(),
        }
    }
}

impl ProcessPool {
    pub fn new(manager: Arc<ClaudeManager>, config: PoolConfig) -> Self {
        let pool = ProcessPool {
            inner: Arc::new(ProcessPoolInner {
                manager,
                pool: Mutex::new(Pool {
                    idle: VecDeque::new(),
                    active: Vec::new(),
                }),
                config,
            }),
        };

        // 预启动最小空闲进程
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            pool_clone.maintain_min_idle().await;
        });

        // 定期清理过期的空闲进程
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            pool_clone.cleanup_loop().await;
        });

        pool
    }

    pub async fn get_or_create(
        &self,
        model: String,
        message: String,
    ) -> Result<(String, mpsc::Receiver<ClaudeCodeOutput>)> {
        // 直接创建新会话，暂时不使用池化（需要更复杂的实现）
        info!("Creating new Claude session for model: {}", model);
        self.inner
            .manager
            .create_session_with_message(None, None, Some(model), &message)
            .await
    }

    #[allow(dead_code)]
    pub async fn acquire(
        &self,
        model: Option<String>,
    ) -> Result<(String, mpsc::Receiver<ClaudeCodeOutput>)> {
        let model = model.unwrap_or_else(|| self.inner.config.default_model.clone());

        // 尝试从池中获取空闲进程
        let session_id = {
            let mut pool = self.inner.pool.lock();

            // 查找匹配模型的空闲进程
            let position = pool.idle.iter().position(|p| p.model == model);

            if let Some(pos) = position {
                let process = pool.idle.remove(pos).unwrap();
                let session_id = process.session_id.clone();

                pool.active.push(ActiveProcess {
                    session_id: session_id.clone(),
                    in_use_since: std::time::Instant::now(),
                });

                info!("Acquired process from pool: {}", session_id);
                Some(session_id)
            } else {
                None
            }
        };

        if let Some(session_id) = session_id {
            // 创建新的接收通道
            let (_tx, rx) = mpsc::channel(100);
            // TODO: 需要重新连接到现有进程的输出流
            Ok((session_id, rx))
        } else {
            // 检查是否达到最大活跃数
            {
                let pool = self.inner.pool.lock();
                if pool.active.len() >= self.inner.config.max_active {
                    return Err(anyhow!("Process pool exhausted"));
                }
            }

            // 创建新进程
            info!("Creating new process for model: {}", model);
            let result = self
                .inner
                .manager
                .create_interactive_session(None, None, Some(model.clone()))
                .await?;

            // 记录为活跃进程
            {
                let mut pool = self.inner.pool.lock();
                pool.active.push(ActiveProcess {
                    session_id: result.0.clone(),
                    in_use_since: std::time::Instant::now(),
                });
            }

            Ok(result)
        }
    }

    #[allow(dead_code)]
    pub async fn release(&self, session_id: String, model: String) {
        // 检查是否需要关闭进程
        let should_close = {
            let mut pool = self.inner.pool.lock();

            // 从活跃列表中移除
            pool.active.retain(|p| p.session_id != session_id);

            // 如果池未满，添加到空闲列表
            if pool.idle.len() < self.inner.config.max_idle {
                pool.idle.push_back(PooledProcess {
                    session_id: session_id.clone(),
                    model,
                    created_at: std::time::Instant::now(),
                });
                info!("Released process back to pool");
                false
            } else {
                true
            }
        }; // 释放锁

        // 在锁释放后执行异步操作
        if should_close {
            let _ = self.inner.manager.close_session(&session_id).await;
            info!("Pool full, closed process: {}", session_id);
        }
    }

    async fn maintain_min_idle(&self) {
        loop {
            let needed = {
                let pool = self.inner.pool.lock();
                let current_idle = pool.idle.len();
                self.inner.config.min_idle.saturating_sub(current_idle)
            };

            for _ in 0..needed {
                match self
                    .inner
                    .manager
                    .create_interactive_session(
                        None,
                        None,
                        Some(self.inner.config.default_model.clone()),
                    )
                    .await
                {
                    Ok((session_id, _)) => {
                        let mut pool = self.inner.pool.lock();
                        pool.idle.push_back(PooledProcess {
                            session_id,
                            model: self.inner.config.default_model.clone(),
                            created_at: std::time::Instant::now(),
                        });
                        info!("Pre-warmed process added to pool");
                    },
                    Err(e) => {
                        error!("Failed to create pre-warmed process: {}", e);
                    },
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        }
    }

    async fn cleanup_loop(&self) {
        let timeout = std::time::Duration::from_secs(self.inner.config.idle_timeout_secs);

        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

            let expired = {
                let mut pool = self.inner.pool.lock();
                let mut expired = Vec::new();

                // 检查过期的空闲进程
                pool.idle.retain(|p| {
                    if p.created_at.elapsed() > timeout {
                        expired.push(p.session_id.clone());
                        false
                    } else {
                        true
                    }
                });

                expired
            };

            // 关闭过期进程
            for session_id in expired {
                let _ = self.inner.manager.close_session(&session_id).await;
                info!("Closed idle process due to timeout: {}", session_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{FileAccessConfig, MCPConfig};
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;

    // ───────────────────────────── test harness ─────────────────────────────

    /// A stand-in for the `claude` binary, portable between Unix and Windows.
    ///
    /// This crate has no transport abstraction — `ClaudeManager` calls
    /// `Command::new(&self.claude_command)` directly — so the command string is
    /// the only seam. `FakeCli` writes a tiny script into a `TempDir` which
    /// prints nothing and then either exits at once or blocks until its stdin is
    /// closed, modelling a CLI that is still waiting for a turn.
    ///
    /// Nothing here spawns a real `claude`, touches the network or outlives the
    /// test: a blocking child is killed by the code under test, and the ones that
    /// are not die on their own.
    struct FakeCli {
        _dir: TempDir,
        script: PathBuf,
    }

    impl FakeCli {
        fn build(keep_alive: bool) -> Self {
            let dir = tempfile::tempdir().expect("tempdir for the fake CLI");
            let script = dir
                .path()
                .join(if cfg!(windows) { "fake.cmd" } else { "fake.sh" });

            let body = if cfg!(windows) {
                let mut body = String::from("@echo off\r\n");
                if keep_alive {
                    // `sort` reads stdin until EOF; while the session holds stdin
                    // open it never returns, so the process stays alive.
                    body.push_str("sort >nul 2>nul\r\n");
                }
                body.push_str("exit /b 0\r\n");
                body
            } else {
                let mut body = String::from("#!/bin/sh\n");
                if keep_alive {
                    body.push_str("cat > /dev/null\n");
                }
                body.push_str("exit 0\n");
                body
            };

            let mut file = std::fs::File::create(&script).expect("create the fake CLI script");
            file.write_all(body.as_bytes())
                .expect("write the fake CLI script");
            drop(file);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod the fake CLI script");
            }

            Self { _dir: dir, script }
        }

        /// Exits 0 immediately — the child is dead almost at once.
        fn exiting() -> Self {
            Self::build(false)
        }

        /// Stays alive until its stdin is closed, so a `kill` has a live child to
        /// reap.
        fn blocking() -> Self {
            Self::build(true)
        }

        fn command(&self) -> String {
            self.script.to_string_lossy().into_owned()
        }
    }

    /// A command string that cannot be spawned on any platform, so
    /// `Command::spawn` fails with [`std::io::ErrorKind::NotFound`].
    fn no_such_command() -> String {
        "nexus-test-claude-does-not-exist".to_string()
    }

    fn manager(command: String) -> Arc<ClaudeManager> {
        Arc::new(ClaudeManager::new(
            command,
            FileAccessConfig::default(),
            MCPConfig::default(),
        ))
    }

    fn config(min_idle: usize, max_idle: usize, max_active: usize) -> PoolConfig {
        PoolConfig {
            min_idle,
            max_idle,
            max_active,
            idle_timeout_secs: 300,
            default_model: "pool-default-model".to_string(),
        }
    }

    /// A pool assembled field by field, i.e. **without** the two background
    /// tasks `new()` spawns. Tests that are about those tasks drive
    /// `maintain_min_idle` / `cleanup_loop` by hand instead, so that nothing
    /// races with the assertions; the two tests that are about `new()` itself
    /// call `new()`.
    fn pool_without_background_tasks(
        manager: Arc<ClaudeManager>,
        config: PoolConfig,
    ) -> ProcessPool {
        ProcessPool {
            inner: Arc::new(ProcessPoolInner {
                manager,
                pool: Mutex::new(Pool {
                    idle: VecDeque::new(),
                    active: Vec::new(),
                }),
                config,
            }),
        }
    }

    fn seed_idle(pool: &ProcessPool, session_id: &str, model: &str) {
        pool.inner.pool.lock().idle.push_back(PooledProcess {
            session_id: session_id.to_string(),
            model: model.to_string(),
            created_at: std::time::Instant::now(),
        });
    }

    fn seed_active(pool: &ProcessPool, session_id: &str) {
        pool.inner.pool.lock().active.push(ActiveProcess {
            session_id: session_id.to_string(),
            in_use_since: std::time::Instant::now(),
        });
    }

    fn idle_entries(pool: &ProcessPool) -> Vec<(String, String)> {
        pool.inner
            .pool
            .lock()
            .idle
            .iter()
            .map(|p| (p.session_id.clone(), p.model.clone()))
            .collect()
    }

    fn active_ids(pool: &ProcessPool) -> Vec<String> {
        pool.inner
            .pool
            .lock()
            .active
            .iter()
            .map(|p| p.session_id.clone())
            .collect()
    }

    // ───────────────────────────── PoolConfig ─────────────────────────────

    /// `PoolConfig::default` is never the configuration the gateway runs with —
    /// `build_components` fills every field from `settings.process_pool` and
    /// hardcodes `idle_timeout_secs: 300` and `default_model:
    /// "claude-sonnet-5"`. The default kept here disagrees with it on the model,
    /// which is pinned so the divergence is visible rather than surprising.
    #[test]
    fn default_config_pre_warms_two_processes_and_expires_them_after_five_minutes() {
        let config = PoolConfig::default();

        assert_eq!(config.min_idle, 2, "a default deployment pre-warms 2 CLIs");
        assert_eq!(config.max_idle, 5);
        assert_eq!(config.max_active, 20);
        assert_eq!(config.idle_timeout_secs, 300);
        assert_eq!(config.default_model, "claude-opus-5");
    }

    // ────────────────────────────── new() ──────────────────────────────

    /// `new` spawns `maintain_min_idle` and `cleanup_loop`. With `min_idle == 0`
    /// the first has nothing to do, so the pool stays empty and no child is
    /// spawned — which is exactly why the test harness sets `min_idle = 0`.
    #[tokio::test]
    async fn new_with_min_idle_zero_leaves_the_pool_empty() {
        let cli = FakeCli::exiting();
        let pool = ProcessPool::new(manager(cli.command()), config(0, 5, 5));

        // Let both spawned tasks reach their first `await`.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        assert!(idle_entries(&pool).is_empty());
        assert!(active_ids(&pool).is_empty());
    }

    /// The pre-warm task `new` spawns consumes real CLI processes that no request
    /// can reach. This is the resource cost of the dead pool, asserted through
    /// the public constructor rather than through the private loop.
    #[tokio::test]
    async fn new_with_min_idle_pre_warms_processes_the_serving_path_cannot_reach() {
        let cli = FakeCli::exiting();
        let claude = manager(cli.command());
        let pool = ProcessPool::new(claude.clone(), config(2, 5, 5));

        for _ in 0..64 {
            if idle_entries(&pool).len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }

        let idle = idle_entries(&pool);
        assert_eq!(idle.len(), 2, "min_idle = 2 pre-warms two children");
        for (session_id, model) in &idle {
            assert_eq!(model, "pool-default-model");
            assert!(
                claude.get_session_info(session_id).is_some(),
                "a pre-warmed entry is backed by a registered child process"
            );
        }
        assert!(
            active_ids(&pool).is_empty(),
            "nothing ever moves a pre-warmed process into service"
        );

        claude.cleanup().await;
    }

    // ───────────────────────── get_or_create: the dead pool ─────────────────────────

    /// The headline finding. `get_or_create` is the only method the router calls,
    /// and it ignores `inner.pool` completely: a warm process whose model matches
    /// the request exactly is left untouched, a brand-new CLI is spawned instead,
    /// and the new session is not recorded in `active` either — so even
    /// `max_active` has no effect on the serving path.
    #[tokio::test]
    async fn get_or_create_ignores_a_matching_idle_process_and_spawns_a_new_one() {
        let cli = FakeCli::exiting();
        let claude = manager(cli.command());
        let pool = pool_without_background_tasks(claude.clone(), config(0, 5, 5));
        seed_idle(&pool, "warm-and-wasted", "model-x");

        let (session_id, _rx) = pool
            .get_or_create("model-x".to_string(), "bonjour".to_string())
            .await
            .expect("the fake CLI spawns");

        assert_ne!(
            session_id, "warm-and-wasted",
            "the pooled process was not reused"
        );
        assert!(
            claude.get_session_info(&session_id).is_some(),
            "a fresh child was spawned for this request"
        );
        assert_eq!(
            idle_entries(&pool),
            vec![("warm-and-wasted".to_string(), "model-x".to_string())],
            "the idle queue is not even consulted"
        );
        assert!(
            active_ids(&pool).is_empty(),
            "the new process is not tracked as active either"
        );

        claude.cleanup().await;
    }

    /// A spawn failure is the only error `get_or_create` can produce, and it is
    /// reported as an `io::Error` with `NotFound` — which `chat_completions` maps
    /// to `ApiError::ClaudeProcess`. The pool is left untouched.
    #[tokio::test]
    async fn get_or_create_reports_a_spawn_failure_as_not_found() {
        let pool = pool_without_background_tasks(manager(no_such_command()), config(0, 5, 5));

        let error = pool
            .get_or_create("model-x".to_string(), "bonjour".to_string())
            .await
            .expect_err("an unspawnable command cannot produce a session");

        let io_error = error
            .downcast_ref::<std::io::Error>()
            .expect("the failure is the spawn's io::Error");
        assert_eq!(io_error.kind(), std::io::ErrorKind::NotFound);
        assert!(idle_entries(&pool).is_empty());
        assert!(active_ids(&pool).is_empty());
    }

    // ───────────────────────── acquire: unreachable, and broken ─────────────────────────

    /// The second half of the finding: the pool-hit path of `acquire` does move
    /// the process from `idle` to `active`, but the receiver it hands back
    /// belongs to a channel whose sender is dropped one line later. `recv()`
    /// therefore returns `None` straight away — the stream is closed on arrival.
    ///
    /// This is why "just call `acquire` from `get_or_create`" is not a fix: every
    /// pooled request would answer with an empty completion. Reattaching a live
    /// process's stdout would need an API `ClaudeManager` does not have.
    #[tokio::test]
    async fn acquire_hands_back_a_receiver_that_is_closed_on_arrival() {
        // No CLI needed: the pool-hit path spawns nothing.
        let pool = pool_without_background_tasks(manager(no_such_command()), config(0, 5, 5));
        seed_idle(&pool, "warm-1", "model-x");

        let (session_id, mut rx) = pool
            .acquire(Some("model-x".to_string()))
            .await
            .expect("a matching idle process is a pool hit");

        assert_eq!(session_id, "warm-1");
        assert!(
            idle_entries(&pool).is_empty(),
            "the hit is removed from the idle queue"
        );
        assert_eq!(active_ids(&pool), vec!["warm-1".to_string()]);
        assert!(
            rx.recv().await.is_none(),
            "the sender is dropped inside acquire, so this channel never yields anything"
        );
    }

    /// `acquire(None)` falls back to `config.default_model`, which is what
    /// decides whether a pre-warmed process matches at all. Note the trap this
    /// exposes: `maintain_min_idle` tags its children with `default_model`, so a
    /// request naming any other model could never match one even if `acquire`
    /// were wired up.
    #[tokio::test]
    async fn acquire_without_a_model_matches_on_the_configured_default() {
        let pool = pool_without_background_tasks(manager(no_such_command()), config(0, 5, 5));
        seed_idle(&pool, "warm-other", "model-x");
        seed_idle(&pool, "warm-default", "pool-default-model");

        let (session_id, _rx) = pool.acquire(None).await.expect("the default model matches");

        assert_eq!(session_id, "warm-default");
        assert_eq!(
            idle_entries(&pool),
            vec![("warm-other".to_string(), "model-x".to_string())],
            "only the matching entry is taken"
        );
    }

    /// On a pool miss `acquire` refuses once `active` is at `max_active`, before
    /// spawning anything.
    #[tokio::test]
    async fn acquire_refuses_when_active_processes_reach_max_active() {
        let pool = pool_without_background_tasks(manager(no_such_command()), config(0, 5, 1));
        seed_active(&pool, "in-flight-1");

        let error = pool
            .acquire(Some("model-x".to_string()))
            .await
            .expect_err("the pool is at capacity");

        assert_eq!(error.to_string(), "Process pool exhausted");
        assert_eq!(active_ids(&pool), vec!["in-flight-1".to_string()]);
    }

    /// `config/fast.toml` sets `process_pool.size = 0` to *disable* pooling, and
    /// `build_components` feeds `size` straight into `max_active`. The guard is
    /// `>=`, so with that configuration the very first `acquire` on an empty pool
    /// is refused — "disabled" and "exhausted" are the same value. Harmless only
    /// because `acquire` has no callers.
    #[tokio::test]
    async fn acquire_with_max_active_zero_refuses_even_the_first_request() {
        let pool = pool_without_background_tasks(manager(no_such_command()), config(0, 0, 0));

        let error = pool
            .acquire(Some("model-x".to_string()))
            .await
            .expect_err("max_active = 0 admits nobody");

        assert_eq!(error.to_string(), "Process pool exhausted");
        assert!(active_ids(&pool).is_empty());
    }

    /// On a pool miss with room to grow, `acquire` spawns an interactive session
    /// and records it as active. The receiver is deliberately not read: the
    /// stdout task of `create_interactive_session` never closes its sender, so
    /// awaiting it would hang.
    #[tokio::test]
    async fn acquire_on_a_miss_spawns_and_tracks_a_new_process() {
        let cli = FakeCli::exiting();
        let claude = manager(cli.command());
        let pool = pool_without_background_tasks(claude.clone(), config(0, 5, 4));
        seed_idle(&pool, "warm-other", "another-model");

        let (session_id, _rx) = pool
            .acquire(Some("model-x".to_string()))
            .await
            .expect("there is room for a new process");

        assert_ne!(session_id, "warm-other");
        assert!(
            claude.get_session_info(&session_id).is_some(),
            "a real child was spawned and registered"
        );
        assert_eq!(active_ids(&pool), vec![session_id]);
        assert_eq!(
            idle_entries(&pool),
            vec![("warm-other".to_string(), "another-model".to_string())],
            "the non-matching idle entry is left alone"
        );

        claude.cleanup().await;
    }

    /// A spawn failure on the miss path propagates with `?` and, notably, leaves
    /// nothing behind in `active` — the process is only recorded after the spawn
    /// succeeds.
    #[tokio::test]
    async fn acquire_on_a_miss_propagates_a_spawn_failure_without_recording_it() {
        let pool = pool_without_background_tasks(manager(no_such_command()), config(0, 5, 4));

        let error = pool
            .acquire(Some("model-x".to_string()))
            .await
            .expect_err("an unspawnable command cannot produce a session");

        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .expect("the failure is the spawn's io::Error")
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(active_ids(&pool).is_empty());
    }

    // ───────────────────────── release: unreachable too ─────────────────────────

    /// With room in the idle queue, `release` moves the session from `active` to
    /// `idle` and keeps the child alive. Since nothing calls `release`, this is
    /// the only way `idle` could ever hold a process that served a request.
    #[tokio::test]
    async fn release_parks_the_session_in_the_idle_queue_when_there_is_room() {
        let pool = pool_without_background_tasks(manager(no_such_command()), config(0, 2, 5));
        seed_active(&pool, "live-1");
        seed_active(&pool, "live-2");

        pool.release("live-1".to_string(), "model-x".to_string())
            .await;

        assert_eq!(
            active_ids(&pool),
            vec!["live-2".to_string()],
            "only the released session leaves the active list"
        );
        assert_eq!(
            idle_entries(&pool),
            vec![("live-1".to_string(), "model-x".to_string())]
        );
    }

    /// With `max_idle = 0` the queue is always full, so `release` closes the
    /// process instead of parking it — and the lock is dropped before the
    /// `await`, which is what makes that `close_session` call legal at all.
    #[tokio::test]
    async fn release_closes_the_process_when_the_idle_queue_is_full() {
        let cli = FakeCli::blocking();
        let claude = manager(cli.command());
        let (session_id, _rx) = claude
            .create_interactive_session(None, None, None)
            .await
            .expect("the fake CLI spawns");
        assert!(claude.get_session_info(&session_id).is_some());

        let pool = pool_without_background_tasks(claude.clone(), config(0, 0, 5));
        seed_active(&pool, &session_id);

        pool.release(session_id.clone(), "model-x".to_string())
            .await;

        assert!(active_ids(&pool).is_empty());
        assert!(
            idle_entries(&pool).is_empty(),
            "a full queue means the process is closed, not parked"
        );
        assert!(
            claude.get_session_info(&session_id).is_none(),
            "close_session removed and killed the child"
        );
    }

    /// `release` of a session the manager never knew is silently fine: the
    /// `close_session` result is discarded with `let _ =`. Worth pinning, because
    /// it means a double release can never be detected.
    #[tokio::test]
    async fn release_of_an_unknown_session_is_silently_ignored() {
        let pool = pool_without_background_tasks(manager(no_such_command()), config(0, 0, 5));

        pool.release("never-existed".to_string(), "model-x".to_string())
            .await;

        assert!(idle_entries(&pool).is_empty());
        assert!(active_ids(&pool).is_empty());
    }

    // ───────────────────────── maintain_min_idle ─────────────────────────

    /// Driven directly, so the assertion cannot race the background task. The
    /// loop never returns, hence the timeout: `maintain_min_idle` tops the queue
    /// up and then sleeps for 10 seconds, forever.
    #[tokio::test]
    async fn maintain_min_idle_fills_the_queue_with_real_children_then_sleeps() {
        let cli = FakeCli::exiting();
        let claude = manager(cli.command());
        let pool = pool_without_background_tasks(claude.clone(), config(2, 5, 5));

        let outcome =
            tokio::time::timeout(Duration::from_millis(500), pool.maintain_min_idle()).await;
        assert!(outcome.is_err(), "the maintenance loop never returns");

        let idle = idle_entries(&pool);
        assert_eq!(idle.len(), 2);
        for (session_id, model) in &idle {
            assert_eq!(
                model, "pool-default-model",
                "pre-warmed children are tagged with default_model, not a request's model"
            );
            assert!(claude.get_session_info(session_id).is_some());
        }
        assert!(active_ids(&pool).is_empty());

        claude.cleanup().await;
    }

    /// The `Err` arm: every spawn fails, each failure is logged and swallowed,
    /// and the loop keeps going with an empty queue. No error is ever surfaced to
    /// anyone — a gateway whose `claude.command` is wrong pre-warms nothing and
    /// says so only at `error!` level.
    #[tokio::test]
    async fn maintain_min_idle_swallows_every_spawn_failure() {
        let claude = manager(no_such_command());
        let pool = pool_without_background_tasks(claude.clone(), config(2, 5, 5));

        let outcome =
            tokio::time::timeout(Duration::from_millis(500), pool.maintain_min_idle()).await;
        assert!(outcome.is_err(), "the maintenance loop never returns");

        assert!(
            idle_entries(&pool).is_empty(),
            "nothing was pre-warmed, and nothing was reported"
        );
    }

    /// `saturating_sub` clamps: a queue already at or above `min_idle` triggers no
    /// spawn at all. Asserted by the queue still holding exactly the two seeded
    /// entries — a spawn would have appended a third.
    #[tokio::test]
    async fn maintain_min_idle_spawns_nothing_when_the_queue_is_already_full_enough() {
        let cli = FakeCli::exiting();
        let pool = pool_without_background_tasks(manager(cli.command()), config(1, 5, 5));
        seed_idle(&pool, "warm-1", "pool-default-model");
        seed_idle(&pool, "warm-2", "pool-default-model");

        let outcome =
            tokio::time::timeout(Duration::from_millis(250), pool.maintain_min_idle()).await;
        assert!(outcome.is_err(), "the maintenance loop never returns");

        assert_eq!(
            idle_entries(&pool),
            vec![
                ("warm-1".to_string(), "pool-default-model".to_string()),
                ("warm-2".to_string(), "pool-default-model".to_string()),
            ]
        );
    }

    // ───────────────────────── cleanup_loop ─────────────────────────

    /// `cleanup_loop` sleeps 60 seconds before its first sweep, so the clock is
    /// paused and auto-advanced. `created_at` is a `std::time::Instant`, which
    /// tokio's paused clock does **not** move — hence `idle_timeout_secs = 0`,
    /// which makes any elapsed time an expiry.
    #[tokio::test(start_paused = true)]
    async fn cleanup_loop_kills_idle_processes_past_the_timeout() {
        let cli = FakeCli::blocking();
        let claude = manager(cli.command());
        let (session_id, _rx) = claude
            .create_interactive_session(None, None, None)
            .await
            .expect("the fake CLI spawns");

        let mut pool_config = config(0, 5, 5);
        pool_config.idle_timeout_secs = 0;
        let pool = pool_without_background_tasks(claude.clone(), pool_config);
        seed_idle(&pool, &session_id, "model-x");

        let outcome = tokio::time::timeout(Duration::from_secs(90), pool.cleanup_loop()).await;
        assert!(outcome.is_err(), "the cleanup loop never returns");

        assert!(
            idle_entries(&pool).is_empty(),
            "the expired entry was dropped from the queue"
        );
        assert!(
            claude.get_session_info(&session_id).is_none(),
            "and its child was closed"
        );
    }

    /// The `retain` keep-arm: an entry younger than `idle_timeout_secs` survives
    /// the sweep untouched.
    #[tokio::test(start_paused = true)]
    async fn cleanup_loop_keeps_idle_processes_that_are_still_fresh() {
        let mut pool_config = config(0, 5, 5);
        pool_config.idle_timeout_secs = 3600;
        let pool = pool_without_background_tasks(manager(no_such_command()), pool_config);
        seed_idle(&pool, "fresh-1", "model-x");

        let outcome = tokio::time::timeout(Duration::from_secs(90), pool.cleanup_loop()).await;
        assert!(outcome.is_err(), "the cleanup loop never returns");

        assert_eq!(
            idle_entries(&pool),
            vec![("fresh-1".to_string(), "model-x".to_string())],
            "a fresh entry survives the sweep"
        );
    }
}
