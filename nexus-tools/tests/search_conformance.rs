//! The same suite for every real engine, against `fake_search` (N22): three engines, one set of
//! behaviours — results with titles and URLs, the limit, no results, outages, unreadable
//! answers, timeouts, special characters in the query, no secret anywhere.

use std::sync::Arc;
use std::time::Duration;

use nexus_tools::fake_search::{self, FakeSearch};
use nexus_tools::search::{
    Engine, HtmlBackend, KeySource, KeyedApiBackend, Protection, SearchBackend, SearchError,
    SearchQuery, SearxngBackend, Secret, WebSearchTool,
};
use nexus_tools::web::{FetchConfig, Fetcher};
use nexus_tools::{CallContext, SessionState, Tool};
use serde_json::json;

/// What the fake accepts, and what a wrong-key engine presents. Distinct sentinels: a leak of
/// either is visible.
const KEY: &str = "sk-fake-search-GOOD-3f9a";
const WRONG_KEY: &str = "sk-fake-search-WRONG-77c1";

fn fetcher(timeout: Duration) -> Arc<Fetcher> {
    Arc::new(Fetcher::new(FetchConfig {
        allow_private_network: true, // the fake is on loopback: the operator's own engine
        upgrade_http: false,
        request_timeout: timeout,
        ..FetchConfig::default()
    }))
}

#[derive(Clone, Copy, Debug)]
enum Engine3 {
    Brave,
    Searxng,
    Html,
}

const ENGINES: [Engine3; 3] = [Engine3::Brave, Engine3::Searxng, Engine3::Html];

fn make(kind: Engine3, fake: &FakeSearch, key: &str, timeout: Duration) -> Box<dyn SearchBackend> {
    let base = fake.base();
    match kind {
        Engine3::Brave => Box::new(KeyedApiBackend::new(
            "brave",
            format!("{base}/brave"),
            KeySource::Static(Secret::new(key)),
            fetcher(timeout),
        )),
        Engine3::Searxng => Box::new(SearxngBackend::new(
            format!("{base}/searxng"),
            fetcher(timeout),
        )),
        Engine3::Html => Box::new(HtmlBackend::explicitly_enabled(
            format!("{base}/html/"),
            fetcher(timeout),
        )),
    }
}

fn query(text: &str, limit: usize) -> SearchQuery {
    SearchQuery {
        text: text.into(),
        limit,
    }
}

async fn each(check: impl AsyncFn(Engine3, &FakeSearch)) {
    for kind in ENGINES {
        let fake = fake_search::start(KEY).await;
        check(kind, &fake).await;
    }
}

#[tokio::test]
async fn every_engine_returns_results_with_a_title_and_a_web_address() {
    each(async |kind, fake| {
        let engine = make(kind, fake, KEY, Duration::from_secs(5));
        let hits = engine
            .search(&query("rust async", 20))
            .await
            .unwrap_or_else(|e| panic!("{kind:?}: {e}"));
        assert!(hits.len() >= 10, "{kind:?}: {}", hits.len());
        for hit in &hits {
            assert!(!hit.title.is_empty(), "{kind:?}");
            assert!(
                hit.url.starts_with("http://") || hit.url.starts_with("https://"),
                "{kind:?}: {}",
                hit.url
            );
        }
        // Markup in the engine's extract never reaches the result, and entities are decoded.
        let snippets: Vec<&str> = hits.iter().filter_map(|h| h.snippet.as_deref()).collect();
        assert!(!snippets.is_empty(), "{kind:?}: no extracts");
        for s in &snippets {
            assert!(!s.contains('<') && !s.contains("&amp;"), "{kind:?}: {s}");
        }
        assert!(
            snippets
                .iter()
                .any(|s| s.contains("rust async matched & more")),
            "{kind:?}: {snippets:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn every_engine_respects_the_limit() {
    each(async |kind, fake| {
        let engine = make(kind, fake, KEY, Duration::from_secs(5));
        let hits = engine.search(&query("limit", 3)).await.unwrap();
        assert_eq!(hits.len(), 3, "{kind:?}");
    })
    .await;
}

#[tokio::test]
async fn no_results_is_an_answer_not_an_error() {
    each(async |kind, fake| {
        let engine = make(kind, fake, KEY, Duration::from_secs(5));
        assert_eq!(
            engine.search(&query("!empty", 10)).await.unwrap(),
            vec![],
            "{kind:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn a_server_error_is_unavailable_and_an_unreadable_answer_is_bad_response() {
    each(async |kind, fake| {
        let engine = make(kind, fake, KEY, Duration::from_secs(5));
        let down = engine.search(&query("!500", 10)).await.unwrap_err();
        assert!(
            matches!(down, SearchError::Unavailable(_)),
            "{kind:?}: {down:?}"
        );
        let garbage = engine.search(&query("!garbage", 10)).await.unwrap_err();
        assert!(
            matches!(garbage, SearchError::BadResponse(_)),
            "{kind:?}: {garbage:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn a_slow_engine_times_out() {
    each(async |kind, fake| {
        let engine = make(kind, fake, KEY, Duration::from_millis(400));
        let started = std::time::Instant::now();
        let error = engine.search(&query("!slow", 10)).await.unwrap_err();
        assert_eq!(error, SearchError::Timeout, "{kind:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    })
    .await;
}

#[tokio::test]
async fn an_unreachable_engine_is_unavailable_not_a_crash() {
    for kind in ENGINES {
        // Nothing listens here.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let dead = FakeSearchAt(addr);
        let engine = make_at(kind, &dead);
        let error = engine.search(&query("x1", 5)).await.unwrap_err();
        assert!(
            matches!(error, SearchError::Unavailable(_)),
            "{kind:?}: {error:?}"
        );
    }
}

struct FakeSearchAt(std::net::SocketAddr);

fn make_at(kind: Engine3, at: &FakeSearchAt) -> Box<dyn SearchBackend> {
    let base = format!("http://{}", at.0);
    let fetcher = fetcher(Duration::from_secs(2));
    match kind {
        Engine3::Brave => Box::new(KeyedApiBackend::new(
            "brave",
            format!("{base}/brave"),
            KeySource::Static(Secret::new(KEY)),
            fetcher,
        )),
        Engine3::Searxng => Box::new(SearxngBackend::new(format!("{base}/searxng"), fetcher)),
        Engine3::Html => Box::new(HtmlBackend::explicitly_enabled(
            format!("{base}/html/"),
            fetcher,
        )),
    }
}

#[tokio::test]
async fn special_characters_in_the_query_arrive_intact() {
    each(async |kind, fake| {
        let engine = make(kind, fake, KEY, Duration::from_secs(5));
        for text in [
            "rust & go: \"fast\" + safe?",
            "café über 日本語",
            "a=b&c=d#frag",
            "100% sure; drop table",
        ] {
            let hits = engine
                .search(&query(text, 5))
                .await
                .unwrap_or_else(|e| panic!("{kind:?} {text}: {e}"));
            assert!(
                hits.iter()
                    .any(|h| h.title == format!("Result 1 for {text}")),
                "{kind:?}: the query {text:?} did not arrive intact: {:?}",
                hits.iter().map(|h| &h.title).collect::<Vec<_>>()
            );
        }
    })
    .await;
}

// ---------------------------------------------------------------------------
// Keyed engines
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_rejected_key_a_quota_and_a_rate_limit_are_told_apart() {
    let fake = fake_search::start(KEY).await;
    let wrong = make(Engine3::Brave, &fake, WRONG_KEY, Duration::from_secs(5));
    assert_eq!(
        wrong.search(&query("hello", 5)).await.unwrap_err(),
        SearchError::KeyRejected
    );
    let right = make(Engine3::Brave, &fake, KEY, Duration::from_secs(5));
    assert_eq!(
        right.search(&query("!402", 5)).await.unwrap_err(),
        SearchError::QuotaExceeded
    );
    assert_eq!(
        right.search(&query("!429", 5)).await.unwrap_err(),
        SearchError::RateLimited
    );
    // A 403 from a keyed engine is its key being refused; from SearXNG it is its json format.
    assert_eq!(
        right.search(&query("!403", 5)).await.unwrap_err(),
        SearchError::KeyRejected
    );
    let searx = make(Engine3::Searxng, &fake, KEY, Duration::from_secs(5));
    let error = searx.search(&query("!403", 5)).await.unwrap_err();
    assert!(
        matches!(&error, SearchError::BadResponse(m) if m.contains("json")),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_key_is_resolved_from_its_reference_and_a_missing_reference_is_a_clear_error() {
    let fake = fake_search::start(KEY).await;
    let from_env = |name: &str| {
        KeyedApiBackend::new(
            "brave",
            format!("{}/brave", fake.base()),
            KeySource::Env(name.into()),
            fetcher(Duration::from_secs(5)),
        )
    };
    let missing = from_env("NEXUS_TEST_SEARCH_KEY_THAT_IS_NOT_SET")
        .search(&query("x1", 5))
        .await
        .unwrap_err();
    match &missing {
        SearchError::NotConfigured(why) => assert!(
            why.contains("NEXUS_TEST_SEARCH_KEY_THAT_IS_NOT_SET"),
            "{why}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(fake.requests().is_empty(), "nothing was sent without a key");

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("key");
    std::fs::write(&file, format!("{KEY}\n")).unwrap();
    let engine = KeyedApiBackend::new(
        "brave",
        format!("{}/brave", fake.base()),
        KeySource::File(file),
        fetcher(Duration::from_secs(5)),
    );
    assert!(
        !engine.search(&query("x1", 5)).await.unwrap().is_empty(),
        "a file with a trailing newline works"
    );

    let empty = dir.path().join("empty");
    std::fs::write(&empty, "  \n").unwrap();
    let engine = KeyedApiBackend::new(
        "brave",
        format!("{}/brave", fake.base()),
        KeySource::File(empty),
        fetcher(Duration::from_secs(5)),
    );
    assert!(matches!(
        engine.search(&query("x1", 5)).await,
        Err(SearchError::NotConfigured(_))
    ));
    let engine = KeyedApiBackend::new(
        "brave",
        format!("{}/brave", fake.base()),
        KeySource::File(dir.path().join("nope")),
        fetcher(Duration::from_secs(5)),
    );
    assert!(matches!(
        engine.search(&query("x1", 5)).await,
        Err(SearchError::NotConfigured(_))
    ));
}

// ---------------------------------------------------------------------------
// The key is in a header and nowhere else
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_key_is_sent_in_a_header_only_and_to_the_engine_only() {
    let fake = fake_search::start(KEY).await;
    let engine = make(Engine3::Brave, &fake, KEY, Duration::from_secs(5));
    engine
        .search(&query("where does the key go", 5))
        .await
        .unwrap();
    let request = &fake.requests()[0];
    assert!(
        !request.request_line.contains(KEY),
        "the key is in the URL: {}",
        request.request_line
    );
    assert_eq!(
        request
            .headers
            .iter()
            .find(|(n, _)| n == "x-subscription-token")
            .map(|(_, v)| v.as_str()),
        Some(KEY)
    );
    // Engines that take no key are never sent one.
    for kind in [Engine3::Searxng, Engine3::Html] {
        let fake = fake_search::start(KEY).await;
        make(kind, &fake, KEY, Duration::from_secs(5))
            .search(&query("x1", 5))
            .await
            .unwrap();
        let sent = fake.requests();
        assert!(
            sent.iter().all(|r| !r
                .headers
                .iter()
                .any(|(n, v)| n.contains("token") || n == "authorization" || v.contains(KEY))),
            "{kind:?}"
        );
    }
}

#[tokio::test]
async fn no_key_appears_in_any_debug_error_or_tool_output() {
    let fake = fake_search::start(KEY).await;
    let wrong = KeyedApiBackend::new(
        "brave",
        format!("{}/brave", fake.base()),
        KeySource::Static(Secret::new(WRONG_KEY)),
        fetcher(Duration::from_secs(5)),
    );
    let mut seen = vec![
        format!("{wrong:?}"),
        format!("{:?}", Secret::new(WRONG_KEY)),
        format!("{:?}", KeySource::Static(Secret::new(WRONG_KEY))),
    ];
    let error = wrong.search(&query("x1", 5)).await.unwrap_err();
    seen.push(format!("{error} {error:?}"));

    // Through the tool: a rejected key, then a working engine behind it.
    let backup = SearxngBackend::new(
        format!("{}/searxng", fake.base()),
        fetcher(Duration::from_secs(5)),
    );
    let clock: Arc<dyn nexus_tools::web::Clock> =
        Arc::new(nexus_tools::web::SystemClock::default());
    let engine = Engine::new(clock)
        .with_backend(wrong, Protection::default())
        .with_backend(backup, Protection::default());
    seen.push(format!(
        "{engine_debug:?}",
        engine_debug = WebSearchTool::new(engine)
    ));
    for text in &seen {
        assert!(
            !text.contains(WRONG_KEY) && !text.contains(KEY),
            "a key leaked: {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// Through the tool, over real engines
// ---------------------------------------------------------------------------

async fn call(tool: &WebSearchTool, args: serde_json::Value) -> nexus_tools::ToolResult {
    tool.call(
        &CallContext::new("s", Arc::new(SessionState::default())),
        args,
    )
    .await
}

#[tokio::test]
async fn a_wrong_key_falls_back_to_the_next_engine_and_the_output_is_clean() {
    let fake = fake_search::start(KEY).await;
    let wrong = make(Engine3::Brave, &fake, WRONG_KEY, Duration::from_secs(5));
    let searx = make(Engine3::Searxng, &fake, KEY, Duration::from_secs(5));
    let clock: Arc<dyn nexus_tools::web::Clock> =
        Arc::new(nexus_tools::web::SystemClock::default());
    let tool = WebSearchTool::new(
        Engine::new(clock)
            .with_backend(Boxed(wrong), Protection::default())
            .with_backend(Boxed(searx), Protection::default()),
    );
    let r = call(
        &tool,
        json!({"query": "fallback over http", "blocked_domains": ["blocked.test"]}),
    )
    .await;
    assert!(!r.is_error, "{}", r.text);
    assert!(r.text.contains("(via searxng)"), "{}", r.text);
    assert!(
        !r.text.contains("blocked.test") && !r.text.contains("Leaky"),
        "the leak got through:\n{}",
        r.text
    );
    assert_eq!(
        r.text.matches("site1.test/fallbackoverhttp").count(),
        1,
        "the duplicate was merged:\n{}",
        r.text
    );
    assert!(!r.text.contains(WRONG_KEY) && !r.text.contains(KEY));
    assert!(
        !r.text.contains("utm_source") && !r.text.contains("<strong>"),
        "{}",
        r.text
    );
}

/// Lets a boxed engine be added to an `Engine`.
struct Boxed(Box<dyn SearchBackend>);

#[async_trait::async_trait]
impl SearchBackend for Boxed {
    fn id(&self) -> &str {
        self.0.id()
    }

    async fn search(
        &self,
        q: &SearchQuery,
    ) -> Result<Vec<nexus_tools::search::SearchHit>, SearchError> {
        self.0.search(q).await
    }
}

#[tokio::test]
async fn the_standard_mode_makes_one_request_and_the_extended_mode_several() {
    let fake = fake_search::start(KEY).await;
    let clock: Arc<dyn nexus_tools::web::Clock> =
        Arc::new(nexus_tools::web::SystemClock::default());
    let tool = WebSearchTool::new(Engine::new(clock).with_backend(
        Boxed(make(Engine3::Searxng, &fake, KEY, Duration::from_secs(5))),
        Protection::default(),
    ));
    call(&tool, json!({"query": "how to read a file in rust"})).await;
    assert_eq!(fake.requests().len(), 1);
    let r = call(
        &tool,
        json!({"query": "how to write a file in rust", "mode": "extended"}),
    )
    .await;
    assert!(!r.is_error, "{}", r.text);
    assert!(fake.requests().len() >= 3, "{}", fake.requests().len());
}

#[tokio::test]
async fn the_html_engine_exists_only_when_it_is_built_explicitly() {
    // Nothing in the default set of engines is the HTML one: an empty engine set has none.
    let clock: Arc<dyn nexus_tools::web::Clock> =
        Arc::new(nexus_tools::web::SystemClock::default());
    let default = Engine::new(clock);
    assert!(default.is_empty());
    let r = call(&WebSearchTool::new(default), json!({"query": "anything"})).await;
    assert!(
        r.is_error && r.text.contains("no search engine is configured"),
        "{}",
        r.text
    );
}
