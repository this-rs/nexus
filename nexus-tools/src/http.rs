//! The streamable-HTTP transport: one `POST /mcp` per message, answered with JSON.
//!
//! Everything is **closed by default**: the signing key is mandatory (a server with no key
//! has no way to tell a session from a stranger), every request carries a bearer token that
//! verifies, a request that comes from a browser (it has an `Origin`) is refused unless that
//! origin was allowed, a server on a loopback address only answers to a loopback `Host`
//! (a DNS-rebinding page cannot reach it), and a body is capped.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use crate::protocol::{code, error_response};
use crate::server::{Server, Session};
use crate::token::{SigningKey, verify};
use crate::tool::SessionState;

/// Largest request body accepted.
pub const MAX_BODY_BYTES: usize = 1 << 20;

/// Most sessions whose state is kept at once; beyond it the oldest is forgotten.
const MAX_SESSIONS: usize = 1024;

/// What a request handler needs.
struct HttpState {
    server: Arc<Server>,
    key: SigningKey,
    allowed_origins: Vec<String>,
    loopback_only: bool,
    sessions: Mutex<Vec<(String, Arc<SessionState>)>>,
}

impl HttpState {
    fn session_state(&self, session_id: &str) -> Arc<SessionState> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, state)) = sessions.iter().find(|(id, _)| id == session_id) {
            return Arc::clone(state);
        }
        if sessions.len() >= MAX_SESSIONS {
            sessions.remove(0);
        }
        let state = Arc::new(SessionState::default());
        sessions.push((session_id.to_owned(), Arc::clone(&state)));
        state
    }

    fn forget(&self, session_id: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(id, _)| id != session_id);
    }
}

/// The router of a server bound to `bound`.
pub fn router(
    server: Arc<Server>,
    key: SigningKey,
    allowed_origins: Vec<String>,
    bound: SocketAddr,
) -> Router {
    let state = Arc::new(HttpState {
        server,
        key,
        allowed_origins,
        loopback_only: bound.ip().is_loopback(),
        sessions: Mutex::new(Vec::new()),
    });
    Router::new()
        .route(
            "/mcp",
            post(post_mcp).delete(delete_mcp).get(method_not_allowed),
        )
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn problem(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({"error": message}))).into_response()
}

/// A `Host` header naming a loopback host, with or without a port.
fn is_loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1")
}

/// Rejections that depend on the request alone, not on who the caller claims to be.
fn screen(state: &HttpState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    if state.loopback_only {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !is_loopback_host(host) {
            return Err(Box::new(problem(StatusCode::FORBIDDEN, "forbidden host")));
        }
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        let allowed = origin.to_str().is_ok_and(|origin| {
            state
                .allowed_origins
                .iter()
                .any(|allowed| allowed == origin)
        });
        if !allowed {
            return Err(Box::new(problem(StatusCode::FORBIDDEN, "forbidden origin")));
        }
    }
    Ok(())
}

fn authenticate(state: &HttpState, headers: &HeaderMap) -> Result<Session, Box<Response>> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| Box::new(problem(StatusCode::UNAUTHORIZED, "unauthorized")))?;
    let profile = verify(&state.key, token, now())
        .map_err(|_| Box::new(problem(StatusCode::UNAUTHORIZED, "unauthorized")))?;
    let session_state = state.session_state(&profile.session_id);
    Ok(Session {
        profile,
        state: session_state,
        // An HTTP request has no channel back once it is answered.
        notifier: None,
    })
}

async fn health() -> Response {
    axum::Json(json!({"status": "ok"})).into_response()
}

async fn method_not_allowed() -> Response {
    problem(StatusCode::METHOD_NOT_ALLOWED, "use POST")
}

async fn delete_mcp(State(state): State<Arc<HttpState>>, headers: HeaderMap) -> Response {
    if let Err(response) = screen(&state, &headers) {
        return *response;
    }
    match authenticate(&state, &headers) {
        Ok(session) => {
            state.forget(&session.profile.session_id);
            StatusCode::NO_CONTENT.into_response()
        },
        Err(response) => *response,
    }
}

async fn post_mcp(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = screen(&state, &headers) {
        return *response;
    }
    let session = match authenticate(&state, &headers) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    let message: Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(error_response(
                    Value::Null,
                    code::PARSE_ERROR,
                    "invalid JSON",
                )),
            )
                .into_response();
        },
    };
    let is_initialize = |m: &Value| m.get("method").and_then(Value::as_str) == Some("initialize");
    let answer = match message {
        Value::Array(batch) => {
            let initialize = batch.iter().any(is_initialize);
            let mut responses = Vec::new();
            for item in batch {
                if let Some(response) = state.server.handle(&session, item).await {
                    responses.push(response);
                }
            }
            (!responses.is_empty()).then_some((Value::Array(responses), initialize))
        },
        single => {
            let initialize = is_initialize(&single);
            state
                .server
                .handle(&session, single)
                .await
                .map(|r| (r, initialize))
        },
    };
    match answer {
        None => StatusCode::ACCEPTED.into_response(),
        Some((value, initialize)) => {
            let mut response = axum::Json(value).into_response();
            if initialize && let Ok(id) = HeaderValue::from_str(&session.profile.session_id) {
                response.headers_mut().insert("mcp-session-id", id);
            }
            response
        },
    }
}

/// Serves `router` on `listener` until the process ends.
pub async fn serve(listener: tokio::net::TcpListener, router: Router) -> std::io::Result<()> {
    axum::serve(listener, router).await
}
