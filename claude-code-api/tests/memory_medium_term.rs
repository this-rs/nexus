//! `core::memory::medium_term` — ce que `MediumTermMemory` fait vraiment face à
//! un project-orchestrator factice.
//!
//! Tout passe par `wiremock` : aucune de ces assertions ne touche le réseau, et
//! le seul point de couture est `McpConfig::url`.
//!
//! Les quatre formes d'enveloppe sont faciles à confondre, et le code les lit
//! sans jamais se plaindre :
//!
//! | étage | route | paramètre de recherche | clé d'enveloppe |
//! |---|---|---|---|
//! | plans | `GET /plans` | `search` | `plans` |
//! | tâches | `GET /tasks` | `search` | `tasks` |
//! | décisions | `GET /decisions/search` | `query` | `decisions` |
//! | notes | `GET /notes/search` | `query` | `notes` |
//!
//! Un tableau nu, une mauvaise clé, un 500 ou une socket morte donnent tous
//! « aucun résultat » sans le moindre signal au sujet appelant. Seul un corps
//! `200` inanalysable remonte en `Err` — l'asymétrie est pinnée plus bas.

mod support;

use chrono::{TimeZone, Utc};
use claude_code_api::core::memory::{
    ContextualMemoryProvider, McpConfig, MediumTermMemory, MemoryResult, MemorySource,
};
use serde_json::{Value, json};
use support::http_mocks;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

// ===========================================================================
// Corps de réponse et lecture des requêtes reçues
// ===========================================================================

fn plan_body(id: &str, title: &str, description: &str, status: &str, created_at: &str) -> Value {
    json!({
        "id": id,
        "title": title,
        "description": description,
        "status": status,
        "created_at": created_at,
    })
}

fn task_body(
    id: &str,
    title: &str,
    description: &str,
    status: &str,
    plan_id: Option<&str>,
) -> Value {
    json!({
        "id": id,
        "title": title,
        "description": description,
        "status": status,
        "plan_id": plan_id,
    })
}

fn decision_body(
    id: &str,
    description: &str,
    rationale: &str,
    chosen_option: Option<&str>,
    task_id: &str,
) -> Value {
    json!({
        "id": id,
        "description": description,
        "rationale": rationale,
        "chosen_option": chosen_option,
        "task_id": task_id,
    })
}

fn note_body(
    id: &str,
    content: &str,
    note_type: &str,
    importance: &str,
    project_id: Option<&str>,
) -> Value {
    json!({
        "id": id,
        "content": content,
        "note_type": note_type,
        "importance": importance,
        "project_id": project_id,
    })
}

/// A project-orchestrator whose `/plans` answers `status` with `body` verbatim,
/// and that has no other route mounted — every other étage gets wiremock's `404`,
/// which the crate reads as "no results".
async fn plans_answering(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    server
}

async fn received(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .expect("wiremock enregistre les requêtes par défaut")
}

fn paths(requests: &[Request]) -> Vec<&str> {
    requests.iter().map(|r| r.url.path()).collect()
}

fn sorted_paths(requests: &[Request]) -> Vec<&str> {
    let mut seen = paths(requests);
    seen.sort_unstable();
    seen
}

fn requests_to<'a>(requests: &'a [Request], route: &str) -> Vec<&'a Request> {
    requests
        .iter()
        .filter(|r| r.url.path() == route)
        .collect::<Vec<_>>()
}

fn request_to<'a>(requests: &'a [Request], route: &str) -> &'a Request {
    let hits = requests_to(requests, route);
    assert_eq!(
        hits.len(),
        1,
        "une seule requête attendue sur {route}, reçu {:?}",
        paths(requests)
    );
    hits[0]
}

fn param(request: &Request, key: &str) -> Option<String> {
    request
        .url
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .map(|v| v.to_str().expect("en-tête ASCII").to_string())
}

fn by_id<'a>(results: &'a [MemoryResult], id: &str) -> &'a MemoryResult {
    results
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("aucun résultat {id} parmi {:?}", ids(results)))
}

fn ids(results: &[MemoryResult]) -> Vec<&str> {
    results.iter().map(|r| r.id.as_str()).collect()
}

const ALL_ROUTES: [&str; 4] = ["/decisions/search", "/notes/search", "/plans", "/tasks"];

// ===========================================================================
// query(): le fan-out sur les quatre étages
// ===========================================================================

/// `query` interroge les quatre étages et donne à chacun un `scope` distinct,
/// fixé en dur par `calculate_score` : décision 0.9, tâche 0.8, plan 0.7,
/// note 0.6. Les métadonnées et le titre diffèrent d'un étage à l'autre.
#[tokio::test]
async fn query_surfaces_the_four_entity_types_with_their_own_scope() {
    let upstream = http_mocks::project_orchestrator(
        vec![plan_body(
            "plan-1",
            "Plan alpha",
            "description alpha",
            "active",
            "2026-01-01T00:00:00Z",
        )],
        vec![task_body(
            "task-1",
            "Tache alpha",
            "faire alpha",
            "todo",
            Some("plan-1"),
        )],
        vec![decision_body(
            "dec-1",
            "Choisir alpha",
            "parce que alpha",
            Some("alpha"),
            "task-1",
        )],
        vec![note_body(
            "note-1",
            "note sur alpha",
            "gotcha",
            "high",
            Some("proj-1"),
        )],
    )
    .await;

    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory
        .query("alpha", 10)
        .await
        .expect("un orchestrateur bien formé ne fait pas échouer query");

    assert_eq!(
        results.len(),
        4,
        "un résultat par étage: {:?}",
        ids(&results)
    );

    let plan = by_id(&results, "plan-1");
    assert_eq!(
        plan.source,
        MemorySource::ProjectOrchestrator {
            entity_type: "plan".to_string(),
            entity_id: "plan-1".to_string(),
        }
    );
    assert_eq!(plan.content, "Plan alpha\n\ndescription alpha");
    assert_eq!(plan.title.as_deref(), Some("Plan alpha"));
    assert_eq!(plan.metadata, json!({ "status": "active" }));
    assert_eq!(plan.score.scope, 0.7);
    assert_eq!(
        plan.timestamp,
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        "`created_at` RFC3339 est repris tel quel"
    );

    let task = by_id(&results, "task-1");
    assert_eq!(
        task.source,
        MemorySource::ProjectOrchestrator {
            entity_type: "task".to_string(),
            entity_id: "task-1".to_string(),
        }
    );
    assert_eq!(task.content, "Tache alpha\n\nfaire alpha");
    assert_eq!(task.title.as_deref(), Some("Tache alpha"));
    assert_eq!(
        task.metadata,
        json!({ "status": "todo", "plan_id": "plan-1" })
    );
    assert_eq!(task.score.scope, 0.8);

    let decision = by_id(&results, "dec-1");
    assert_eq!(
        decision.content,
        "Choisir alpha\n\nRationale: parce que alpha\nChosen: alpha"
    );
    assert_eq!(
        decision.title, None,
        "query() n'attache aucun titre aux décisions"
    );
    assert_eq!(decision.metadata, json!({ "task_id": "task-1" }));
    assert_eq!(decision.score.scope, 0.9);

    let note = by_id(&results, "note-1");
    assert_eq!(
        note.source,
        MemorySource::KnowledgeNote {
            note_id: "note-1".to_string(),
            project_id: Some("proj-1".to_string()),
        },
        "les notes sortent en KnowledgeNote, pas en ProjectOrchestrator"
    );
    assert_eq!(
        note.source.level(),
        3,
        "conséquence: une note de l'étage médian est pondérée comme du long terme \
         par UnifiedMemoryProvider::apply_weight"
    );
    assert_eq!(note.content, "note sur alpha");
    assert_eq!(note.title, None);
    assert_eq!(
        note.metadata,
        json!({ "note_type": "gotcha", "importance": "high" })
    );
    assert_eq!(note.score.scope, 0.6);
}

/// Les quatre routes n'ont pas le même nom de paramètre de recherche : `search`
/// pour `/plans` et `/tasks`, `query` pour `/decisions/search` et
/// `/notes/search`. Et `query` divise son `limit` par quatre avant de l'envoyer,
/// avec un plancher à 2.
#[tokio::test]
async fn query_sends_search_to_plans_and_tasks_but_query_to_decisions_and_notes() {
    let upstream = http_mocks::project_orchestrator(vec![], vec![], vec![], vec![]).await;
    let memory = http_mocks::medium_term_for(&upstream);

    assert!(
        memory
            .query("recherche", 10)
            .await
            .expect("étages vides")
            .is_empty()
    );

    let requests = received(&upstream).await;
    assert_eq!(sorted_paths(&requests), ALL_ROUTES);

    for route in ["/plans", "/tasks"] {
        let request = request_to(&requests, route);
        assert_eq!(
            param(request, "search").as_deref(),
            Some("recherche"),
            "{route} attend `search`"
        );
        assert_eq!(param(request, "query"), None, "{route} n'a pas de `query`");
    }
    for route in ["/decisions/search", "/notes/search"] {
        let request = request_to(&requests, route);
        assert_eq!(
            param(request, "query").as_deref(),
            Some("recherche"),
            "{route} attend `query`"
        );
        assert_eq!(
            param(request, "search"),
            None,
            "{route} n'a pas de `search`"
        );
    }
    for route in ALL_ROUTES {
        assert_eq!(
            param(request_to(&requests, route), "limit").as_deref(),
            Some("2"),
            "per_type_limit = (10 / 4).max(2) = 2 sur {route}"
        );
    }
}

/// Le plancher `.max(2)` et la division entière sont observables sur le fil :
/// `limit = 1` demande quand même 2 entités par étage, `limit = 100` en demande 25.
#[tokio::test]
async fn the_per_type_limit_is_a_quarter_of_the_requested_limit_with_a_floor_of_two() {
    for (requested, expected_upstream) in [(1_usize, "2"), (7, "2"), (8, "2"), (100, "25")] {
        let upstream = http_mocks::project_orchestrator(vec![], vec![], vec![], vec![]).await;
        let memory = http_mocks::medium_term_for(&upstream);
        memory
            .query("alpha", requested)
            .await
            .expect("étages vides");

        let requests = received(&upstream).await;
        assert_eq!(
            param(request_to(&requests, "/plans"), "limit").as_deref(),
            Some(expected_upstream),
            "limit={requested} doit demander {expected_upstream} plans"
        );
    }
}

/// `query` trie par `score.combined` décroissant puis tronque à `limit`, même
/// quand l'amont a renvoyé davantage d'entités que demandé.
#[tokio::test]
async fn query_sorts_by_combined_score_then_truncates_to_the_requested_limit() {
    let plans = vec![
        plan_body(
            "les-deux",
            "Alpha",
            "beta",
            "active",
            "2026-01-01T00:00:00Z",
        ),
        plan_body("un-seul", "Alpha", "rien", "active", "2026-01-01T00:00:00Z"),
        plan_body("aucun", "Zeta", "rien", "active", "2026-01-01T00:00:00Z"),
    ];

    let upstream = http_mocks::project_orchestrator(plans.clone(), vec![], vec![], vec![]).await;
    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory.query("alpha beta", 10).await.expect("trois plans");

    assert_eq!(ids(&results), ["les-deux", "un-seul", "aucun"]);
    // semantic = mots trouvés / mots de la requête ; recency = 0.5 en dur ;
    // scope = 0.7 pour un plan. combined = 0.5*s + 0.3*0.5 + 0.2*0.7.
    assert_eq!(results[0].score.semantic, 1.0);
    assert!((results[0].score.combined - 0.79).abs() < 1e-9);
    assert_eq!(results[1].score.semantic, 0.5);
    assert!((results[1].score.combined - 0.54).abs() < 1e-9);
    assert_eq!(
        results[2].score.semantic, 0.0,
        "un plan sans aucun mot de la requête est quand même renvoyé: \
         l'étage médian ne filtre jamais sur la pertinence"
    );

    let upstream = http_mocks::project_orchestrator(plans.clone(), vec![], vec![], vec![]).await;
    let memory = http_mocks::medium_term_for(&upstream);
    let two = memory.query("alpha beta", 2).await.expect("trois plans");
    assert_eq!(
        ids(&two),
        ["les-deux", "un-seul"],
        "truncate garde les meilleurs"
    );

    let upstream = http_mocks::project_orchestrator(plans, vec![], vec![], vec![]).await;
    let memory = http_mocks::medium_term_for(&upstream);
    let none = memory.query("alpha beta", 0).await.expect("trois plans");
    assert!(
        none.is_empty(),
        "limit=0 interroge l'amont puis jette tout: {:?}",
        ids(&none)
    );
}

/// Une requête vide — ou blanche — ne vaut pas 0 : `calculate_score` renvoie
/// `semantic = 0.0`, mais `recency` reste à 0.5 et `scope` à 0.7, donc le plan
/// ressort avec `combined = 0.29`.
#[tokio::test]
async fn an_empty_query_still_returns_every_entity_with_a_zero_semantic_score() {
    let upstream = http_mocks::project_orchestrator(
        vec![plan_body(
            "plan-1",
            "Plan",
            "sans rapport",
            "active",
            "2026-01-01T00:00:00Z",
        )],
        vec![],
        vec![],
        vec![],
    )
    .await;

    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory.query("   ", 10).await.expect("un plan");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].score.semantic, 0.0);
    assert_eq!(results[0].score.recency, 0.5);
    assert!((results[0].score.combined - 0.29).abs() < 1e-9);
}

/// Un `created_at` inanalysable est remplacé par `Utc::now()` sans un mot, et un
/// décalage horaire est ramené à UTC.
#[tokio::test]
async fn an_unparseable_created_at_silently_becomes_now() {
    let before = Utc::now();
    let upstream = http_mocks::project_orchestrator(
        vec![
            plan_body("date-cassee", "Alpha", "un", "active", "pas-une-date"),
            plan_body(
                "avec-offset",
                "Alpha",
                "deux",
                "active",
                "2026-01-01T02:00:00+02:00",
            ),
        ],
        vec![],
        vec![],
        vec![],
    )
    .await;

    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory.query("alpha", 10).await.expect("deux plans");
    let after = Utc::now();

    let broken = by_id(&results, "date-cassee");
    assert!(
        broken.timestamp >= before && broken.timestamp <= after,
        "une date illisible retombe sur l'heure courante, pas sur une erreur: {}",
        broken.timestamp
    );

    assert_eq!(
        by_id(&results, "avec-offset").timestamp,
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        "le décalage +02:00 est converti en UTC"
    );
}

// ===========================================================================
// Les quatre façons de ne rien trouver
// ===========================================================================

/// Un `500` sur toutes les routes donne `Ok(vec![])` : l'appelant ne peut pas
/// distinguer « l'orchestrateur est en panne » de « il n'y a rien ».
#[tokio::test]
async fn an_upstream_failure_degrades_to_an_empty_result_without_an_error() {
    let upstream = http_mocks::project_orchestrator_failing(500).await;
    let memory = http_mocks::medium_term_for(&upstream);

    let results = memory
        .query("alpha", 10)
        .await
        .expect("un 500 amont n'est pas remonté");
    assert!(results.is_empty());

    assert_eq!(
        sorted_paths(&received(&upstream).await),
        ALL_ROUTES,
        "les quatre étages ont bien été interrogés avant d'abandonner"
    );
}

/// Une URL sans schéma fait échouer `send()` dès la construction de la requête :
/// c'est la branche `Err(e) => { warn!(...); Ok(vec![]) }` des quatre
/// `search_*`, atteinte sans ouvrir une seule socket.
#[tokio::test]
async fn a_transport_error_is_swallowed_by_every_stage() {
    let memory = MediumTermMemory::new(McpConfig {
        url: "orchestrateur-sans-schema".to_string(),
        api_key: None,
    });

    assert!(
        memory
            .query("alpha", 10)
            .await
            .expect("query avale les erreurs de transport")
            .is_empty()
    );
    assert!(
        memory
            .get_relevant_decisions("alpha", 5)
            .await
            .expect("get_relevant_decisions aussi")
            .is_empty()
    );
    for filter in ["plan", "task", "decision"] {
        assert!(
            memory
                .search_context("alpha", Some(filter), 5)
                .await
                .unwrap_or_else(|e| panic!("search_context({filter}) ne doit pas échouer: {e}"))
                .is_empty()
        );
    }
}

/// Un tableau nu à la place de `{"plans": [...]}` est lu comme « aucun plan ».
/// `data.get("plans")` sur un `Value::Array` vaut `None`, et le code tombe sur
/// le `Ok(vec![])` final sans rien journaliser.
#[tokio::test]
async fn a_bare_array_body_is_read_as_no_results() {
    let upstream = plans_answering(
        200,
        r#"[{"id":"plan-1","title":"Alpha","description":"d","status":"active","created_at":"2026-01-01T00:00:00Z"}]"#,
    )
    .await;

    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory.query("alpha", 10).await.expect("200 analysable");

    assert!(
        results.is_empty(),
        "un tableau nu est ignoré en silence: {:?}",
        ids(&results)
    );
    assert_eq!(
        requests_to(&received(&upstream).await, "/plans").len(),
        1,
        "le vide vient bien de la forme du corps, pas d'un appel manquant"
    );
}

/// Même silence quand l'enveloppe existe sous la mauvaise clé — et ce silence
/// vaut pour les quatre étages, chacun ne cherchant que la sienne.
#[tokio::test]
async fn a_wrong_envelope_key_is_read_as_no_results_on_every_stage() {
    let upstream = MockServer::start().await;
    Mock::given(path_regex(r".*"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"items":[{"id":"entite-1","title":"Alpha","description":"d","status":"active","created_at":"2026-01-01T00:00:00Z","rationale":"r","task_id":"t","content":"c","note_type":"n","importance":"i"}]}"#,
        ))
        .mount(&upstream)
        .await;

    let memory = http_mocks::medium_term_for(&upstream);
    assert!(
        memory
            .query("alpha", 10)
            .await
            .expect("200 analysable")
            .is_empty(),
        "`items` au lieu de plans, tasks, decisions ou notes ne déclenche \
         aucun avertissement, alors que les entités sont par ailleurs valides"
    );
    assert_eq!(
        sorted_paths(&received(&upstream).await),
        ALL_ROUTES,
        "les quatre étages ont répondu 200 et ont tous été lus comme vides"
    );
}

/// Un élément incomplet du tableau est jeté par le `filter_map(... .ok())` :
/// les voisins valides passent, la perte n'est pas signalée.
#[tokio::test]
async fn a_malformed_entity_is_dropped_and_its_neighbours_survive() {
    let upstream = plans_answering(
        200,
        r#"{"plans":[
            {"id":"complet","title":"Alpha","description":"d","status":"active","created_at":"2026-01-01T00:00:00Z"},
            {"id":"sans-titre","description":"d","status":"active","created_at":"2026-01-01T00:00:00Z"}
        ]}"#,
    )
    .await;

    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory.query("alpha", 10).await.expect("200 analysable");

    assert_eq!(
        ids(&results),
        ["complet"],
        "le plan sans `title` disparaît sans trace"
    );
}

/// L'asymétrie à retenir : socket morte et `500` donnent `Ok(vec![])`, mais un
/// corps `200` inanalysable remonte en `Err` via le `?` de `response.json()`.
/// Une seule réponse mal formée fait donc échouer toute la requête, y compris
/// `UnifiedMemoryProvider::query` qui propage avec `?`.
#[tokio::test]
async fn a_malformed_body_is_the_only_failure_that_propagates() {
    let upstream = plans_answering(200, "<html>je ne suis pas du json</html>").await;
    let memory = http_mocks::medium_term_for(&upstream);

    let error = memory
        .query("alpha", 10)
        .await
        .expect_err("un corps inanalysable doit remonter");
    let reqwest_error = error
        .downcast_ref::<reqwest::Error>()
        .expect("l'erreur vient de reqwest");
    assert!(
        reqwest_error.is_decode(),
        "erreur de décodage attendue, vu: {reqwest_error}"
    );

    assert_eq!(
        requests_to(&received(&upstream).await, "/tasks").len(),
        0,
        "l'échec sur /plans interrompt le fan-out: les tâches, décisions et \
         notes ne sont jamais demandées"
    );
}

// ===========================================================================
// Les coutures de portée : project_id, api_key, scope
// ===========================================================================

/// `with_project` ne filtre que deux étages sur quatre : `/plans` et
/// `/notes/search` reçoivent `project_id`, `/tasks` et `/decisions/search` non.
/// Les tâches et les décisions d'un autre projet remontent donc dans le
/// contexte — voir aussi le test ignoré plus bas.
#[tokio::test]
async fn with_project_only_filters_plans_and_notes() {
    let upstream = http_mocks::project_orchestrator(vec![], vec![], vec![], vec![]).await;
    let memory = http_mocks::medium_term_for(&upstream).with_project("proj-42".to_string());

    memory.query("alpha", 10).await.expect("étages vides");

    let requests = received(&upstream).await;
    for route in ["/plans", "/notes/search"] {
        assert_eq!(
            param(request_to(&requests, route), "project_id").as_deref(),
            Some("proj-42"),
            "{route} doit porter le filtre de projet"
        );
    }
    for route in ["/tasks", "/decisions/search"] {
        assert_eq!(
            param(request_to(&requests, route), "project_id"),
            None,
            "{route} ignore le projet courant (comportement constaté)"
        );
    }
}

/// `set_project` écrase la valeur posée par `with_project`, et `None` retire le
/// paramètre de la requête suivante.
#[tokio::test]
async fn set_project_overrides_then_clears_the_filter() {
    let upstream = http_mocks::project_orchestrator(vec![], vec![], vec![], vec![]).await;
    let mut memory = http_mocks::medium_term_for(&upstream).with_project("proj-1".to_string());

    memory.set_project(Some("proj-2".to_string()));
    memory.query("alpha", 10).await.expect("étages vides");

    memory.set_project(None);
    memory.query("alpha", 10).await.expect("étages vides");

    let requests = received(&upstream).await;
    let plans = requests_to(&requests, "/plans");
    assert_eq!(plans.len(), 2, "deux passages sur /plans");
    assert_eq!(
        param(plans[0], "project_id").as_deref(),
        Some("proj-2"),
        "set_project(Some) écrase with_project"
    );
    assert_eq!(
        param(plans[1], "project_id"),
        None,
        "set_project(None) retire le filtre"
    );
}

/// `McpConfig::api_key` n'est posé en `Authorization: Bearer` que sur `/plans`.
/// Les trois autres étages partent sans authentification.
#[tokio::test]
async fn the_api_key_is_only_sent_to_the_plans_endpoint() {
    let upstream = http_mocks::project_orchestrator(vec![], vec![], vec![], vec![]).await;
    let memory = MediumTermMemory::new(McpConfig {
        url: upstream.uri(),
        api_key: Some("jeton-factice".to_string()),
    });

    memory.query("alpha", 10).await.expect("étages vides");

    let requests = received(&upstream).await;
    assert_eq!(
        header(request_to(&requests, "/plans"), "authorization").as_deref(),
        Some("Bearer jeton-factice")
    );
    for route in ["/tasks", "/decisions/search", "/notes/search"] {
        assert_eq!(
            header(request_to(&requests, route), "authorization"),
            None,
            "{route} part sans Authorization (comportement constaté)"
        );
    }
}

/// `scope` est un champ mort : `set_scope` le mémorise, `current_scope` le
/// relit, et aucune requête n'en porte la moindre trace. Le seul filtre qui
/// atteint l'orchestrateur est `project_id`.
#[tokio::test]
async fn the_scope_is_stored_but_never_reaches_the_orchestrator() {
    let upstream = http_mocks::project_orchestrator(vec![], vec![], vec![], vec![]).await;
    let mut memory = http_mocks::medium_term_for(&upstream);

    assert_eq!(memory.current_scope(), None, "aucune portée au départ");

    memory.set_scope(Some("espace-de-travail-1".to_string()));
    assert_eq!(
        memory.current_scope().as_deref(),
        Some("espace-de-travail-1")
    );

    memory.query("alpha", 10).await.expect("étages vides");
    for request in received(&upstream).await {
        assert!(
            !request.url.as_str().contains("espace-de-travail-1"),
            "la portée ne doit apparaître dans aucune URL: {}",
            request.url
        );
    }

    memory.set_scope(None);
    assert_eq!(memory.current_scope(), None, "set_scope(None) efface");
}

// ===========================================================================
// search_context(): le routage par filtre
// ===========================================================================

/// `search_context(Some("plan"))` n'interroge que `/plans`, passe `limit` tel
/// quel — sans la division par quatre de `query` — et perd les métadonnées
/// `status` que `query` attache.
#[tokio::test]
async fn search_context_plan_queries_only_plans_and_drops_the_status_metadata() {
    let upstream = http_mocks::project_orchestrator(
        vec![plan_body(
            "plan-1",
            "Plan alpha",
            "description alpha",
            "active",
            "2026-01-01T00:00:00Z",
        )],
        vec![task_body("task-1", "Tache alpha", "x", "todo", None)],
        vec![],
        vec![],
    )
    .await;

    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory
        .search_context("alpha", Some("plan"), 7)
        .await
        .expect("un plan");

    assert_eq!(ids(&results), ["plan-1"]);
    assert_eq!(results[0].title.as_deref(), Some("Plan alpha"));
    assert_eq!(
        results[0].metadata,
        Value::Null,
        "search_context n'attache pas le `status`, contrairement à query"
    );
    assert_eq!(results[0].score.scope, 0.7);
    assert_eq!(
        results[0].timestamp,
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    );

    let requests = received(&upstream).await;
    assert_eq!(
        paths(&requests),
        ["/plans"],
        "le filtre `plan` n'interroge pas les autres étages"
    );
    assert_eq!(
        param(&requests[0], "limit").as_deref(),
        Some("7"),
        "le filtre transmet `limit` sans le diviser"
    );
}

/// `search_context(Some("task"))` n'interroge que `/tasks`, garde le titre,
/// perd `status` et `plan_id`, et horodate à l'instant de l'appel.
#[tokio::test]
async fn search_context_task_queries_only_tasks_and_timestamps_with_now() {
    let before = Utc::now();
    let upstream = http_mocks::project_orchestrator(
        vec![plan_body(
            "plan-1",
            "Alpha",
            "x",
            "active",
            "2026-01-01T00:00:00Z",
        )],
        vec![task_body(
            "task-1",
            "Tache alpha",
            "faire alpha",
            "todo",
            Some("plan-1"),
        )],
        vec![],
        vec![],
    )
    .await;

    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory
        .search_context("alpha", Some("task"), 3)
        .await
        .expect("une tâche");
    let after = Utc::now();

    assert_eq!(ids(&results), ["task-1"]);
    assert_eq!(results[0].content, "Tache alpha\n\nfaire alpha");
    assert_eq!(results[0].title.as_deref(), Some("Tache alpha"));
    assert_eq!(
        results[0].metadata,
        Value::Null,
        "ni `status` ni `plan_id` ne survivent au filtre"
    );
    assert_eq!(results[0].score.scope, 0.8);
    assert!(
        results[0].timestamp >= before && results[0].timestamp <= after,
        "une tâche n'a pas de date amont: l'horodatage est celui de l'appel"
    );

    assert_eq!(paths(&received(&upstream).await), ["/tasks"]);
}

/// `search_context(Some("decision"))` délègue mot pour mot à
/// `get_relevant_decisions` : pas de titre, pas de métadonnées, et le `task_id`
/// que `query` exposait disparaît.
#[tokio::test]
async fn search_context_decision_delegates_to_get_relevant_decisions() {
    let upstream = http_mocks::project_orchestrator(
        vec![],
        vec![],
        vec![decision_body(
            "dec-1",
            "Choisir alpha",
            "parce que alpha",
            Some("alpha"),
            "task-1",
        )],
        vec![],
    )
    .await;

    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory
        .search_context("alpha", Some("decision"), 4)
        .await
        .expect("une décision");

    assert_eq!(ids(&results), ["dec-1"]);
    assert_eq!(
        results[0].content,
        "Choisir alpha\n\nRationale: parce que alpha\nChosen: alpha"
    );
    assert_eq!(results[0].title, None);
    assert_eq!(
        results[0].metadata,
        Value::Null,
        "le `task_id` que query attache est perdu par ce chemin"
    );
    assert_eq!(paths(&received(&upstream).await), ["/decisions/search"]);
}

/// Tout filtre que le `match` ne connaît pas — y compris `None` et
/// `Some("note")`, que `UnifiedMemoryProvider::search_context` route pourtant
/// vers l'étage médian — retombe sur `query`, donc sur les quatre étages.
#[tokio::test]
async fn an_unknown_source_filter_falls_back_to_the_full_query() {
    for filter in [None, Some("note"), Some("chat-noir")] {
        let upstream = http_mocks::project_orchestrator(
            vec![plan_body(
                "plan-1",
                "Alpha",
                "x",
                "active",
                "2026-01-01T00:00:00Z",
            )],
            vec![],
            vec![],
            vec![note_body("note-1", "alpha", "gotcha", "high", None)],
        )
        .await;

        let memory = http_mocks::medium_term_for(&upstream);
        let results = memory
            .search_context("alpha", filter, 10)
            .await
            .expect("un plan et une note");

        assert_eq!(
            sorted_paths(&received(&upstream).await),
            ALL_ROUTES,
            "filter={filter:?} doit retomber sur le fan-out complet"
        );
        assert!(
            results.iter().any(|r| r.id == "plan-1"),
            "filter={filter:?} renvoie aussi des plans: {:?}",
            ids(&results)
        );
    }
}

// ===========================================================================
// get_relevant_decisions()
// ===========================================================================

/// `get_relevant_decisions` rend `Chosen: N/A` quand aucune option n'a été
/// retenue, et préserve l'ordre de l'amont : contrairement à `query`, il ne trie
/// pas, donc une décision moins pertinente peut arriver en tête.
#[tokio::test]
async fn get_relevant_decisions_renders_na_and_keeps_the_upstream_order() {
    let upstream = http_mocks::project_orchestrator(
        vec![],
        vec![],
        vec![
            decision_body("sans-choix", "Choisir beta", "car beta", None, "task-2"),
            decision_body(
                "avec-choix",
                "Choisir alpha",
                "car alpha",
                Some("alpha"),
                "task-1",
            ),
        ],
        vec![],
    )
    .await;

    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory
        .get_relevant_decisions("alpha", 5)
        .await
        .expect("deux décisions");

    assert_eq!(
        ids(&results),
        ["sans-choix", "avec-choix"],
        "aucun tri: la décision sans rapport avec `alpha` reste première"
    );
    assert_eq!(
        results[0].content,
        "Choisir beta\n\nRationale: car beta\nChosen: N/A"
    );
    assert_eq!(results[0].score.semantic, 0.0);
    assert_eq!(
        results[1].content,
        "Choisir alpha\n\nRationale: car alpha\nChosen: alpha"
    );
    assert_eq!(results[1].score.semantic, 1.0);
    assert_eq!(results[1].score.scope, 0.9);
    assert_eq!(results[1].title, None);
    assert_eq!(results[1].metadata, Value::Null);

    let requests = received(&upstream).await;
    assert_eq!(
        param(request_to(&requests, "/decisions/search"), "query").as_deref(),
        Some("alpha")
    );
}

/// `limit` est un maximum, dit la documentation du trait. Les trois chemins
/// filtrés le transmettent à l'amont *et* le font respecter localement, comme
/// `query` le fait avec son `truncate` — un orchestrateur bavard ne peut donc
/// pas faire déborder le contexte.
#[tokio::test]
async fn the_filtered_paths_enforce_the_limit_even_when_the_server_ignores_it() {
    let plans = (0..3)
        .map(|i| {
            plan_body(
                &format!("plan-{i}"),
                "Alpha",
                "x",
                "active",
                "2026-01-01T00:00:00Z",
            )
        })
        .collect::<Vec<_>>();
    let tasks = (0..3)
        .map(|i| task_body(&format!("task-{i}"), "Alpha", "x", "todo", None))
        .collect::<Vec<_>>();
    let decisions = (0..3)
        .map(|i| decision_body(&format!("dec-{i}"), "Alpha", "x", None, "t"))
        .collect::<Vec<_>>();

    let upstream = http_mocks::project_orchestrator(plans, tasks, decisions, vec![]).await;
    let memory = http_mocks::medium_term_for(&upstream);

    assert_eq!(
        memory
            .search_context("alpha", Some("plan"), 1)
            .await
            .expect("trois plans")
            .len(),
        1,
        "search_context(plan) doit respecter limit=1"
    );
    assert_eq!(
        memory
            .search_context("alpha", Some("task"), 2)
            .await
            .expect("trois tâches")
            .len(),
        2,
        "search_context(task) doit respecter limit=2"
    );
    assert_eq!(
        memory
            .get_relevant_decisions("alpha", 1)
            .await
            .expect("trois décisions")
            .len(),
        1,
        "get_relevant_decisions doit respecter limit=1"
    );
}

// ===========================================================================
// Bugs constatés et laissés en place — la preuve, pas le correctif
// ===========================================================================

/// BUG (`MediumTermMemory::search_context`) : `source_filter = Some("note")`
/// tombe dans le bras `_` et relance le fan-out complet.
///
/// `UnifiedMemoryProvider::search_context` route explicitement
/// `Some("note")` vers l'étage médian, en comptant sur lui pour filtrer. Comme
/// `medium_term` n'a pas de bras `Some("note")`, un appelant qui demande des
/// notes reçoit plans, tâches et décisions en plus — avec, pour les notes
/// elles-mêmes, un `MemorySource::KnowledgeNote` de niveau 3.
///
/// Attendu : seuls des résultats issus de `/notes/search`.
/// Constaté : les quatre étages, cf.
/// `an_unknown_source_filter_falls_back_to_the_full_query`.
///
/// Non corrigé : le bras manquant est du code neuf (une boucle de conversion de
/// `NoteSummary` vers `MemoryResult`), pas un refactoring à comportement
/// identique.
#[tokio::test]
#[ignore = "documente un bug: search_context(Some(\"note\")) ne filtre pas les notes"]
async fn search_context_note_should_only_return_notes() {
    let upstream = http_mocks::project_orchestrator(
        vec![plan_body(
            "plan-1",
            "Alpha",
            "x",
            "active",
            "2026-01-01T00:00:00Z",
        )],
        vec![],
        vec![],
        vec![note_body("note-1", "alpha", "gotcha", "high", None)],
    )
    .await;

    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory
        .search_context("alpha", Some("note"), 10)
        .await
        .expect("une note");

    assert_eq!(ids(&results), ["note-1"]);
    assert_eq!(paths(&received(&upstream).await), ["/notes/search"]);
}

/// BUG (`MediumTermMemory::search_tasks`, `search_decisions`) : la clé d'API de
/// `McpConfig` n'est posée que par `search_plans`.
///
/// Devant un orchestrateur qui exige une authentification, `/tasks`,
/// `/decisions/search` et `/notes/search` répondent `401`, que les `search_*`
/// lisent comme « aucun résultat ». L'étage médian se réduit alors
/// silencieusement aux plans.
///
/// Attendu : les quatre requêtes portent `Authorization: Bearer <clé>`.
/// Constaté : une seule, cf. `the_api_key_is_only_sent_to_the_plans_endpoint`.
///
/// Non corrigé : poser un en-tête d'authentification sur trois requêtes
/// supplémentaires change ce qui part sur le fil vers un service réel.
#[tokio::test]
#[ignore = "documente un bug: la clé d'API ne part que vers /plans"]
async fn every_stage_should_carry_the_api_key() {
    let upstream = http_mocks::project_orchestrator(vec![], vec![], vec![], vec![]).await;
    let memory = MediumTermMemory::new(McpConfig {
        url: upstream.uri(),
        api_key: Some("jeton-factice".to_string()),
    });

    memory.query("alpha", 10).await.expect("étages vides");

    for request in received(&upstream).await {
        assert_eq!(
            header(&request, "authorization").as_deref(),
            Some("Bearer jeton-factice"),
            "{} part sans authentification",
            request.url.path()
        );
    }
}

/// BUG (`MediumTermMemory::search_tasks`, `search_decisions`) : le projet
/// courant ne filtre ni les tâches ni les décisions.
///
/// `with_project` / `set_project` promettent « the current project ID », et
/// `search_plans` comme `search_notes` l'envoient en `project_id`. Les deux
/// autres étages interrogent l'orchestrateur sans filtre : les tâches et les
/// décisions de tous les projets atterrissent dans le contexte du modèle.
///
/// Attendu : les quatre requêtes portent `project_id`.
/// Constaté : deux, cf. `with_project_only_filters_plans_and_notes`.
///
/// Non corrigé : ajouter un paramètre de requête modifie l'appel émis vers un
/// service réel, qui peut l'interpréter autrement.
#[tokio::test]
#[ignore = "documente un bug: project_id ne filtre pas /tasks ni /decisions/search"]
async fn every_stage_should_be_scoped_to_the_current_project() {
    let upstream = http_mocks::project_orchestrator(vec![], vec![], vec![], vec![]).await;
    let memory = http_mocks::medium_term_for(&upstream).with_project("proj-42".to_string());

    memory.query("alpha", 10).await.expect("étages vides");

    for request in received(&upstream).await {
        assert_eq!(
            param(&request, "project_id").as_deref(),
            Some("proj-42"),
            "{} n'est pas filtré par projet",
            request.url.path()
        );
    }
}

/// BUG (`MediumTermMemory::search_plans` et ses trois jumelles) : deux pannes
/// amont de même nature sont traitées de deux façons opposées.
///
/// `request.send()` en erreur est journalisé puis transformé en `Ok(vec![])`,
/// mais `response.json().await?` propage. Une mauvaise passerelle qui répond
/// `200 text/html` casse donc tout `query()` — et, par propagation, le
/// `UnifiedMemoryProvider` complet, y compris les étages court et long terme
/// qui n'y sont pour rien — là où une panne franche dégrade proprement.
///
/// Attendu : un corps inanalysable est journalisé et donne `Ok(vec![])`, comme
/// les autres pannes amont.
/// Constaté : `Err`, cf. `a_malformed_body_is_the_only_failure_that_propagates`.
///
/// Non corrigé : choisir entre « tout dégrader » et « tout propager » est une
/// décision de conception sur la mémoire contextuelle, pas une correction locale.
#[tokio::test]
#[ignore = "documente une incohérence: le décodage propage là où le transport est avalé"]
async fn a_malformed_body_should_degrade_like_any_other_upstream_failure() {
    let upstream = plans_answering(200, "<html>je ne suis pas du json</html>").await;
    let memory = http_mocks::medium_term_for(&upstream);

    assert!(
        memory
            .query("alpha", 10)
            .await
            .expect("un corps inanalysable devrait être avalé comme un 500")
            .is_empty()
    );
}
