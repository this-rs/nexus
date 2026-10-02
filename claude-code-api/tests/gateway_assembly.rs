//! What `claude_code_api::build_router` / `build_components` / `create_app`
//! actually assemble.
//!
//! These tests drive the **production** router (through `tests/support`), so an
//! assertion here is an assertion about what the deployed gateway serves, which
//! middleware it layers, and which configuration switches it honours.
//!
//! Two of them exist because the answer used to be "none":
//!
//! * `auth.enabled = true` layered nothing. `core::auth::AuthManager` and
//!   `core::auth::auth_middleware` compiled, were never mounted, and the gateway
//!   answered every request anonymously whatever the configuration said.
//! * `/v1/sessions` and `/v1/projects` are `404` while `api::sessions` and
//!   `api::projects` carry handlers. The tests below show what those handlers
//!   can answer, which is the evidence for leaving the routes unmounted.
//!
//! What the mounted middleware *checks* is `tests/auth_token_verification.rs`'s
//! subject; this file only asserts that it is in the stack, in the right place,
//! and driven by the configuration.

mod support;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use claude_code_api::core::auth::AuthManager;
use claude_code_api::{build_router, create_app};
use http_body_util::BodyExt;
use support::{TestSettings, test_app_with, test_components};

/// The shared secret for the tests that mint a token. It is a test fixture, not
/// a credential: no deployment reads this file.
const TEST_SECRET: &str = "secret-for-this-test-binary-only";

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect handler body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("handler body must be JSON")
}

// ===========================================================================
// auth.enabled — the switch that used to do nothing
// ===========================================================================

/// With `auth.enabled = true` the gateway refuses an anonymous caller on every
/// route it serves, including the ones that never touch the CLI.
///
/// Before `build_router` layered `core::auth::auth_middleware`, each of these
/// returned its normal success status with no credential presented at all.
///
/// `/health` is the one exemption — see
/// `core::auth::UNAUTHENTICATED_PATHS` and
/// `tests/auth_token_verification.rs::health_stays_anonymous_so_a_liveness_probe_keeps_working`.
#[tokio::test]
async fn auth_enabled_refuses_every_route_to_an_anonymous_caller() {
    let server = test_app_with(TestSettings::new().auth(true, TEST_SECRET).build()).await;

    for path in ["/v1/models", "/stats", "/v1/conversations"] {
        assert_eq!(
            server.get(path).await.status_code(),
            StatusCode::UNAUTHORIZED,
            "GET {path} must be refused when auth.enabled = true"
        );
    }

    // The completion endpoint is refused *before* the CLI is reached: with the
    // default unspawnable `claude.command` an authorised call would come back
    // 500 `claude_process_error`, so 401 proves the middleware ran first.
    let completion = server
        .post("/v1/chat/completions")
        .json(&support::openai::chat_request("Bonjour"))
        .await;
    assert_eq!(completion.status_code(), StatusCode::UNAUTHORIZED);

    assert_eq!(
        server.post("/v1/models/refresh").await.status_code(),
        StatusCode::UNAUTHORIZED
    );
}

/// A token minted by `AuthManager::generate_token` gets through, and
/// `verify_token` reads back the subject that was put in — the two halves of
/// `core::auth` agree with each other.
#[tokio::test]
async fn auth_enabled_admits_a_token_minted_by_auth_manager() {
    let manager = AuthManager::new(TEST_SECRET.to_string(), true);
    let token = manager
        .generate_token("nexus-gateway-test", 1)
        .expect("generate_token must succeed for an HS256 secret");
    let claims = manager
        .verify_token(&token)
        .expect("a freshly minted token must verify");
    assert_eq!(claims.sub, "nexus-gateway-test");
    assert!(claims.exp > claims.iat, "exp must be after iat");

    let server = test_app_with(TestSettings::new().auth(true, TEST_SECRET).build()).await;
    assert_eq!(
        server
            .get("/v1/models")
            .add_header("authorization", format!("Bearer {token}"))
            .await
            .status_code(),
        StatusCode::OK
    );
}

/// The bearer prefix is not a credential.
///
/// This was the hole, and it was `#[ignore]`d here as the specification while
/// `core::auth::auth_middleware` returned `Ok(next.run(req))` on the strength of
/// the prefix alone, never calling `AuthManager::verify_token`. The middleware now
/// receives the `AuthManager` as state and verifies; the full battery of
/// forged, expired and tampered tokens is in
/// `tests/auth_token_verification.rs`.
#[tokio::test]
async fn auth_enabled_must_refuse_an_unverifiable_bearer_token() {
    let server = test_app_with(TestSettings::new().auth(true, TEST_SECRET).build()).await;

    let response = server
        .get("/v1/models")
        .add_header("authorization", "Bearer not-a-jwt")
        .await;

    assert_eq!(
        response.status_code(),
        StatusCode::UNAUTHORIZED,
        "a bearer token that does not verify against auth.secret_key must be refused"
    );

    // A structurally valid JWT signed with somebody else's secret is refused too.
    let forged = AuthManager::new("another-test-secret-not-a-real-key".to_string(), true)
        .generate_token("intruder", 1)
        .expect("generate_token must succeed");
    assert_eq!(
        server
            .get("/v1/models")
            .add_header("authorization", format!("Bearer {forged}"))
            .await
            .status_code(),
        StatusCode::UNAUTHORIZED
    );
}

/// Schemes other than `Bearer` are refused rather than silently tolerated.
#[tokio::test]
async fn auth_enabled_refuses_a_non_bearer_scheme() {
    let server = test_app_with(TestSettings::new().auth(true, TEST_SECRET).build()).await;

    for header in ["Basic dXNlcjpwYXNz", "bearer lowercase-scheme", "Token abc"] {
        assert_eq!(
            server
                .get("/v1/models")
                .add_header("authorization", header)
                .await
                .status_code(),
            StatusCode::UNAUTHORIZED,
            "Authorization: {header} must be refused"
        );
    }
}

/// `auth.enabled = false` is the shipped default and must stay anonymous: the
/// wiring above may not become a surprise 401 for an existing deployment.
#[tokio::test]
async fn auth_disabled_keeps_serving_anonymously() {
    let server = test_app_with(TestSettings::new().auth(false, TEST_SECRET).build()).await;

    server.get("/health").await.assert_text("OK");
    assert_eq!(server.get("/v1/models").await.status_code(), StatusCode::OK);
    assert_eq!(server.get("/stats").await.status_code(), StatusCode::OK);

    // A credential is not required, and presenting a nonsensical one changes
    // nothing: the middleware is not in the stack at all.
    server
        .get("/health")
        .add_header("authorization", "Basic nonsense")
        .await
        .assert_text("OK");
}

/// Layer order is a decision, not an accident: `auth_middleware` sits *inside*
/// `request_id::add_request_id`, so a refused request still carries the
/// correlation id an operator needs to find it in the log.
#[tokio::test]
async fn a_refused_request_still_carries_its_request_id() {
    let server = test_app_with(TestSettings::new().auth(true, TEST_SECRET).build()).await;

    let response = server
        .get("/v1/models")
        .add_header("x-request-id", "nexus-auth-401")
        .await;

    assert_eq!(response.status_code(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.header("x-request-id").to_str().unwrap(),
        "nexus-auth-401"
    );
}

/// `auth_middleware` also sits inside the CORS layer, so a browser preflight is
/// answered instead of being refused — a 401 on `OPTIONS` would make the
/// gateway unusable from a page even for a caller that holds a token.
#[tokio::test]
async fn a_cors_preflight_is_answered_not_refused() {
    let server = test_app_with(TestSettings::new().auth(true, TEST_SECRET).build()).await;

    let response = server
        .method(axum::http::Method::OPTIONS, "/v1/chat/completions")
        .add_header("origin", "https://nexus.example")
        .add_header("access-control-request-method", "POST")
        .await;

    assert_ne!(
        response.status_code(),
        StatusCode::UNAUTHORIZED,
        "the permissive CORS layer must short-circuit the preflight before auth"
    );
    assert!(
        response
            .maybe_header("access-control-allow-origin")
            .is_some(),
        "the preflight response must carry the CORS headers"
    );
}

// ===========================================================================
// create_app / build_components: the production entry points
// ===========================================================================

/// `create_app` is what `main` calls. It must produce a router that already
/// serves, with no further assembly.
#[tokio::test]
async fn create_app_returns_a_router_that_already_serves() {
    let app = create_app(TestSettings::new().build())
        .await
        .expect("create_app must succeed for in-memory settings");
    let server = axum_test::TestServer::new(app).expect("TestServer over create_app's router");

    server.get("/health").await.assert_text("OK");
    assert_eq!(server.get("/v1/models").await.status_code(), StatusCode::OK);
}

/// And it carries `auth.enabled` all the way from `Settings` to the stack, so
/// the switch works through the production entry point and not only through
/// `build_router`.
#[tokio::test]
async fn create_app_carries_auth_enabled_from_settings_to_the_stack() {
    let app = create_app(TestSettings::new().auth(true, TEST_SECRET).build())
        .await
        .expect("create_app must succeed");
    let server = axum_test::TestServer::new(app).expect("TestServer over create_app's router");

    assert_eq!(
        server.get("/v1/models").await.status_code(),
        StatusCode::UNAUTHORIZED
    );
}

/// `build_components` reads `auth.enabled` and `auth.secret_key` into the
/// `AuthManager` rather than inventing either, and `build_router` obeys
/// `AuthManager::is_enabled`: swapping the manager by hand on an otherwise
/// anonymous component set is enough to protect the router.
///
/// `AppComponents` used to carry an `auth_enabled: bool` beside no manager at
/// all, which is how the switch and the key came to live apart and only the
/// switch was consulted.
#[tokio::test]
async fn build_components_carries_the_auth_manager_and_build_router_obeys_it() {
    let mut components =
        test_components(TestSettings::new().auth(false, TEST_SECRET).build()).await;
    assert!(
        !components.auth.is_enabled(),
        "auth.enabled = false must arrive as false"
    );

    components.auth = std::sync::Arc::new(AuthManager::new(TEST_SECRET.to_string(), true));
    let server = axum_test::TestServer::new(build_router(components))
        .expect("TestServer over the hand-flipped router");
    assert_eq!(
        server.get("/v1/models").await.status_code(),
        StatusCode::UNAUTHORIZED
    );

    let enabled = test_components(TestSettings::new().auth(true, TEST_SECRET).build()).await;
    assert!(
        enabled.auth.is_enabled(),
        "auth.enabled = true must arrive as true"
    );
    // The secret arrived with it: a token minted from the same string verifies
    // against the manager the gateway built.
    let token = AuthManager::new(TEST_SECRET.to_string(), true)
        .generate_token("nexus-operator", 1)
        .expect("generate_token must succeed");
    assert_eq!(
        enabled
            .auth
            .verify_token(&token)
            .expect("the manager must hold auth.secret_key, not a default")
            .sub,
        "nexus-operator"
    );
}

/// `claude.use_interactive_sessions = true` makes `build_components` call
/// `InteractiveSessionManager::prewarm_default_session`, whose `Err` arm logs
/// "Failed to pre-warm Claude process".
///
/// Nothing is pre-warmed: the method is a `TODO` stub that returns `Ok(())`
/// without spawning. The proof is this test — `claude.command` points at a
/// binary that cannot exist, so a real pre-warm would have to fail, and
/// `build_components` nonetheless returns a router that serves. The error arm in
/// `build_components` is therefore unreachable as the code stands.
#[tokio::test]
async fn the_interactive_prewarm_never_spawns_anything() {
    let settings = TestSettings::new().interactive_sessions(true).build();
    assert_eq!(
        settings.claude.command,
        support::config::no_such_command(),
        "the pre-warm must be given a command it cannot possibly spawn"
    );

    let server = test_app_with(settings).await;
    server.get("/health").await.assert_text("OK");
}

// ===========================================================================
// /v1/sessions and /v1/projects: the handlers that stay unmounted
// ===========================================================================

/// The route table does not mention them, so the paths are `404` — including
/// the `POST` verbs the handlers were written for.
///
/// Note that `/v1/sessions/:conversation_id/interrupt` *is* mounted (by
/// `api::chat::interrupt_session`), which is why `/v1/sessions` looks like it
/// should exist.
#[tokio::test]
async fn the_session_and_project_collections_are_not_served() {
    let server = test_app_with(TestSettings::new().build()).await;

    for path in ["/v1/sessions", "/v1/projects"] {
        for response in [server.get(path).await, server.post(path).await] {
            assert_eq!(
                response.status_code(),
                StatusCode::NOT_FOUND,
                "{path} is not in build_router's table"
            );
            // axum's own fallback: no handler ran, so there is no body at all.
            assert_eq!(
                response.text(),
                "",
                "an unrouted 404 must carry no body, unlike a handler's 404"
            );
        }
    }

    // The one `/v1/sessions/...` path that *is* mounted also answers 404 for an
    // unknown conversation — but it is `interrupt_session` answering, not the
    // router's fallback, and the body proves which. This rules out "the whole
    // prefix is blocked" as an explanation for the 404s above.
    let interrupted = server
        .post("/v1/sessions/unknown-conversation/interrupt")
        .await;
    assert_eq!(interrupted.status_code(), StatusCode::NOT_FOUND);
    assert_eq!(
        interrupted.json::<serde_json::Value>(),
        serde_json::json!({
            "error": "session not found",
            "conversation_id": "unknown-conversation"
        }),
        "interrupt_session is mounted and answered"
    );
}

/// `interrupt_session` does not speak the error envelope the rest of the
/// gateway speaks.
///
/// Every `ApiError` renders as `{"error": {"message", "type", "param", "code"}}`
/// — an object, per the OpenAI shape clients expect. `interrupt_session` builds
/// its JSON by hand and emits `{"error": "<string>", "conversation_id": …}`
/// instead, so a client that reads `error.type` to classify a failure finds
/// nothing on this one route.
///
/// Pinned rather than fixed: `api::chat::interrupt_session` is outside this
/// file's ownership.
#[tokio::test]
async fn interrupt_session_answers_a_different_error_shape_than_the_rest() {
    let server = test_app_with(TestSettings::new().build()).await;

    let odd_one_out: serde_json::Value = server
        .post("/v1/sessions/unknown-conversation/interrupt")
        .await
        .json();
    assert!(
        odd_one_out["error"].is_string(),
        "interrupt_session puts a bare string under `error`"
    );

    let envelope: serde_json::Value = server
        .post("/v1/chat/completions")
        .json(&support::openai::request().build())
        .await
        .json();
    assert!(
        envelope["error"].is_object() && envelope["error"]["type"] == "invalid_request_error",
        "every ApiError puts an object under `error`, got {envelope}"
    );
}

/// Why they stay unmounted: the handlers have nothing to serve.
///
/// `api::sessions::list_sessions` builds a `Vec<SessionInfo>` locally and
/// returns it empty — it reads no store, so no session can ever appear in it.
/// `api::sessions::create_session` ignores its (absent) input and answers
/// `{"message": "Not implemented"}` with `200 OK`, i.e. it reports success for
/// work it did not do.
///
/// Mounting them would advertise a sessions collection that is permanently
/// empty and a creation endpoint that silently discards every request, which is
/// worse than a `404`. Deleting the module is the other option, and that is a
/// call for a human — see the report.
#[tokio::test]
async fn the_unmounted_session_handlers_can_only_answer_placeholders() {
    let listed = claude_code_api::api::sessions::list_sessions()
        .await
        .expect("list_sessions is infallible today")
        .into_response();
    assert_eq!(listed.status(), StatusCode::OK);
    assert_eq!(
        body_json(listed).await,
        serde_json::json!([]),
        "list_sessions returns a freshly built empty Vec, never a stored session"
    );

    let created = claude_code_api::api::sessions::create_session()
        .await
        .expect("create_session is infallible today")
        .into_response();
    assert_eq!(
        created.status(),
        StatusCode::OK,
        "create_session reports 200 for work it did not do"
    );
    assert_eq!(
        body_json(created).await,
        serde_json::json!({"message": "Not implemented"})
    );
}

// ===========================================================================
// Cross-checks against docs/diagrams/nexus-api-misc.mmd
// ===========================================================================

/// The specification, red today.
///
/// Faulty function: `utils::text_chunker::split_text_into_chunks` (and its twin
/// `TextChunker::next_chunk`) in `claude-code-api/src/utils/text_chunker.rs`.
/// Triggering input: any text of multi-byte UTF-8 whose `chunk_size`-th **byte**
/// is not a character boundary, e.g. `"日本語テキスト…"` with the default
/// `chunk_size = 20`.
///
/// `chunk_size` is compared against `remaining.len()`, which is a byte count,
/// and the chunk is then cut with `&remaining[..chunk_end]`. Slicing a `str` at a
/// non-boundary byte panics, so the gateway aborts the task mid-stream instead of
/// chunking. `chunk_text` is live — `api::streaming_handler` calls it on assistant
/// text — so any non-ASCII answer long enough to be chunked can hit this.
///
/// It should chunk on characters (`char_indices`, or `floor_char_boundary`) and
/// reassemble to the original. The fix belongs to `utils/text_chunker.rs`, which
/// is outside this file's ownership — hence `#[ignore]`.
#[test]
#[ignore = "utils::text_chunker::split_text_into_chunks panics on multi-byte UTF-8; fix belongs in utils/text_chunker.rs"]
fn chunking_multibyte_text_must_not_panic() {
    use claude_code_api::utils::text_chunker::{ChunkConfig, split_text_into_chunks};

    let text = "日本語のテキストを分割する必要があります。".repeat(4);
    let chunks = split_text_into_chunks(&text, &ChunkConfig::default());

    let reassembled: String = chunks.concat();
    assert_eq!(
        reassembled, text,
        "chunking must be lossless, whatever the encoding"
    );
}

/// Same story for `api::projects`.
#[tokio::test]
async fn the_unmounted_project_handlers_can_only_answer_placeholders() {
    let listed = claude_code_api::api::projects::list_projects()
        .await
        .expect("list_projects is infallible today")
        .into_response();
    assert_eq!(body_json(listed).await, serde_json::json!([]));

    let created = claude_code_api::api::projects::create_project()
        .await
        .expect("create_project is infallible today")
        .into_response();
    assert_eq!(
        created.status(),
        StatusCode::OK,
        "create_project reports 200 for work it did not do"
    );
    assert_eq!(
        body_json(created).await,
        serde_json::json!({"message": "Not implemented"})
    );
}
