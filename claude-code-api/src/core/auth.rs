//! JWT authentication for the gateway.
//!
//! [`AuthManager`] owns the shared secret and is the only thing that mints or
//! verifies a token. [`auth_middleware`] is the axum layer that [`crate::build_router`]
//! mounts when `auth.enabled` is true; it receives the `AuthManager` as middleware
//! state, so the secret reaches the check instead of sitting unused in the
//! configuration.
//!
//! On success the verified [`Claims`] are inserted as a request extension, so a
//! downstream handler can answer *who* is calling with `Extension<Claims>` rather
//! than parsing the header a second time.

use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::Response,
};
use chrono::{Duration, Utc};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The payload of a gateway token.
///
/// `Clone` so that [`auth_middleware`] can hand it to a handler through
/// `axum::Extension`, whose extractor clones out of the extensions map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: i64,
    pub iat: i64,
}

pub struct AuthManager {
    secret: String,
    enabled: bool,
}

impl AuthManager {
    pub fn new(secret: String, enabled: bool) -> Self {
        Self { secret, enabled }
    }

    /// Whether [`crate::build_router`] should mount [`auth_middleware`] at all.
    ///
    /// This is the single source of truth for `auth.enabled`. `AppComponents`
    /// used to carry a separate `auth_enabled: bool` mirror next to an
    /// `AuthManager` that was never built, which is how the flag and the secret
    /// came to live in two places and only one of them was consulted.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn generate_token(
        &self,
        user_id: &str,
        expiry_hours: i64,
    ) -> Result<String, jsonwebtoken::errors::Error> {
        let now = Utc::now();
        let exp = now + Duration::hours(expiry_hours);

        let claims = Claims {
            sub: user_id.to_string(),
            exp: exp.timestamp(),
            iat: now.timestamp(),
        };

        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(self.secret.as_bytes()),
        )
    }

    /// Verify `token` against the configured secret.
    ///
    /// [`Validation::default`] checks the signature *and* `exp`, with the
    /// 60-second leeway `jsonwebtoken` applies by default.
    pub fn verify_token(&self, token: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
        decode::<Claims>(
            token,
            &DecodingKey::from_secret(self.secret.as_bytes()),
            &Validation::default(),
        )
        .map(|data| data.claims)
    }
}

/// Paths [`auth_middleware`] lets through without a credential.
///
/// Only `/health`, and only because an authenticated liveness probe is a
/// liveness probe that fails: Kubernetes, Docker `HEALTHCHECK` and the
/// orchestrator's own poller have nowhere to put a bearer token, so a protected
/// `/health` makes every healthy gateway look dead and get restarted in a loop.
///
/// Exempting it discloses nothing. `health_check` answers the constant string
/// `"OK"` — no version, no configuration, no counters, no identity — so the only
/// fact it reveals is "something is listening", which anyone who can open the
/// TCP socket already knows. Everything that *does* disclose state stays behind
/// the check: `/stats` (cache counters), `/v1/models` (the model list) and the
/// conversation and completion routes.
///
/// The exemption is decided here rather than by mounting `/health` on a router
/// outside the layer, because `Router::layer` also covers the fallback: an
/// unrouted path answers `401` before `404` when authentication is on, and
/// moving `/health` out would have turned the whole route table into something
/// an anonymous caller can enumerate.
pub const UNAUTHENTICATED_PATHS: &[&str] = &["/health"];

/// Reject any request that does not carry a token this gateway signed.
///
/// Mounted by [`crate::build_router`] with
/// [`axum::middleware::from_fn_with_state`], which is what gives it the
/// `AuthManager` — and therefore the secret — that `from_fn` could not.
///
/// Note the scheme match is case-sensitive (`Bearer `, not `bearer `) while
/// RFC 7235 makes the scheme case-insensitive. That is a pre-existing
/// interoperability gap, pinned by a test rather than changed here; see the
/// report.
pub async fn auth_middleware(
    State(auth): State<Arc<AuthManager>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if UNAUTHENTICATED_PATHS.contains(&req.uri().path()) {
        return Ok(next.run(req).await);
    }

    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let claims = auth.verify_token(token).map_err(|error| {
        // The `ErrorKind` says *why* (bad signature, expired, malformed) and
        // carries no part of the token, so it is safe to log.
        tracing::debug!(kind = ?error.kind(), "rejected a bearer token");
        StatusCode::UNAUTHORIZED
    })?;

    req.extensions_mut().insert(claims);
    Ok(next.run(req).await)
}
