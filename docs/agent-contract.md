# Contrat `agent/` — spécification gelée (v1)

Statut : **gelée le 2026-10-05**, `CONTRACT_VERSION = 1`. Fait autorité pour les trois couloirs
(nexus, backend, frontend) du chantier « harness multi-provider ». Les décisions A1–A45 sont dans
[`harness-consolidation-2026-10-05.md`](harness-consolidation-2026-10-05.md) ; ce document les
traduit en signatures, en formes JSON et en règles vérifiables. En cas de contradiction entre ce
document et le code de `claude-code-sdk-rs/src/agent/`, c'est un bug : l'un des deux est corrigé
dans la même PR, et `CONTRACT_VERSION` monte si une forme sérialisée change.

Le module vit dans le crate `nexus-claude` (`claude-code-sdk-rs/`), sous `nexus_claude::agent`.

Sommaire : 1. Vue d'ensemble · 2. Traits · 3. `SessionSpec` · 4. `AgentEvent` · 5. `Capabilities`
et replis · 6. `ToolPolicy` · 7. Erreurs · 8. `ResumeToken` · 9. Concurrence · 10. Annulation ·
11. Identifiants et environnement · 12. `ModelEndpoint` · 13. Registre · 14. Table
`Message → AgentEvent → ChatEvent` · 15. JSON de contrôle Claude Code · 16. Versionnement,
features cargo · 17. Ce que chaque couloir consomme.

---

## 1. Vue d'ensemble

```
backend (résolveur A16) ──> ProviderRegistry (nexus, unique, A1)
                               │  id d'instance → Arc<dyn AgentProvider>
                               ▼
                         AgentProvider ── capabilities(model) ─> Capabilities (par modèle, A4)
                               │ open(spec) / resume(spec, token)
                               ▼
                         AgentSession ── send_turn ─> flux d'AgentEvent … Done | Error
                               │ answer_permission / answer_question / interrupt / cancel_tools
                               │ set_model / set_policy_mode / out_of_band / close
harness natif = AgentProvider composé sur ModelEndpoint (API brute, A2) + outils MCP seulement
```

Deux traits de fournisseur (A2) : `AgentProvider`/`AgentSession` pour un **harness** (Claude Code,
Codex, ACP, natif) et `ModelEndpoint` pour une **API de modèle brute** (OpenAI-compatible).

Règle transversale (contrainte `d79436cc`) : **une capacité absente rend `ProviderError::Unsupported
{ capability }` ou applique le repli écrit au §5 — jamais un succès silencieux.**

## 2. Traits

```rust
pub const CONTRACT_VERSION: u32 = 1;

pub type EventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

#[async_trait]
pub trait AgentProvider: Send + Sync {
    /// Identifiant d'INSTANCE (clé du registre), p. ex. "claude-code", "deepseek-prod".
    fn id(&self) -> &str;
    /// Famille d'adaptateur.
    fn kind(&self) -> ProviderKind; // ClaudeCode | Native | Codex | Acp | Scripted
    /// État de l'instance : jamais d'erreur, l'échec est une valeur.
    async fn health(&self) -> ProviderHealth;
    /// Modèles connus de l'instance (peut être vide : catalogue inconnu).
    async fn catalog(&self) -> Result<Vec<ModelInfo>, ProviderError>;
    /// Capacités PAR MODÈLE (A4). `None` = modèle par défaut de l'instance.
    fn capabilities(&self, model: Option<&str>) -> Capabilities;
    async fn open(&self, spec: SessionSpec) -> Result<Arc<dyn AgentSession>, ProviderError>;
    async fn resume(&self, spec: SessionSpec, token: ResumeToken)
        -> Result<Arc<dyn AgentSession>, ProviderError>;
}

#[async_trait]
pub trait AgentSession: Send + Sync {
    /// Instantané figé à l'ouverture (A4) : ne change plus, même après `set_model`.
    fn capabilities(&self) -> &Capabilities;
    /// Jeton de reprise courant ; `None` tant que le provider n'en a pas émis.
    fn resume_token(&self) -> Option<ResumeToken>;
    /// Démarre un tour. Le flux se termine par exactement UN événement terminal (`Done` ou `Error`).
    async fn send_turn(&self, input: TurnInput) -> Result<EventStream, ProviderError>;
    async fn answer_permission(&self, request_id: &str, decision: PermissionDecision)
        -> Result<(), ProviderError>;
    async fn answer_question(&self, question_id: &str, answer: QuestionAnswer)
        -> Result<(), ProviderError>;
    async fn interrupt(&self, scope: InterruptScope) -> Result<InterruptOutcome, ProviderError>;
    /// Annule des outils SANS finir le tour (A6).
    async fn cancel_tools(&self, scope: CancelScope) -> Result<CancelOutcome, ProviderError>;
    async fn set_model(&self, model: &str) -> Result<(), ProviderError>;
    /// Seul le MODE est modifiable à chaud ; `allow`/`deny` sont figés à l'ouverture.
    async fn set_policy_mode(&self, mode: PolicyMode, native: Option<&str>)
        -> Result<(), ProviderError>;
    /// Flux hors tour, à prendre UNE fois (§9). `None` : déjà pris.
    fn out_of_band(&self) -> Option<EventStream>;
    async fn close(&self) -> Result<(), ProviderError>;
}
```

Toutes les méthodes prennent `&self` : `answer_permission`, `interrupt` et `cancel_tools` doivent
être appelables pendant que le flux du tour est consommé par une autre tâche (C9). L'utilitaire
`agent::run_turn(&dyn AgentSession, TurnInput) -> Result<TurnSummary, ProviderError>` agrège un
tour (texte final, usage, coût) pour les usages « one-shot » du runner.

Types d'accompagnement :

| Type | Forme |
|---|---|
| `ProviderKind` | `claude_code \| native \| codex \| acp \| scripted` (`#[non_exhaustive]`) |
| `ProviderHealth` | `{ status: ok \| degraded \| unavailable, version: Option<String>, detail: Option<String>, error: Option<ProviderError>, login_hint: Option<String>, checked_at_ms: u64 }` — `login_hint` est la commande à lancer par l'humain (A27), jamais exécutée par PO |
| `ModelInfo` | `{ id, display_name: Option<String>, context_window: Option<ContextWindow>, supports_tools: Option<bool>, supports_images: Option<bool>, supports_thinking: Option<bool>, is_default: bool, pricing: Option<ModelPrice> }` |
| `ModelPrice` | `{ input_per_mtok, output_per_mtok, cache_read_per_mtok: Option, cache_write_per_mtok: Option }` en USD par million de tokens |
| `TurnInput` | `{ blocks: Vec<InputBlock> }` ; `TurnInput::text(s)` |
| `InputBlock` | `text { text } \| image { media_type, data_base64 }` — images hors v1 (A12) : refusées par `Unsupported { capability: "images" }` si `Capabilities.images == false` |
| `PermissionDecision` | `allow { scope: once \| session \| always, updated_input: Option<Value> } \| deny { message: Option<String>, interrupt: bool }` — si `updated_input` est `None`, **l'adaptateur rejoue l'entrée d'origine** (le backend n'a plus à la conserver) |
| `QuestionAnswer` | `{ answers: Vec<{ question: String, selected: Vec<String>, free_text: Option<String> }> } \| cancelled` |
| `InterruptScope` | `turn_and_tools \| turn_only` |
| `CancelScope` | `all \| task { id }` |
| `InterruptOutcome` / `CancelOutcome` | `{ turn_interrupted: bool, tools_cancelled: u32, diagnostic: Option<ProcessDiagnostic> }` / `{ tools_cancelled: u32, diagnostic: Option<ProcessDiagnostic> }` |
| `ProcessDiagnostic` | `{ pid: Option<u32>, killed_pids: Vec<u32> }` — diagnostic seulement, optionnel sur le fil (A6) |
| `TurnSummary` | `{ text, stop_reason, usage, cost, is_error }` |

## 3. `SessionSpec` (A9)

```rust
pub struct SessionSpec {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub system_prompt: Option<SystemPromptSpec>,   // { text, mode: replace | append }
    pub policy: ToolPolicy,
    pub policy_ceiling: Option<ToolPolicy>,        // plafond ; open() refuse si policy le dépasse
    pub mcp_servers: BTreeMap<String, McpServerSpec>,
    pub extra_dirs: Vec<PathBuf>,
    pub max_turns: Option<u32>,
    pub env: EnvSpec,                              // { inherit: Vec<String>, set: BTreeMap<String,String> }
    pub hooks: Option<Arc<dyn SessionHooks>>,      // jamais sérialisé
    pub limits: SessionLimits,                     // { max_cost_usd, max_tokens, turn_timeout_ms, max_tool_iterations }
    pub lineage: Option<Lineage>,                  // { parent_session_id, root_session_id, depth, spawned_by }
    pub deltas: bool,                              // demander les deltas de flux (défaut true)
    pub extensions: BTreeMap<String, Value>,       // par provider, clé = ProviderKind ("claude_code", …)
}
```

- `McpServerSpec` : `stdio { command, args, env } | http { url, headers } | sse { url, headers }`.
  Les valeurs de `env` et `headers` peuvent porter un jeton : `Debug` les remplace par
  `<redacted:N>` et elles ne voyagent jamais sur argv (§11).
- `EnvSpec.inherit` : noms **ajoutés** à la liste blanche de base (§11) ; `EnvSpec.set` : variables
  posées explicitement (p. ex. `PATH` enrichi). Une liste blanche seule ne suffit pas (C10).
- `SessionHooks` (A7), trait neutre :
  ```rust
  #[async_trait]
  pub trait SessionHooks: Send + Sync {
      async fn before_tool(&self, call: &ToolCallInfo) -> HookVerdict { HookVerdict::Continue }
      async fn after_tool(&self, result: &ToolResultInfo) -> Option<String> { None }      // contexte ajouté
      async fn before_compaction(&self, info: &CompactionInfo) -> Option<String> { None } // instructions
  }
  pub enum HookVerdict { Continue, Deny { reason: String }, ReplaceInput(Value), AddContext(String) }
  ```
  `Capabilities.hooks` dit si le provider les appelle (`in_protocol`), sait seulement lancer une
  commande (`command` — pas d'exécutable relais en v1) ou ne sait pas (`none`). Dans les deux
  derniers cas `open()` **ignore** `spec.hooks` et le backend applique son repli (injection par tour,
  observation de `ToolResult`) : c'est le seul cas où un champ de `SessionSpec` est ignoré, et il est
  signalé par un `ProviderNotice { kind: "hooks_not_supported" }` en tête de `out_of_band()`.
- Extensions reconnues en v1 : `claude_code` = `{ cli_path, setting_sources, betas, fallback_model,
  permission_prompt_tool, append_system_prompt_preset, extra_args }` ; `codex` =
  `{ codex_home, sandbox }` ; `acp` = `{ command, args }` (normalement portés par l'instance).
- Refus d'ouverture (`open` / `resume`) : `policy` plus permissive que `policy_ceiling` →
  `Unsupported { capability: "policy_ceiling" }` ; mode `trust` sans `Capabilities.sandbox != none`
  pour un provider tiers (A35) → `Unsupported { capability: "sandbox" }` ; serveurs MCP demandés
  sans `per_session_mcp` → `Unsupported { capability: "per_session_mcp" }`.

## 4. `AgentEvent`

Sérialisation : `#[serde(tag = "type", rename_all = "snake_case")]`, `#[non_exhaustive]`. Tout
consommateur a un bras `_` (un événement inconnu est ignoré et journalisé, jamais une panique).
`parent` = identifiant de l'appel d'outil parent quand l'événement vient d'un sous-agent imbriqué
(`parent_tool_use_id` côté Claude) ; absent sinon. `seq: Option<u64>` = numéro du message d'origine
côté provider : les événements issus d'un même message partagent le même `seq` (le backend s'en sert pour
regrouper une sortie hors tour en un seul `background_output`).

| Variante (`type`) | Champs |
|---|---|
| `session_started` | `provider_session_id: Option<String>`, `model: Option<String>`, `policy_mode: Option<PolicyMode>`, `native_mode: Option<String>`, `tools: Vec<String>`, `mcp_servers: Vec<McpServerStatus { name, status }>`, `cwd: Option<String>` |
| `user_echo` | `text: String`, `seq`, `parent` — message utilisateur réémis par le provider (rejeu, ou sortie hors tour) |
| `text` | `text: String`, `seq`, `parent` |
| `thinking` | `text: String`, `signature: Option<String>`, `seq`, `parent` |
| `delta` | `kind: text \| thinking \| tool_input`, `text: String`, `index: Option<u32>`, `tool_call_id: Option<String>`, `parent` |
| `tool_call` | `id`, `name`, `input: Value`, `category: ToolCategory`, `canonical: Option<String>`, `input_complete: bool`, `seq`, `parent` |
| `tool_result` | `id` (celui du `tool_call`), `output: Option<ToolOutput>`, `is_error: bool`, `seq`, `parent` |
| `permission_ask` | `request_id`, `tool_name`, `input: Value`, `category`, `canonical`, `tool_call_id: Option<String>`, `scopes: Vec<PermissionScope>`, `parent` |
| `question` | `question_id`, `tool_call_id: Option<String>`, `reply: turn \| call`, `input: Value`, `questions: Vec<QuestionSpec { question, header: Option<String>, options: Vec<{ label, description: Option<String> }>, multi_select: bool }>`, `parent` |
| `compaction` | `phase: started \| completed`, `trigger: Option<manual \| auto>`, `pre_tokens: Option<u64>` |
| `background_tasks` | `tasks: Vec<BackgroundTask>` — instantané COMPLET à chaque émission |
| `task_update` | `phase: started \| progress \| updated \| notification`, `task_id: Option<String>`, `tool_call_id: Option<String>`, `description: Option<String>`, `status: Option<String>`, `summary: Option<String>`, `event_id: Option<String>`, `data: Value` |
| `model_changed` | `model: String` |
| `policy_mode_changed` | `mode: PolicyMode`, `native_mode: Option<String>` |
| `done` | `stop_reason: StopReason`, `subtype: Option<String>`, `is_error: bool`, `result_text: Option<String>`, `usage: Usage`, `cost: Cost`, `duration_ms: u64`, `duration_api_ms: Option<u64>`, `num_turns: u32`, `model: Option<String>`, `provider_session_id: Option<String>`, `structured_output: Option<Value>` |
| `error` | `error: ProviderError` — TERMINAL pour le tour (ou la session si `!error.retryable()` et que le processus est mort) |
| `provider_notice` | `kind: String`, `data: Value` — diagnostic propre au provider ; **aucune information affichée aujourd'hui pour Claude Code n'y passe** (A5) |

Types liés :

- `ToolCategory` (A8) : `command | read | edit | search | web | mcp | agent | other`. Fournie par
  l'adaptateur. `canonical` est l'alias stable de l'outil pour le rendu et les motifs
  (`mcp__<serveur>__<outil>` pour tout outil MCP, quel que soit le nommage natif — opencode nomme
  `serveur_outil`, l'adaptateur normalise).
- `ToolOutput` : `text(String) | blocks(Vec<Value>)` (sérialisé non étiqueté : chaîne ou tableau).
- `PermissionScope` : `once | session | always`.
- `BackgroundTask` : `{ id, kind: shell | monitor | agent | other, description, status: running |
  completed | failed | killed, started_at_ms: Option<u64>, tool_call_id: Option<String>,
  parent: Option<String>, pid: Option<u32> }` (`pid` = diagnostic, optionnel).
- `Usage` : `{ input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
  reasoning_tokens : Option<u64> chacun, context_tokens: Option<u64>, by_model: Vec<ModelUsage> }` ;
  `ModelUsage { model, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
  cost_usd: Option<f64>, context_window: Option<u64> }`. Un compteur inconnu vaut `None`, pas 0.
- `Cost` : `{ usd: Option<f64>, basis: reported | priced | free | subscription | unknown }` (A5,
  A21). `usd == None` quand le modèle n'a pas de prix connu ; jamais 0 fictif, jamais le
  `total_cost_usd` d'un CLI pour un modèle qu'il ne connaît pas.
- `StopReason` : `completed | max_turns | max_tokens | interrupted | refusal | budget_exceeded |
  error`. `subtype` conserve la chaîne native (`success`, `error_max_turns`,
  `error_during_execution`…) : l'auto-continue du backend se décide sur `stop_reason == max_turns`.

Invariants (vérifiés par `testkit::conformance`) :

1. Un flux de tour se termine par exactement un `done` ou un `error`, puis se ferme.
2. Tout `tool_result.id` et tout `permission_ask.tool_call_id` renvoie à un `tool_call.id` déjà émis
   dans la session.
3. Les `delta` sont des indices de progression : le `text` / `thinking` / `tool_call` complet est
   toujours émis ensuite. Un consommateur qui ignore les `delta` ne perd rien.
4. `tool_call` peut être émis deux fois pour le même `id` : d'abord `input_complete: false` (début
   de bloc en flux, entrée vide), puis `input_complete: true` avec l'entrée entière.
5. `session_started` est émis au plus une fois par processus de provider, au premier tour ou sur
   `out_of_band()` s'il arrive avant tout tour.

## 5. `Capabilities` (A4) et replis

Par MODÈLE (`provider.capabilities(model)`), figées sur la session à l'ouverture
(`session.capabilities()`), sérialisées telles quelles dans `GET /api/chat/providers` et
`system_init.capabilities` (A42). `#[non_exhaustive]` ; construction par `Capabilities::none()`
puis affectation des champs.

| Champ | Type | Repli quand absent (obligatoire, un scénario de conformité par ligne) |
|---|---|---|
| `interactive_permissions` | `bool` | Pas de `permission_ask` : la politique seule décide ; ce qui demanderait est REFUSÉ (`tool_result` en erreur « permission denied by policy ») ; `answer_permission` → `Unsupported` |
| `permission_scopes` | `Vec<PermissionScope>` | Une portée non listée dans une réponse → `Unsupported { capability: "permission_scope" }` ; le frontend ne propose que les portées listées |
| `sandbox` | `none \| workspace \| full` | `none` : mode `trust` refusé à l'ouverture pour un provider tiers (A35) |
| `secret_isolation` | `bool` | `false` : le registre refuse l'instance pour un projet soumis au consentement (porte A32) ; jamais ouvert « quand même » |
| `per_session_mcp` | `bool` | `open()` avec `mcp_servers` non vide → `Unsupported { capability: "per_session_mcp" }` |
| `hooks` | `in_protocol \| command \| none` | `spec.hooks` ignoré + `provider_notice { kind: "hooks_not_supported" }` ; repli backend (A7) |
| `subagents` | `nested \| separate_thread \| none` | `none` : repli par session enfant PO (A18) ; `separate_thread` : les événements de l'enfant portent `parent` = identifiant du fil enfant |
| `compaction_signal` | `bool` | Aucun `compaction` émis ; le backend ne promet pas de bandeau ; `before_compaction` jamais appelé |
| `thinking` | `bool` | Aucun `thinking` ; pas d'erreur |
| `images` | `bool` | `send_turn` avec un bloc image → `Unsupported { capability: "images" }` |
| `tools` | `bool` | `open()` avec `mcp_servers` non vide ou préflight d'outil → `ModelNoTools` |
| `context_window` | `Option<{ value: u64, source: reported \| catalog \| configured \| probed \| assumed }>` | `None` : budget en tokens impossible → run budgété refusé (A21) ; jamais de 200000 implicite |
| `set_model_live` | `bool` | `set_model` → `Unsupported { capability: "set_model_live" }` ; modèle verrouillé dans l'UI |
| `native_question` | `bool` | Aucun `question` natif ; le backend synthétise `ask_user_question` et répond par un tour utilisateur (A45) ; `answer_question` → `Unsupported` |
| `tool_cancel` | `bool` | `cancel_tools` → `Unsupported { capability: "tool_cancel" }` ; seul `interrupt` reste |
| `background_tasks` | `bool` | Aucun `background_tasks` / `task_update` ; `cancel_tools(task)` → `Unsupported` |
| `resume` | `bool` | `resume()` → `Unsupported { capability: "resume" }` ; `resume_token()` rend `None` |
| `cost` | `reported \| priced \| free \| subscription \| unknown` | `unknown` : `done.cost.usd == None`, `basis: unknown` ; budget en USD refusé |

Valeurs de référence (v1, à confirmer par la conformité de chaque adaptateur) :

| Capacité | Claude Code | Natif (OpenAI-compat.) | Codex app-server | ACP |
|---|---|---|---|---|
| interactive_permissions | oui | oui (émises par la boucle) | oui | oui |
| permission_scopes | once, session, always | once, session | once, session | once, always |
| sandbox | none | none (outils MCP seuls) | workspace | none |
| secret_isolation | oui (env en liste blanche, MCP hors argv) | oui | oui (`env_vars` par nom) | oui (env par JSON stdio) |
| per_session_mcp | oui | oui | oui (un processus par session) | oui |
| hooks | in_protocol | in_protocol (boucle locale) | command | none |
| subagents | nested | none | separate_thread | none |
| compaction_signal | oui | oui | oui | non |
| thinking | oui | selon modèle | oui | oui |
| images | non en v1 (A12) | non | non | non |
| tools | oui | selon modèle (sonde) | oui | oui |
| context_window | reported | configured / probed | configured | None |
| set_model_live | oui | oui (entre deux tours) | oui (par tour) | non |
| native_question | oui (`AskUserQuestion`) | non | non (expérimental) | non |
| tool_cancel | oui (par PID, dans l'adaptateur) | oui (jeton d'annulation) | non | non |
| background_tasks | oui | non | non | non |
| resume | oui | oui (transcript) | oui | selon `loadSession` |
| cost | reported (subscription si OAuth) | priced / free / unknown | unknown (tokens seuls, A40) | reported si `Cost` présent, sinon unknown |

## 6. `ToolPolicy` (A8)

```rust
pub enum PolicyMode { PlanOnly, Ask, AutoEdits, Trust }           // ordre croissant de permissivité
pub struct ToolPolicy { pub mode: PolicyMode, pub native_mode: Option<String>,
                        pub allow: Vec<ToolPattern>, pub deny: Vec<ToolPattern> }
pub struct ToolPattern { pub tool: String, pub arg: Option<String> } // glob `*` sur le nom et sur l'argument
```

- JSON : `{"mode":"ask","allow":["Read","Bash(git *)"],"deny":["mcp__po__admin"]}`. Un motif se
  (dé)sérialise en chaîne ; la syntaxe Claude `Outil(glob)` est la forme d'entrée et de sortie.
  `Bash(git:*)` (ancienne forme) est accepté et normalisé en `Bash(git *)`.
- Décision locale (`policy.decide(tool, arg) -> allow | ask | deny`), utilisée par le harness natif
  et par les replis : `deny` l'emporte toujours ; puis `allow` ; puis le mode — `trust` → allow ;
  `auto_edits` → allow pour les catégories `read | search | edit`, ask sinon ; `ask` → allow pour
  `read | search`, ask sinon ; `plan_only` → allow pour `read | search`, deny sinon (une entrée `allow` ne lève pas le mode plan). **Motif malformé ou
  mode inconnu → erreur à la construction (`invalid_request`), jamais ignoré** : une entrée `deny`
  perdue élargirait la politique en silence. Un `deny` à argument face à un appel dont l'argument
  est inconnu s'applique (échec fermé).
- `child.restrict(&parent)` est monotone (A17) : mode = le moins permissif des deux ; `deny` =
  union ; `allow` = motifs de l'enfant couverts par un motif du parent (tous si le parent est
  `trust`). `restrict` est idempotent et `a.restrict(&p).is_within(&p)` est toujours vrai.
- `native_mode` : valeur exacte du provider quand l'appelant la connaît (chaînes héritées du
  frontend, A43). L'adaptateur qui la reconnaît l'applique ; les autres l'ignorent.

Correspondance Claude Code (six modes du CLI + `default` hérité) :

| Mode natif | → neutre | neutre → natif émis |
|---|---|---|
| `default` (hérité), `manual` | `ask` | `ask` → `default` (inchangé tant que la façade doit rester identique octet pour octet) |
| `acceptEdits` | `auto_edits` | `auto_edits` → `acceptEdits` |
| `auto` | `auto_edits` (`native_mode: "auto"`) | — (seulement par `native_mode`) |
| `dontAsk` | `ask` (`native_mode: "dontAsk"`) | — (seulement par `native_mode`) |
| `plan` | `plan_only` | `plan_only` → `plan` |
| `bypassPermissions` | `trust` | `trust` → `bypassPermissions` |

Codex : `ask` → `approvalPolicy: on-request` + `sandbox: workspaceWrite` ; `auto_edits` →
`on-request` + `workspaceWrite` (éditions dans l'espace de travail sans demande) ; `plan_only` →
`on-request` + `readOnly` ; `trust` → `never` + `workspaceWrite` (jamais `dangerFullAccess` en v1).
ACP : `session/set_mode` quand l'agent publie des modes, sinon `Unsupported`.

## 7. `ProviderError` (A10)

`#[serde(tag = "kind", rename_all = "snake_case")]`, `#[non_exhaustive]`, `Clone`, `thiserror`.

| Variante (`kind`) | Champs | `retryable()` | HTTP côté backend (A29) |
|---|---|---|---|
| `cli_not_found` | `program` | non | 503 |
| `auth_required` | `login_hint: Option<String>` | non | 401 |
| `credentials_locked` | — | non (pas de repli vers un autre provider, A26) | 423 |
| `unauthorized` | — | non | 401 |
| `endpoint_unreachable` | `detail` | oui | 502 |
| `model_no_tools` | `model` | non | 422 |
| `context_too_small` | `needed: Option<u64>`, `available: Option<u64>` | non | 422 |
| `rate_limited` | `retry_after_ms: Option<u64>` | oui | 429 |
| `overloaded` | — | oui | 503 |
| `timeout` | `after_ms: u64` | oui | 504 |
| `process_exited` | `code: Option<i32>` | non | 502 |
| `protocol` | `detail` | non | 502 |
| `unsupported` | `capability` | non | 501 |
| `turn_in_progress` | — | non | 409 |
| `invalid_request` | `detail` | non | 400 |
| `closed` | — | non | 410 |

Règles : `detail` passe par `agent::redact()` avant construction (en-têtes d'autorisation, valeurs
de `Secret` enregistrées, URL avec identifiants) ; **jamais d'identifiant dans un message
d'erreur**, corps HTTP tronqué à 512 octets. `From<SdkError> for ProviderError` vit dans
`providers/claude_code/` : `CliNotFound → cli_not_found`, `Timeout → timeout`,
`ProcessExited → process_exited`, `NotSupported → unsupported`, le reste → `protocol`. La
classification des erreurs Anthropic (`overloaded`, `rate_limited`, « prompt is too long » →
`context_too_small`, 401 → `unauthorized`) est faite par l'adaptateur Claude Code à partir du texte
de `done.result_text` quand `is_error`, et non par le backend.

## 8. `ResumeToken` (A3)

```rust
pub struct ResumeToken { kind: ProviderKind, version: u32, data: serde_json::Value }
```

Opaque : seul le provider émetteur lit `data`. Sérialisé en une chaîne JSON compacte par
`to_wire()` / `from_wire()` (`{"k":"claude_code","v":1,"d":{…}}`), persistée telle quelle sur la
session (`ChatSession.resume_token`). `resume()` avec un jeton d'un autre `kind` →
`invalid_request`. Contenu par provider (informatif, non contractuel) : Claude Code
`{"session_id"}` ; natif `{"transcript_id"}` ; Codex `{"thread_id"}` ; ACP `{"session_id"}`.
Migration : `ResumeToken::claude_code_session(id)` construit un jeton à partir d'un
`cli_session_id` déjà persisté. Une reprise ne re-résout jamais le provider (A16).

## 9. Concurrence (A13)

- **Un seul tour à la fois.** `send_turn` pendant un tour actif → `Err(TurnInProgress)`. Aucune
  file, aucune interruption implicite : la file d'attente reste côté backend.
- Le tour est « actif » de l'acceptation de `send_turn` jusqu'à l'émission de l'événement terminal
  (pas jusqu'à la fin de la consommation du flux). Lâcher le flux (`drop`) n'interrompt pas le
  tour : les événements restants sont redirigés vers `out_of_band()`.
- **Exactement une destination par événement.** Pendant un tour, tout va dans le flux du tour ;
  hors tour, tout va dans `out_of_band()` (permission levée par un sous-agent ou un outil de fond,
  `background_tasks`, `task_update`, `compaction`, `session_started` précoce, `provider_notice`).
- `out_of_band()` : un seul consommateur ; premier appel `Some`, suivants `None`. Tampon borné
  (1024) ; au débordement les plus anciens sont perdus et un `provider_notice { kind: "lagged",
  data: { "dropped": n } }` est émis — jamais silencieux.
- `answer_permission` / `answer_question` : appelables depuis n'importe quelle tâche, pendant ou
  hors tour. Identifiant inconnu ou déjà répondu → `invalid_request`.
- `close()` est idempotent ; après `close`, toute méthode rend `Closed`. Un flux de tour en cours
  reçoit `error { closed }`.

## 10. Annulation (A6)

| Opération | Effet | Événements attendus |
|---|---|---|
| `interrupt(turn_and_tools)` | fin du tour + arrêt des outils en cours | `tool_result` en erreur pour chaque outil coupé (si le provider les émet), puis `done { stop_reason: interrupted }` |
| `interrupt(turn_only)` | fin du tour, tâches de fond épargnées | `done { stop_reason: interrupted }` |
| `cancel_tools(all)` | arrêt des outils en cours, **tour préservé** | `tool_result { is_error: true }` puis le tour CONTINUE jusqu'à son `done` normal |
| `cancel_tools(task { id })` | arrêt d'une tâche de fond | `background_tasks` sans la tâche (ou `status: killed`) |

`interrupt` hors tour → `Ok` avec `turn_interrupted: false`. Le suivi par PID (descendants du CLI,
`pgrep`, signal au sous-arbre) est un détail de `providers/claude_code/` ; le contrat ne porte que
`ProcessDiagnostic`, optionnel. Le natif annule par jeton d'annulation ; Codex et ACP n'ont que
`interrupt` (`tool_cancel: false`).

## 11. Identifiants et environnement (A11, A33)

```rust
pub struct Secret(/* privé */);            // ni Debug utile, ni Display, ni Serialize ; zéroïsé au drop
impl Secret { pub fn expose(&self) -> &str }
pub enum CredentialRef { Vault(String), Env(String), None } // "vault:<nom>" | "env:<VAR>" | "none"
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// `instance` = id de l'instance qui demande (grant `Provider(instance)`, A26).
    async fn resolve(&self, instance: &str, reference: &CredentialRef)
        -> Result<Option<Secret>, ProviderError>; // vault verrouillé → CredentialsLocked
}
```

Le backend fournit le résolveur (vault) ; nexus fournit `EnvCredentialResolver` (références `env:`
et `none` seulement, `vault:` → `CredentialsLocked`). Un identifiant n'est jamais stocké dans une
config d'instance : seulement sa référence. `Debug` de `Secret` imprime `Secret(<redacted>)`.

Lanceur unique `transport::spawn::isolated_command(program, &EnvPolicy)` — **`Command::new` est
interdit dans `providers/`** (test de garde par recherche textuelle) :

- `env_clear()` puis liste blanche de base : `PATH, HOME, USER, LOGNAME, SHELL, LANG, LC_ALL,
  LC_CTYPE, TERM, TMPDIR, TZ, SSL_CERT_FILE, SSL_CERT_DIR, NODE_EXTRA_CA_CERTS, HTTP_PROXY,
  HTTPS_PROXY, NO_PROXY` (et minuscules), `SYSTEMROOT, COMSPEC, PATHEXT, APPDATA, LOCALAPPDATA,
  USERPROFILE, TEMP, TMP` sous Windows ; plus, pour Claude Code seulement : `ANTHROPIC_*`,
  `CLAUDE_*`, `AWS_*`/`GOOGLE_*`/`VERTEX_*` listés nommément par l'instance (Bedrock/Vertex) ; plus
  `EnvSpec.inherit` ; puis `EnvSpec.set`.
- `HOME` dédié par instance pour tout provider hors Claude Code (Claude garde le `HOME` réel : son
  authentification y vit).
- Secrets MCP hors argv : la configuration MCP est écrite dans un fichier `0600` d'un répertoire
  `0700`, passé par chemin (`--mcp-config <fichier>`), supprimé à la fermeture de la session.
- Compatibilité : `ClaudeCodeOptions` (ancien chemin, clients directs) garde l'héritage complet
  **par défaut** tant que le backend n'a pas migré (`EnvPolicy::InheritAll`) ; `ClaudeCodeProvider`
  et tous les autres providers utilisent `EnvPolicy::Allowlist` par défaut.

## 12. `ModelEndpoint` (A2)

```rust
#[async_trait]
pub trait ModelEndpoint: Send + Sync {
    fn id(&self) -> &str;                                   // id d'instance
    async fn health(&self) -> ProviderHealth;
    async fn models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream, ProviderError>;
    async fn probe(&self, model: &str) -> Result<EndpointProbe, ProviderError>; // sonde d'outil, A30
}
pub type CompletionStream = Pin<Box<dyn Stream<Item = Result<CompletionChunk, ProviderError>> + Send>>;
```

- `CompletionRequest { model, messages: Vec<ChatMessage>, tools: Vec<ToolSpec>, max_tokens,
  temperature, parallel_tool_calls: Option<bool>, stream: bool }` ; `ChatMessage { role: system |
  user | assistant | tool, content: Option<String>, reasoning: Option<String>, tool_calls,
  tool_call_id }`.
- `CompletionChunk` : `text(String) | reasoning(String) | tool_call(ToolCallChunk { id, name,
  arguments: String /* JSON entier, assemblé */ }) | usage(Usage) | finish(FinishReason)`. Des
  appels d'outils entiers ou fragmentés donnent le même `tool_call` (A39).
- `EndpointProbe { tools: bool, parallel_tools: Option<bool>, reasoning_field: Option<String>,
  context_window: Option<u64>, checked_at_ms }`, mise en cache par (instance, modèle).
- Quirks = drapeaux d'instance (`EndpointQuirks`) : `echo_reasoning_with_tools` (DeepSeek),
  `omit_tool_choice` (Ollama), `explicit_parallel_tool_calls: Option<bool>` (llama-server, NIM),
  `reasoning_field: reasoning_content | reasoning` (vLLM), `fold_late_system` (message système
  tardif replié dans `system`), `tool_args_as_object`. Préréglages : `deepseek`, `vllm`, `ollama`,
  `llama_server`, `nim`, `generic`.
- Table de prix UNIQUE (`model::pricing::PriceTable`, A1) : coût = usage × prix, `None` sans prix.
- HTTP : `reqwest` derrière la feature `provider-native` ; redirections désactivées ; identifiant
  résolu par rappel à chaque requête, jamais stocké.

## 13. `ProviderRegistry` (A1, A32)

Unique, dans nexus. Le backend n'a qu'un résolveur (quel provider/modèle pour quelle demande).

```rust
pub struct ProviderInstanceConfig {
    pub id: String, pub kind: ProviderKind, pub endpoint: Option<String>,
    pub credential: CredentialRef, pub preset: Option<String>, pub quirks: Option<EndpointQuirks>,
    pub default_model: Option<String>, pub model_aliases: BTreeMap<String, String>,
    pub context_window: Option<u64>, pub cost_source: CostBasis, pub prices: BTreeMap<String, ModelPrice>,
    pub command: Option<Vec<String>>, pub env_inherit: Vec<String>, pub extensions: BTreeMap<String, Value>,
}
impl ProviderRegistry {
    pub fn new(resolver: Arc<dyn CredentialResolver>) -> Self;
    pub fn activate_security_gate(&self, proof: SecurityGate);        // A32
    pub fn upsert(&self, config: ProviderInstanceConfig) -> Result<(), ProviderError>; // rechargement à chaud
    pub fn remove(&self, id: &str) -> bool;
    pub fn get(&self, id: &str) -> Result<Arc<dyn AgentProvider>, ProviderError>;
    pub fn list(&self) -> Vec<ProviderInstanceConfig>;
    pub fn resolve_alias(&self, id: &str, alias: &str) -> Result<String, ProviderError>; // inconnu → invalid_request
}
```

Porte (A32) : `upsert` / `get` d'une instance dont `kind != claude_code` (et `!= scripted` en test)
rend `Unsupported { capability: "security_gate" }` tant que `activate_security_gate` n'a pas été
appelé. Aucun secret dans `Debug` ni dans la sérialisation d'une config. `claude-code` est une
instance intégrée, toujours présente.

## 14. Table `Message` (SDK) → `AgentEvent` → `ChatEvent` (backend)

Relevée le 2026-10-05 sur `claude-code-sdk-rs/src/types.rs` (`Message`, `ContentBlock`,
`StreamEventData`) et sur le backend (`src/chat/types.rs` : 32 variantes de `ChatEvent` ;
`ChatManager::message_to_events`, `parse_permission_control_msg`, `oob_listener::extract_payload`).
La projection `Message → AgentEvent` est la fonction publique
`providers::claude_code::map_message(&Message, &mut MapState) -> Vec<AgentEvent>` ; la projection
`AgentEvent → ChatEvent` est `agent_event_to_chat_event` côté backend.

### 14.1 Événements issus du flux du provider

| Source SDK | `AgentEvent` | Champ `AgentEvent` ← clé SDK | `ChatEvent` | Champ `ChatEvent` ← champ `AgentEvent` |
|---|---|---|---|---|
| `Assistant` → `ContentBlock::Text` | `text` | `text` ← `text` ; `parent` ← `parent_tool_use_id` | `assistant_text` | `content` ← `text` ; `parent_tool_use_id` ← `parent` |
| `Assistant` → `ContentBlock::Thinking` | `thinking` | `text` ← `thinking` ; `signature` ← `signature` (vide → `None`) | `thinking` | `content` ← `text` (signature non transmise) ; `parent_tool_use_id` ← `parent` |
| `Assistant` → `ContentBlock::ToolUse` | `tool_call` (`input_complete: true`) | `id` ← `id` ; `name` ← `name` ; `input` ← `input` ; `category`, `canonical` calculés | `tool_use`, ou `tool_use_input_resolved` si l'`id` a déjà été vu avec `input_complete: false` | `id` ; `tool` ← `name` ; `input` ; `parent_tool_use_id` ← `parent` |
| `StreamEvent` → `ContentBlockStart` avec `content_block.type == "tool_use"` | `tool_call` (`input_complete: false`) | `id` ← `content_block.id` (défaut `""`) ; `name` ← `.name` (défaut `""`) ; `input` ← `.input` (défaut `{}`) | `tool_use` | idem |
| `Assistant` ou `User.content_blocks` → `ContentBlock::ToolResult` | `tool_result` | `id` ← `tool_use_id` ; `output` ← `content` (`Text` → chaîne, `Structured` → tableau, absent → `None`) ; `is_error` ← `is_error.unwrap_or(false)` | `tool_result` | `id` ; `result` ← `output` (chaîne, tableau, ou `null` si absent) ; `is_error` ; `parent_tool_use_id` |
| `User` sans `content_blocks` (texte) | `user_echo` | `text` ← `content` | en tour : rien (comme aujourd'hui) ; hors tour : `background_output { source: "user" }` | — |
| `StreamEvent` → `ContentBlockDelta` → `TextDelta` | `delta { kind: text }` | `text` ← `text` ; `index` ← `index` | `stream_delta` | `text` ; `parent_tool_use_id` ← `parent` |
| `StreamEvent` → `ContentBlockDelta` → `ThinkingDelta` | `delta { kind: thinking }` | `text` ← `thinking` | (aucun aujourd'hui) | — |
| `StreamEvent` → `ContentBlockDelta` → `InputJsonDelta` | `delta { kind: tool_input }` | `text` ← `partial_json` ; `tool_call_id` ← id du bloc d'index égal | (aucun aujourd'hui) | — |
| `StreamEvent` → `MessageStart`, `MessageDelta`, `ContentBlockStop`, `MessageStop`, `ContentBlockStart` non `tool_use` | (rien) | — | — | — |
| `System { subtype: "init" }` | `session_started` | `provider_session_id` ← `data.session_id` ; `model` ← `data.model` ; `tools` ← `data.tools[]` (chaînes) ; `mcp_servers` ← `data.mcp_servers[]` (`name`, `status`) ; `native_mode` ← `data.permissionMode` ; `policy_mode` ← table §6 ; `cwd` ← `data.cwd` | `system_init` | `cli_session_id` ← `provider_session_id` (défaut `""`) ; `model` ; `tools` ; `mcp_servers` ← `[{name, status}]` ; `permission_mode` ← `native_mode` ; nouveaux (A42) : `provider`, `capabilities`, `tool_policy` ajoutés par le backend |
| `System { subtype: "compact_boundary" }` | `compaction { phase: completed }` | `trigger` ← `data.compact_metadata.trigger` (`"manual"`/`"auto"`, absent → `auto`) ; `pre_tokens` ← `data.compact_metadata.pre_tokens` | `compact_boundary` | `trigger` (chaîne) ; `pre_tokens` |
| hook `PreCompact` (callback en protocole, pas un `Message`) | `compaction { phase: started }` | `trigger` ← `HookInput::PreCompact.trigger` | `compaction_started` | `trigger` |
| `System { subtype: task_started \| task_progress \| task_updated \| task_notification }` | `task_update` | `phase` ← suffixe du sous-type ; `task_id` ← `data.task_id` ; `tool_call_id` ← `data.tool_use_id` ; `description`, `status`, `summary` ← clés homonymes ; `event_id` ← `data.uuid` ; `data` ← `data` entier | `workflow` | `subtype` ← `"task_" + phase` ; `data` ← `data` (dédoublonnage sur `event_id`) |
| `System { subtype: "background_tasks_changed" }` | `background_tasks` | `tasks[]` ← `data.tasks[]` (lecture tolérante : `id`\|`task_id`, `type`\|`kind`, `description`\|`command`, `status`) | (compteur `cli_background_tasks` ← `tasks.len()` ; `active_tasks_update` quand B13b remplace le suivi par PID) | — |
| `System` d'un autre sous-type (`status`, inconnu) | `provider_notice` | `kind` ← `subtype` ; `data` ← `data` | en tour : rien ; hors tour : `background_output { source: "system:" + kind, content: pretty(data) }` | vidage opaque, aucun champ lu (seule exception admise à A5 : c'est déjà un vidage JSON aujourd'hui) |
| `Result` | `done` | `subtype` ← `subtype` ; `stop_reason` ← table ci-dessous ; `is_error` ; `result_text` ← `result` ; `duration_ms` ; `duration_api_ms` ; `num_turns` ; `provider_session_id` ← `session_id` ; `cost` ← `{ usd: total_cost_usd, basis: reported }` (ou `subscription`/`unknown` selon l'instance) ; `usage` ← `usage` (`input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`) et `by_model` ← `modelUsage` ; `structured_output` | `result` | `session_id` ← `provider_session_id` ; `duration_ms` ; `cost_usd` ← `cost.usd` ; `subtype` ← `subtype` (défaut d'après `stop_reason`) ; `is_error` ; `num_turns` ; `result_text` ; nouveau : `cost_basis` (A21) |
| contrôle `can_use_tool` (canal de contrôle, pas un `Message`), outil ≠ `AskUserQuestion` | `permission_ask` | `request_id` ← `request_id`\|`requestId` (racine) ; `tool_name` ← `request.tool_name`\|`toolName` (défaut `"unknown"`) ; `input` ← `request.input` (défaut `{}`) ; `tool_call_id` ← `request.tool_use_id`\|`toolUseId` ; `parent` ← dernier `parent_tool_use_id` vu sur le flux ; `scopes` ← `[once, session, always]` | `permission_request` | `id` ← `request_id` ; `tool` ← `tool_name` ; `input` ; `parent_tool_use_id` ← `parent` |
| contrôle `can_use_tool` avec `tool_name == "AskUserQuestion"` | `question { reply: turn }` — l'adaptateur répond lui-même `allow` + `updatedInput: input` (comme le backend aujourd'hui, §15.6) | `question_id` ← `request_id` ; `tool_call_id` ← `tool_use_id` ; `questions` ← `input.questions` (lecture tolérante) ; `input` ← `input` | `ask_user_question` | `id` ← `question_id` ; `tool_call_id` (défaut `""`) ; `questions` ← `input.questions` (brut, défaut `[]`) ; `input` ; `parent_tool_use_id` |
| contrôle `hook_callback` | (aucun événement : traité par l'adaptateur, appelle `SessionHooks`) | — | — | — |
| fin du flux SDK sans `Result` (processus mort) | `error { process_exited }` | `code` si connu | `session_error` | `reason: "subprocess_exited"`, message fixe |
| erreur d'item du flux SDK | `error { … }` (classée, §7) | — | `error` (ou `retrying` si `retryable()` et aucun octet émis) | `message` ← `"Error: " + Display` |

`subtype` → `stop_reason` : `success` → `completed` ; `error_max_turns` → `max_turns` ;
`error_during_execution` → `interrupted` si une interruption a été demandée pendant le tour,
sinon `error` ; `error_max_budget_usd` → `budget_exceeded` ; autre → `error` si `is_error`, sinon
`completed`.

`question.reply` : `turn` = la réponse de l'utilisateur part comme un tour ordinaire
(`send_turn`), l'outil ayant déjà été débloqué par l'adaptateur (Claude Code : comportement actuel
de `InputResponse`) ; `call` = répondre par `answer_question`. `answer_question` sur une question
`reply: turn` → `Unsupported { capability: "answer_question" }`.

Hors tour, le backend reconstruit `background_output` à partir des événements de `out_of_band()`
regroupés par `seq` : `source` = `"assistant"` (groupe venant d'un message assistant),
`"tool_result"` (groupe contenant un `tool_result` venu d'un message utilisateur), `"user"`
(`user_echo`), `"system:<kind>"` (`provider_notice`) ; `content` = lignes jointes par `\n` (`text`
brut, `[thinking] …`, `[tool_use: <name>]`, `[tool_result] …`) ; `correlation_id` ← `parent`.
Règle de provenance, sans état caché : un groupe `seq` qui ne contient que des `tool_result` vient
d'un message utilisateur.

### 14.2 `ChatEvent` synthétisés par le backend (sans source dans le provider)

Inchangés par le contrat, toujours produits par le backend : `user_message`, `system_hint`,
`tool_use_input_resolved` (2ᵉ `tool_call` du même `id`), `tool_cancelled` (appels sans résultat à
l'interruption), `streaming_status`, `pending_queue`, `permission_decision` (après
`answer_permission`), `permission_mode_changed` (après `set_policy_mode` ; ou depuis
`policy_mode_changed` quand le provider change de mode seul), `model_changed` (après `set_model` ;
ou depuis `model_changed`), `compaction_recovery`, `auto_continue`,
`auto_continue_state_changed`, `retrying`, `tools_cancelled` (après `cancel_tools` :
`cli_pid` ← `diagnostic.pid`, `killed_count` ← `tools_cancelled`), `active_tasks_update`,
`secret_request`, `secret_request_resolved`, `session_closed` (A45, émis par `close_session`).

Bilan : les 32 variantes ont une source nommée. 12 viennent d'un `AgentEvent` (`assistant_text`,
`thinking`, `tool_use`, `tool_result`, `stream_delta`, `result`, `system_init`,
`compact_boundary`, `compaction_started`, `workflow`, `permission_request`, `ask_user_question`),
3 d'un `AgentEvent` d'erreur ou hors tour (`error`, `session_error`, `background_output`), 17 du
backend seul.

## 15. JSON de contrôle Claude Code (`providers/claude_code/control.rs`)

Tout le JSON de contrôle écrit vers le CLI vit dans ce seul module. Les formes ci-dessous sont
celles que le backend écrit à la main aujourd'hui ; la conformité les compare **octet pour octet**
(hors `request_id` aléatoire) à ce que `fake_claude` reçoit sur stdin. L'ordre des clés est celui
de `serde_json` sans `preserve_order` (alphabétique).

| # | Fonction | JSON |
|---|---|---|
| 15.1 | `permission_allow(request_id, updated_input)` | `{"response":{"request_id":R,"response":{"behavior":"allow","updatedInput":I},"subtype":"success"},"type":"control_response"}` — `I` = `updated_input` ou l'entrée d'origine conservée par l'adaptateur (`{}` si inconnue) |
| 15.2 | `permission_deny(request_id, message)` | `{"response":{"request_id":R,"response":{"behavior":"deny","message":"User denied the permission request"},"subtype":"success"},"type":"control_response"}` (message par défaut ; remplacé si `deny.message` est fourni) |
| 15.3 | `set_permission_mode(mode)` | `{"request":{"mode":M,"subtype":"set_permission_mode"},"request_id":U,"type":"control_request"}` |
| 15.4 | `set_model(model)` | `{"request":{"model":M,"subtype":"set_model"},"request_id":U,"type":"control_request"}` |
| 15.5 | `interrupt()` | `{"request":{"request_id":U,"type":"interrupt"},"type":"control_request"}` (forme de `InteractiveClient::build_interrupt_json`, réutilisée telle quelle) |
| 15.6 | auto-réponse `AskUserQuestion` | forme 15.1 avec `I` = entrée de la question |
| 15.7 | réponse de hook | `build_hook_response_json(request_id, result)` (existant, déplacé sans changement) |

Non reproduit : la forme du relais NATS `{"type":"control_response","response":{"allow":<bool>}}`
(sans `request_id` ni `behavior`). Le relais devient un appel à `answer_permission` sur
l'instance propriétaire, donc la forme 15.1/15.2 (tâche backend B12b).

Portée d'une autorisation : `scope: once` → 15.1 seul ; `session` / `always` → 15.1 avec
`"updatedPermissions"` reprenant les suggestions du CLI (`request.permission_suggestions`) pour la
destination `session` / `localSettings`. Le backend n'envoie que `once` aujourd'hui.

`SessionSpec` → `ClaudeCodeOptions` (identique à `ChatManager::build_options`) : `model`, `cwd`,
`system_prompt` (`replace` → `--system-prompt`, `append` → `--append-system-prompt`),
`permission_mode` ← §6, `max_turns`, `include_partial_messages` ← `deltas`,
`permission_prompt_tool_name` ← `"stdio"`, `cli_channel_buffer_size` ← 8192, `mcp_servers`,
`allowed_tools` / `disallowed_tools` ← `policy.allow` / `policy.deny` (omis si vides), `resume` ←
jeton, `add_dirs` ← `extra_dirs`, `env` ← `EnvSpec.set`, `cli_path` ← extension, hooks
`PreToolUse` / `PostToolUse` / `PreCompact` (matcher `None`) ← `SessionHooks` quand présent.

## 16. Versionnement et features cargo (A12, A14)

- `agent::CONTRACT_VERSION: u32`. Monte de 1 à chaque changement d'une forme sérialisée
  (`AgentEvent`, `Capabilities`, `ProviderError`, `ToolPolicy`, `ResumeToken`) ou d'une signature de
  trait. Les instantanés JSON de `tests/agent_contract_snapshots.rs` portent la version : changer
  une forme sans changer la version fait échouer le test.
- Toutes les énumérations publiques du contrat sont `#[non_exhaustive]`.
- **Additif (A14)** : rien de public n'est retiré tant que le backend en dépend —
  `InteractiveClient`, `clone_stdin_sender`, `take_sdk_control_receiver`, `MockTransport`,
  `nexus_claude::memory`, `find_claude_cli`, `cli_download::*`, les types `Hook*`.
- Features du crate `nexus-claude` :

| Feature | Contenu | Par défaut |
|---|---|---|
| (aucune) | `agent/` (types, traits, registre), `providers/claude_code/` | oui |
| `auto-download`, `memory` | existantes, inchangées | `auto-download` oui |
| `testkit` | `testkit/` : conformité, `ScriptedProvider`, rejeu de transcriptions | non (`dev-dependencies` du backend) |
| `provider-native` | `model/` (HTTP + SSE, `reqwest`), `providers/native/` | non |
| `provider-codex` | `providers/codex/` | non |
| `provider-acp` | `providers/acp/` | non |

- Côté backend : `nexus-claude = { …, features = ["memory", "auto-download", "provider-native",
  "provider-codex", "provider-acp"] }` et `features = ["testkit"]` en `dev-dependencies`. Pendant
  l'intégration : surcharge locale non commitée `[patch]` vers le worktree nexus (B3) ; l'épingle
  `rev` n'est montée qu'à la fin, sur un sha poussé de `integration/harness-multi-provider`.
- Faux exécutables (`fake_claude`, `fake_openai`, `fake_codex`, `fake_acp`) : binaires du crate,
  non construits pour un crate dépendant. Le backend teste par `testkit::ScriptedProvider` et par
  `testkit::claude_code_replay(transcript)` (provider Claude monté sur un transport de rejeu, avec
  capture de stdin).

## 17. Ce que chaque couloir consomme

- **Backend** : §2–§11 pour `ChatManager` sur `AgentSession` ; §13 pour le résolveur (le registre
  est celui de nexus) ; §14 pour `agent_event_to_chat_event` ; §15 pour supprimer ses JSON écrits à
  la main ; §7 pour le statut HTTP et le `code` d'erreur (`code` = `kind`).
- **Frontend** : §5 (forme JSON de `capabilities`, valeurs `snake_case`), §6 (forme JSON de
  `tool_policy`, modes neutres), §7 (`kind` → carte d'erreur), `Cost.basis` (§4) pour l'affichage
  du coût par base, `ToolCategory` / `canonical` pour le registre de rendu.
- Exemple de `capabilities` (Claude Code) :
  ```json
  {"interactive_permissions":true,"permission_scopes":["once","session","always"],"sandbox":"none",
   "secret_isolation":true,"per_session_mcp":true,"hooks":"in_protocol","subagents":"nested",
   "compaction_signal":true,"thinking":true,"images":false,"tools":true,
   "context_window":{"value":200000,"source":"reported"},"set_model_live":true,
   "native_question":true,"tool_cancel":true,"background_tasks":true,"resume":true,"cost":"reported"}
  ```
- Exemple de `done` :
  ```json
  {"type":"done","stop_reason":"completed","subtype":"success","is_error":false,"result_text":"ok",
   "usage":{"input_tokens":12,"output_tokens":3,"by_model":[]},"cost":{"usd":0.0004,"basis":"reported"},
   "duration_ms":812,"num_turns":1,"provider_session_id":"…"}
  ```
