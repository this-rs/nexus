//! OpenAI-compatible HTTP gateway for the Claude Code CLI.
//!
//! The crate is primarily a binary (`src/main.rs`), but everything lives in this
//! library target so that the router, the handlers and the core services can be
//! exercised from integration tests in `tests/` without starting a server.
//!
//! The entry points are [`create_app`] (production assembly, used by `main`) and
//! [`build_router`] (route table + middleware stack only, for tests that want to
//! supply their own state).

pub mod api;
pub mod core;
pub mod middleware;
pub mod models;
pub mod utils;

/// Planting the fake `claude` executable the unit tests spawn.
///
/// The same file backs `tests/support/fake_exec.rs` for the integration tests;
/// it is shared rather than duplicated because the invariant it enforces — never
/// execute an inode this process opened for writing — has to hold at *every*
/// site or `ETXTBSY` comes back at whichever one was left out.
#[cfg(test)]
#[path = "../tests/support/fake_exec.rs"]
mod fake_exec;

use anyhow::Result;
use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::api::chat::ChatState;
use crate::core::{
    claude_manager::ClaudeManager,
    config::Settings,
    model_registry::ModelRegistry,
    process_pool::{PoolConfig, ProcessPool},
};

/// Every piece of state the router needs.
///
/// [`create_app`] builds this from a [`Settings`]; tests can build it piecewise
/// (e.g. with a [`ModelRegistry`] pointed at a local mock server) and hand it to
/// [`build_router`].
pub struct AppComponents {
    pub chat_state: ChatState,
    pub conversation_state: api::conversations::ConversationState,
    pub stats_state: api::stats::StatsState,
    pub model_registry: Arc<ModelRegistry>,
    /// Mirror of `settings.auth.enabled`: when true, [`build_router`] layers
    /// [`core::auth::auth_middleware`] over **every** route, `/health` included.
    ///
    /// The flag exists because `auth.enabled` used to be inert — `core::auth`
    /// was compiled and never mounted, so the gateway served every request
    /// anonymously whatever the configuration said.
    ///
    /// Note what the mounted middleware does and does not do: it demands an
    /// `Authorization: Bearer …` header and nothing more. It never calls
    /// [`core::auth::AuthManager::verify_token`], so any bearer string is
    /// accepted and `auth.secret_key` is still unused.
    pub auth_enabled: bool,
}

/// The route table and middleware stack of the gateway.
///
/// This is the single definition of what the gateway serves: `main` reaches it
/// through [`create_app`], tests reach it directly.
///
/// Routes served: `/health`, `/v1/models`, `/v1/models/refresh`,
/// `/v1/chat/completions`, `/v1/sessions/:conversation_id/interrupt`,
/// `/v1/conversations` and `/v1/conversations/:id`, `/stats`.
///
/// Deliberately **not** served: `/v1/sessions` and `/v1/projects`. The handlers
/// in [`api::sessions`] and [`api::projects`] exist but are placeholders — they
/// answer with a hardcoded empty array and `{"message": "Not implemented"}`.
/// Mounting them would advertise two working collections that can never hold
/// anything, so they stay off the table and the paths stay `404`.
pub fn build_router(components: AppComponents) -> Router {
    let AppComponents {
        chat_state,
        conversation_state,
        stats_state,
        model_registry,
        auth_enabled,
    } = components;

    let cors = CorsLayer::permissive();

    let api_routes = Router::new()
        .route("/v1/chat/completions", post(api::chat::chat_completions))
        .route(
            "/v1/sessions/:conversation_id/interrupt",
            post(api::chat::interrupt_session),
        )
        .with_state(chat_state);

    let conversation_routes = Router::new()
        .route(
            "/v1/conversations",
            post(api::conversations::create_conversation),
        )
        .route(
            "/v1/conversations",
            get(api::conversations::list_conversations),
        )
        .route(
            "/v1/conversations/:id",
            get(api::conversations::get_conversation),
        )
        .with_state(conversation_state);

    let stats_routes = Router::new()
        .route("/stats", get(api::stats::get_stats))
        .with_state(stats_state);

    // 模型注册表（动态模型列表，带 TTL 缓存）
    let model_routes = Router::new()
        .route("/v1/models", get(api::models::list_models))
        .route("/v1/models/refresh", post(api::models::refresh_models))
        .with_state(model_registry);

    // 组合所有路由
    let routes = Router::new()
        .route("/health", get(health_check))
        .merge(model_routes)
        .merge(api_routes)
        .merge(conversation_routes)
        .merge(stats_routes);

    // `auth.enabled` only means something if the middleware is actually in the
    // stack. It sits *inside* `request_id` and `error_handler` so a rejected
    // request still carries an `x-request-id` and still gets logged, and inside
    // the CORS layer so a preflight `OPTIONS` is answered rather than refused.
    let routes = if auth_enabled {
        routes.layer(axum::middleware::from_fn(core::auth::auth_middleware))
    } else {
        routes
    };

    routes
        .layer(axum::middleware::from_fn(
            middleware::request_id::add_request_id,
        ))
        .layer(axum::middleware::from_fn(
            middleware::error_handler::handle_errors,
        ))
        .layer(cors)
}

/// Build every service from `settings` and assemble the router.
pub async fn create_app(settings: Settings) -> Result<Router> {
    Ok(build_router(build_components(settings).await?))
}

/// Build the application state from `settings`.
///
/// Split out of [`create_app`] so a test can swap one component (typically the
/// [`ModelRegistry`]) while keeping the production wiring for all the others.
pub async fn build_components(settings: Settings) -> Result<AppComponents> {
    use crate::core::{
        cache::{CacheConfig, ResponseCache},
        conversation::{ConversationConfig, ConversationManager},
        interactive_session::InteractiveSessionManager,
        storage::{InMemoryConversationConfig, InMemoryConversationStore},
    };

    let claude_manager = Arc::new(ClaudeManager::new(
        settings.claude.command.clone(),
        settings.file_access.clone(),
        settings.mcp.clone(),
    ));

    // 创建进程池配置
    let pool_config = PoolConfig {
        min_idle: settings.process_pool.min_idle,
        max_idle: settings.process_pool.max_idle,
        max_active: settings.process_pool.size,
        idle_timeout_secs: 300,
        default_model: "claude-sonnet-5".to_string(),
    };

    // 初始化进程池
    info!(
        "Initializing process pool with {} min idle processes",
        pool_config.min_idle
    );
    let process_pool = Arc::new(ProcessPool::new(claude_manager.clone(), pool_config));

    // 初始化交互式会话管理器
    info!("Initializing interactive session manager");
    let interactive_session_manager = Arc::new(InteractiveSessionManager::new(
        claude_manager.clone(),
        settings.claude.command.clone(),
    ));

    // 如果启用了交互式会话，预热一个默认进程
    //
    // Nothing is actually pre-warmed: `prewarm_default_session` is a `TODO` stub
    // that logs and returns `Ok(())` without spawning anything, so this `Err`
    // arm is unreachable as the code stands and no test can enter it. The arm is
    // kept so that the day the stub grows a body, a failure is still logged
    // instead of being discarded by a `let _ =`.
    if settings.claude.use_interactive_sessions
        && let Err(e) = interactive_session_manager.prewarm_default_session().await
    {
        tracing::error!("Failed to pre-warm Claude process: {}", e);
    }

    let conversation_store = InMemoryConversationStore::new(InMemoryConversationConfig::default());
    let conversation_manager = Arc::new(ConversationManager::new(
        conversation_store,
        ConversationConfig::default(),
    ));
    let cache = Arc::new(ResponseCache::new(CacheConfig::default()));

    let chat_state = ChatState::new(
        claude_manager.clone(),
        process_pool.clone(),
        interactive_session_manager.clone(),
        conversation_manager.clone(),
        cache.clone(),
        settings.claude.use_interactive_sessions,
        Arc::new(settings.clone()),
    );

    let conversation_state = api::conversations::ConversationState {
        manager: conversation_manager.clone(),
    };

    let stats_state = api::stats::StatsState {
        cache: cache.clone(),
    };

    Ok(AppComponents {
        chat_state,
        conversation_state,
        stats_state,
        model_registry: Arc::new(ModelRegistry::new()),
        auth_enabled: settings.auth.enabled,
    })
}

async fn health_check() -> &'static str {
    "OK"
}
