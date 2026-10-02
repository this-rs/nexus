//! What `core::auth::auth_middleware` accepts, and from whom.
//!
//! The gap these tests close: the mounted middleware used to check that the
//! `Authorization` header started with `Bearer ` and stop there. It never called
//! `AuthManager::verify_token`, so `Authorization: Bearer x` opened every route
//! and `auth.secret_key` was never read by anything. `auth.rs` was at 100% line
//! coverage the whole time — the lines of `verify_token` were executed by tests
//! that called it *directly*, never through the stack, and a test in
//! `gateway_assembly.rs` asserted the hole as the documented behaviour.
//!
//! Every test below therefore drives the **production** router (`build_router`
//! through `tests/support`) or the real middleware function, never a reimplemented
//! copy of the check.

mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use axum::{Extension, Router, middleware::from_fn_with_state, routing::get};
use claude_code_api::core::auth::{AuthManager, Claims};
use claude_code_api::core::config::{AuthConfig, AuthConfigError, PLACEHOLDER_SECRET_KEY};
use support::{TestSettings, test_app_with};

/// A fabricated fixture, not a credential: no deployment reads this file, and
/// nothing here is loaded from a configuration.
const TEST_SECRET: &str = "test-secret-not-a-real-key";
/// A second fabricated fixture, for "someone else's key".
const OTHER_SECRET: &str = "another-test-secret-not-a-real-key";

/// A route that is protected (unlike `/health`) and answers without touching the
/// CLI, so a `200` means "the middleware let me through" and nothing else.
const PROTECTED: &str = "/v1/models";

fn manager(secret: &str) -> AuthManager {
    AuthManager::new(secret.to_string(), true)
}

fn token_from(secret: &str, sub: &str, expiry_hours: i64) -> String {
    manager(secret)
        .generate_token(sub, expiry_hours)
        .expect("generate_token must succeed for an HS256 secret")
}

async fn protected_server() -> axum_test::TestServer {
    test_app_with(TestSettings::new().auth(true, TEST_SECRET).build()).await
}

// ===========================================================================
// The test that was missing: a bearer prefix is not a credential
// ===========================================================================

/// `Authorization: Bearer <anything>` is refused.
///
/// This is the regression. Before the middleware called `verify_token`, every
/// one of these strings came back `200` with the route's normal body.
#[tokio::test]
async fn an_arbitrary_bearer_string_is_refused() {
    let server = protected_server().await;

    for credential in [
        "Bearer x",
        "Bearer not-a-jwt",
        "Bearer ",
        "Bearer open-sesame",
        "Bearer eyJhbGciOiJIUzI1NiJ9.not.base64",
        // Three dots: structurally wrong for a JWS.
        "Bearer a.b.c.d",
    ] {
        assert_eq!(
            server
                .get(PROTECTED)
                .add_header("authorization", credential)
                .await
                .status_code(),
            StatusCode::UNAUTHORIZED,
            "{credential:?} is not a token this gateway signed"
        );
    }
}

/// A token signed with the configured secret is admitted.
#[tokio::test]
async fn a_token_signed_with_the_configured_secret_is_admitted() {
    let server = protected_server().await;
    let token = token_from(TEST_SECRET, "nexus-operator", 1);

    assert_eq!(
        server
            .get(PROTECTED)
            .add_header("authorization", format!("Bearer {token}"))
            .await
            .status_code(),
        StatusCode::OK,
        "a token minted with auth.secret_key must get through"
    );
}

/// And the verified claims are readable downstream, so a handler can answer
/// *who* is calling without re-parsing the header.
///
/// The middleware under test is the production function; only the route is local,
/// because no shipped handler reads `Claims` yet. Without the
/// `req.extensions_mut().insert(claims)` this is a `500`: axum's `Extension`
/// extractor has nothing to pull out.
#[tokio::test]
async fn the_verified_claims_reach_the_handler() {
    async fn whoami(Extension(claims): Extension<Claims>) -> String {
        format!("{}|{}|{}", claims.sub, claims.iat, claims.exp)
    }

    let app = Router::new()
        .route("/whoami", get(whoami))
        .layer(from_fn_with_state(
            Arc::new(manager(TEST_SECRET)),
            claude_code_api::core::auth::auth_middleware,
        ));
    let server = axum_test::TestServer::new(app).expect("TestServer over the auth-layered router");

    let token = token_from(TEST_SECRET, "nexus-operator", 2);
    let response = server
        .get("/whoami")
        .add_header("authorization", format!("Bearer {token}"))
        .await;

    assert_eq!(response.status_code(), StatusCode::OK);
    let body = response.text();
    let fields: Vec<&str> = body.split('|').collect();
    assert_eq!(
        fields[0], "nexus-operator",
        "the handler must see the `sub` that was signed, got {body:?}"
    );
    let iat: i64 = fields[1].parse().expect("iat must be an integer");
    let exp: i64 = fields[2].parse().expect("exp must be an integer");
    assert_eq!(
        exp - iat,
        2 * 3600,
        "the claims must be the ones from the token, not defaults"
    );
}

// ===========================================================================
// The four ways a well-formed JWT is still not ours
// ===========================================================================

/// Signed with somebody else's key: structurally a perfect JWT, refused.
#[tokio::test]
async fn a_token_signed_with_another_secret_is_refused() {
    let forged = token_from(OTHER_SECRET, "intruder", 1);
    assert!(
        manager(TEST_SECRET).verify_token(&forged).is_err(),
        "fixture check: the forged token must not verify against the gateway secret"
    );
    assert!(
        manager(OTHER_SECRET).verify_token(&forged).is_ok(),
        "fixture check: it is a valid token, just not ours"
    );

    let server = protected_server().await;
    assert_eq!(
        server
            .get(PROTECTED)
            .add_header("authorization", format!("Bearer {forged}"))
            .await
            .status_code(),
        StatusCode::UNAUTHORIZED
    );
}

/// Expired: our signature, `exp` in the past.
///
/// `generate_token(_, -1)` puts `exp` an hour behind `iat`, which is well beyond
/// the 60-second leeway `Validation::default()` allows.
#[tokio::test]
async fn an_expired_token_is_refused() {
    let expired = token_from(TEST_SECRET, "nexus-operator", -1);
    assert_eq!(
        manager(TEST_SECRET)
            .verify_token(&expired)
            .expect_err("fixture check: an expired token must not verify")
            .kind(),
        &jsonwebtoken::errors::ErrorKind::ExpiredSignature,
        "the fixture must be refused for being expired, not for anything else"
    );

    let server = protected_server().await;
    assert_eq!(
        server
            .get(PROTECTED)
            .add_header("authorization", format!("Bearer {expired}"))
            .await
            .status_code(),
        StatusCode::UNAUTHORIZED
    );
}

/// Tampered: one character of the payload changed, so the claims no longer match
/// the signature.
#[tokio::test]
async fn a_tampered_token_is_refused() {
    let token = token_from(TEST_SECRET, "nexus-operator", 1);
    let (header, rest) = token.split_once('.').expect("a JWS has three segments");
    let (payload, signature) = rest.split_once('.').expect("a JWS has three segments");

    // Flip the first payload character to another base64url character, which
    // changes the encoded claims without changing the shape of the token.
    let first = payload.chars().next().expect("a non-empty payload");
    let replacement = if first == 'f' { 'g' } else { 'f' };
    let tampered_payload: String = replacement.to_string() + &payload[first.len_utf8()..];
    assert_ne!(tampered_payload, payload, "the fixture must differ");
    let tampered = format!("{header}.{tampered_payload}.{signature}");

    assert!(
        manager(TEST_SECRET).verify_token(&tampered).is_err(),
        "fixture check: a tampered payload must not verify"
    );

    let server = protected_server().await;
    assert_eq!(
        server
            .get(PROTECTED)
            .add_header("authorization", format!("Bearer {tampered}"))
            .await
            .status_code(),
        StatusCode::UNAUTHORIZED
    );
}

/// No credential at all, an empty header, and the wrong scheme.
///
/// `bearer` in lower case is refused too. RFC 7235 makes the scheme
/// case-insensitive, so this is stricter than the specification — a known
/// interoperability gap, pinned here rather than changed: loosening what is
/// accepted is a separate decision from making the gateway verify anything, and
/// the token is now checked whatever the casing of the scheme.
#[tokio::test]
async fn a_missing_empty_or_wrong_scheme_header_is_refused() {
    let server = protected_server().await;
    let token = token_from(TEST_SECRET, "nexus-operator", 1);

    assert_eq!(
        server.get(PROTECTED).await.status_code(),
        StatusCode::UNAUTHORIZED,
        "no Authorization header at all"
    );

    for credential in [
        String::new(),
        " ".to_string(),
        "Bearer".to_string(),
        "Basic dXNlcjpwYXNz".to_string(),
        "Token abc".to_string(),
        format!("bearer {token}"),
        format!("BEARER {token}"),
        // The token alone, without a scheme.
        token.clone(),
    ] {
        assert_eq!(
            server
                .get(PROTECTED)
                .add_header("authorization", credential.clone())
                .await
                .status_code(),
            StatusCode::UNAUTHORIZED,
            "Authorization: {credential:?} must be refused"
        );
    }
}

// ===========================================================================
// The two judgement calls, pinned
// ===========================================================================

/// `/health` is reachable with no credential when `auth.enabled = true`.
///
/// An authenticated liveness probe is a probe that fails, and the body is the
/// constant `"OK"`, so the exemption discloses nothing a TCP connect does not.
#[tokio::test]
async fn health_stays_anonymous_so_a_liveness_probe_keeps_working() {
    let server = protected_server().await;

    server.get("/health").await.assert_text("OK");
    // A valid token is still fine on it, and so is a nonsensical one: the path
    // is not checked at all.
    let token = token_from(TEST_SECRET, "nexus-operator", 1);
    server
        .get("/health")
        .add_header("authorization", format!("Bearer {token}"))
        .await
        .assert_text("OK");
    server
        .get("/health")
        .add_header("authorization", "Bearer not-a-jwt")
        .await
        .assert_text("OK");
}

/// `/health` is the *only* exemption, and the rest of the table is protected —
/// including a path that is not in the table at all.
///
/// The `404` fallback stays behind the check, so an anonymous caller cannot
/// enumerate which routes the gateway serves. That is why the exemption lives in
/// the middleware instead of mounting `/health` outside the layer.
#[tokio::test]
async fn everything_but_health_is_protected_including_the_fallback() {
    let server = protected_server().await;

    for path in [
        "/v1/models",
        "/stats",
        "/v1/conversations",
        "/health/../stats",
        "/healthz",
        "/no-such-route",
    ] {
        assert_eq!(
            server.get(path).await.status_code(),
            StatusCode::UNAUTHORIZED,
            "GET {path} must be refused: only /health is exempt"
        );
    }

    assert_eq!(
        claude_code_api::core::auth::UNAUTHENTICATED_PATHS,
        ["/health"],
        "the exemption list is one path; adding to it is a deliberate act"
    );
}

/// `auth.enabled = false` is the shipped default and must stay anonymous: no
/// credential is required and a nonsensical one changes nothing, because the
/// middleware is not in the stack at all.
#[tokio::test]
async fn auth_disabled_requires_no_credential() {
    let server = test_app_with(TestSettings::new().auth(false, TEST_SECRET).build()).await;

    server.get("/health").await.assert_text("OK");
    for credential in ["", "Bearer not-a-jwt", "Basic nonsense"] {
        assert_eq!(
            server
                .get(PROTECTED)
                .add_header("authorization", credential)
                .await
                .status_code(),
            StatusCode::OK,
            "auth.enabled = false must serve {credential:?} like any other request"
        );
    }
    assert_eq!(server.get("/stats").await.status_code(), StatusCode::OK);
}

/// Authentication enabled on the placeholder secret does not start.
///
/// `build_components` is the single assembly point `main` and every test go
/// through, so the refusal cannot be bypassed by building a router by hand.
#[tokio::test]
async fn the_gateway_refuses_to_start_on_the_placeholder_secret() {
    let settings = TestSettings::new()
        .auth(true, PLACEHOLDER_SECRET_KEY)
        .build();
    // `AppComponents` is not `Debug`, so `expect_err` is not available here.
    let error = match claude_code_api::build_components(settings).await {
        Ok(_) => panic!("auth.enabled on the public placeholder key must not assemble"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.contains("auth.secret_key") && message.contains("auth.enabled = false"),
        "the refusal must name the field to set and the way out, got {message:?}"
    );

    // And a real secret assembles.
    claude_code_api::build_components(TestSettings::new().auth(true, TEST_SECRET).build())
        .await
        .expect("a secret of the operator's own must be accepted");
}

/// The same rule, at the level it is written: the placeholder and the empty
/// string are refused when authentication is on, and ignored when it is off.
#[test]
fn only_a_publicly_known_key_is_refused_and_only_when_auth_is_on() {
    let config = |enabled: bool, secret: &str| AuthConfig {
        enabled,
        secret_key: secret.to_string(),
        token_expiry_hours: 24,
    };

    assert_eq!(
        config(true, PLACEHOLDER_SECRET_KEY).validate(),
        Err(AuthConfigError::PlaceholderSecretKey)
    );
    assert_eq!(
        config(true, "").validate(),
        Err(AuthConfigError::EmptySecretKey)
    );
    assert_eq!(config(true, TEST_SECRET).validate(), Ok(()));

    // `auth.enabled = false` is the shipped default and is never refused, so no
    // existing deployment is stopped by this check.
    assert_eq!(config(false, PLACEHOLDER_SECRET_KEY).validate(), Ok(()));
    assert_eq!(config(false, "").validate(), Ok(()));

    assert_eq!(
        PLACEHOLDER_SECRET_KEY, "change-me-in-production",
        "the refused value must stay the one `Settings::new` defaults to"
    );
}
