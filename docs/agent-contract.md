# Contrat `agent/` — spécification gelée (v1)

Statut : **gelée le 2026-10-05**, `CONTRACT_VERSION = 2` (v2, même jour : `done.error` ajouté — voir §16). Fait autorité pour les trois couloirs
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
pub const CONTRACT_VERSION: u32 = 2;

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
  `Unsupported { capability: "policy_ceiling" }` ; serveurs MCP demandés
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
| `done` | `stop_reason: StopReason`, `subtype: Option<String>`, `is_error: bool`, `result_text: Option<String>`, `usage: Usage`, `cost: Cost`, `duration_ms: u64`, `duration_api_ms: Option<u64>`, `num_turns: u32`, `model: Option<String>`, `provider_session_id: Option<String>`, `structured_output: Option<Value>`, `error: Option<ProviderError>` (échec classé derrière `is_error` ; le tour finit quand même par `done`, usage et coût conservés) |
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
| `sandbox` | `none \| workspace \| full` | `none` : information pour l'utilisateur (ce qui isole les outils), **pas une barrière** : `trust` s'ouvre sur tout provider (décision du 2026-10-07, remplace A35) |
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

Règle `images` par moteur (A12) : `images` est `false` pour TOUS les moteurs en v1, quoi que le moteur annonce
(`promptCapabilities.image` d'ACP, `supports_images` d'un modèle, entrée image de Codex, `ModelInfo.supports_images`).
Un bloc `image` dans `send_turn` rend `Unsupported { capability: "images" }` ; une capacité réelle d'un moteur ne
bascule `images` à `true` qu'avec une décision de contrat et un scénario `message_images` joué (et non replié).
| tools | oui | selon modèle (sonde) | oui | oui |
| context_window | reported | configured / probed | configured | None |
| set_model_live | oui | oui (entre deux tours) | oui (par tour) | non |
| native_question | oui (`AskUserQuestion`) | non | non (expérimental) | non |
| tool_cancel | oui (par PID, dans l'adaptateur) | oui (jeton d'annulation) | non | non |
| background_tasks | oui | non | non | non |
| resume | oui | oui (transcript) | oui | selon `loadSession` |
| cost | reported (subscription si OAuth) | priced / free / unknown | unknown (tokens seuls, A40) | reported si `Cost` présent, sinon unknown |

Écarts constatés à l'implémentation du natif (`claude-code-sdk-rs/src/providers/native/`, feature
`provider-native`, `NativeProvider` / `NativeConfig`) :

- **`hooks` = `none`**, pas `in_protocol` comme la table de référence : `SessionSpec::hooks` est ignoré
  et `provider_notice { kind: "hooks_not_supported" }` ouvre `out_of_band()` ; `before_tool`,
  `after_tool` et `before_compaction` ne sont jamais appelés (le repli est celui du backend, A7).
- **`capabilities(model)` est synchrone** : il lit ce qui a été sondé. Tant qu'un modèle n'est pas sondé
  (`refresh_capabilities(model)`, ou `open`, qui sonde), `tools` est `false`, `thinking` `false`,
  `context_window` `None` ; rien n'est affirmé sans preuve. `thinking` = la sonde a vu un champ de
  raisonnement. `context_window` : configuré, sinon `probed`, sinon `catalog` ; `None` ⇒ aucune
  compaction automatique (l'erreur typée `context_too_small` sort quand l'endpoint refuse).
  `cost` : `free` si configuré, `priced` si le modèle a un prix, sinon `unknown`.
- **Portées** : `once` et `session` (`always` n'a nulle part où être gardé) ; `always` répond
  `Unsupported { permission_scope }`. `native_question`, `background_tasks`, `subagents`, `images`,
  `sandbox` : absents ; `cancel_tools(task)` → `Unsupported { background_tasks }`,
  `answer_question` → `Unsupported { native_question }`, mode `trust` accepté à
  l'ouverture et à chaud (la politique locale autorise tout appel que ne refuse pas un `deny`).
- **Outils** : uniquement ceux des serveurs MCP de la session, offerts sous le nom
  `mcp__<serveur>__<outil>` (caractères hors `[A-Za-z0-9_-]` remplacés par `_`, 64 caractères au plus) ;
  `category: mcp`, `canonical` = ce nom. Règle d'exposition : un outil n'est pas offert si un `deny`
  sans argument le vise, si le mode est `plan_only` et qu'il n'est pas `readOnlyHint: true`, ou — en
  exposition stricte (défaut) — si `allow` n'est pas vide et ne le nomme pas (**une liste `allow` est une
  liste d'exposition**). Un appel à un outil non offert reçoit un `tool_result` en erreur, sans
  demande de permission. `readOnlyHint: true` compte comme une lecture pour `decide` (donc exécuté sans
  demande en mode `ask`). `auto_edits` se comporte comme `ask` pour MCP. Transports MCP : stdio
  (`isolated_command`, HOME dédié par instance) et HTTP streamable ; le transport HTTP+SSE historique
  (`McpServerSpec::Sse`) est refusé (`Unsupported { mcp_sse }`).
- **Budgets** : `limits.max_tokens` est un budget de **session**, compté sur l'usage rapporté (estimé à
  4 caractères par jeton si l'endpoint n'en rapporte pas : texte envoyé et reçu, schémas d'outils non
  comptés, donc le réel est plus haut) ; **le budget USD avance aussi sur cette estimation** quand le modèle a un
  prix, et chaque requête ainsi comptée émet `provider_notice { kind: "usage_estimated", data: { estimated:
  true, chars_per_token: 4, tokens, usd } }` ; `done.usage` et `done.cost` ne portent que ce que l'endpoint a
  rapporté. **Défauts non nuls** (réglages d'adaptateur dans `NativeConfig`, `SessionSpec` inchangé) : quand le
  `SessionSpec` ne fixe pas la limite, `NativeConfig::new` applique `max_turns` = 50
  (`DEFAULT_MAX_TURNS`), délai de tour = 30 min (`DEFAULT_TURN_TIMEOUT_MS`), budget de session = 10 000 000
  jetons (`DEFAULT_MAX_TOKENS`) ; l'opérateur les lève en mettant le champ à `None`, le `SessionSpec` gagne
  toujours ; il fonctionne sans fenêtre de contexte
  connue (écart à la ligne `context_window` du tableau). `limits.max_cost_usd` sans prix pour le modèle
  → `Unsupported { cost }` à l'ouverture (A21) ; avec prix, ou endpoint `free`, il est tenu. Dépassement
  → `done { stop_reason: budget_exceeded }`, contrôlé avant chaque requête et avant d'exécuter les
  outils d'une réponse. `max_turns` / `limits.max_tool_iterations` bornent les allers-retours modèle d'un
  tour (`max_turns`).
- **Fin de tour** : une erreur d'endpoint classée est un `done { is_error, error }` (usage et coût
  gardés) et le transcript revient à l'état d'avant le tour ; `error { process_exited }` terminal
  seulement quand un serveur MCP stdio meurt (la session est alors morte : `send_turn` rend la même
  erreur) ; un délai de tour (`turn_timeout_ms`) est un `done { error: timeout }`.
  `done.provider_session_id` = identifiant du transcript. Les `tool_result` sont émis à leur fin ; le
  modèle les reçoit dans l'ordre des appels.
- **Transcript** : `ResumeToken { transcript_id }` ; stockage mémoire par défaut, fichiers JSON `0600`
  (répertoire `0700`) en option ; le fichier est expurgé (`agent::redact` par tronçons) : un identifiant
  collé dans un message n'est pas persisté, donc le texte rechargé peut différer du texte envoyé. Le
  raisonnement est conservé (A39), y compris par la compaction (les derniers messages gardés sont
  intacts). Pas de compaction manuelle (le contrat n'en prévoit pas pour le natif).
- **Hors tour** : le natif émet `session_started` (et la notice `hooks_not_supported`) sur `out_of_band()`
  dès l'ouverture ; un flux de tour lâché envoie le reste du tour (permissions comprises) hors tour.
  La conformité de `permission_hors_tour` est montée ainsi (`DetachedTurn` dans
  `tests/native_conformance.rs`).
- **Serveurs MCP stdio** : une ligne de plus de 8 Mio (`MAX_LINE_BYTES`) n'est jamais accumulée au-delà de la
  borne : la lecture s'arrête au dépassement, jette le reste de la ligne et répond aux requêtes en attente par une
  erreur `protocol` (même lecteur borné pour Codex et ACP, où la ligne est signalée `malformed`).
- **Serveurs MCP HTTP** : connexion épinglée sur les adresses validées (`resolve_to_addrs`, résolveur
  injectable `McpConfig::dns_resolver`), nom re-vérifié à chaque requête (`an_http_server_connection_is_pinned_…`,
  `an_http_server_name_that_rebinds_…`, rouges sans épinglage).
- **Non prouvé** : aucun https réel ; `Mcp-Session-Id`
  expiré (404) n'est pas rejoué ; un serveur MCP qui renvoie des requêtes (`sampling`, `roots`) reçoit
  `method not found`.

Écarts constatés à l'implémentation de Codex (`claude-code-sdk-rs/src/providers/codex/`, feature
`provider-codex`, `CodexProvider` / `CodexConfig`) ; **vérifié sur un vrai `codex-cli` 0.160.0 jusqu'à la
création d'un thread** (poignée de main `initialize`, `initialized` et `thread/start`, dans un `CODEX_HOME`
jetable, sans connexion ni appel de modèle : `tests/codex_real.rs`, ignoré, `NEXUS_REAL_CODEX` ;
`tests/codex_real_schema.rs`, hors ligne ; fichiers réels sous `tests/transcripts/codex/0.160.0/`, dont
`PROVENANCE.md`). Cela a trouvé un bug que le faux exécutable cachait : `sandbox` de `thread/start` est en
**kebab-case** (`workspace-write`), le serveur refusait `workspaceWrite` (`-32600`) ; `turn/start` garde son
objet `sandboxPolicy` en camelCase. **Aucun tour réel, aucun appel d'outil réel, aucune approbation réelle** :
cela exige une connexion dans le `CODEX_HOME` propre à l'instance, qu'un humain fait (A27) ; le reste est joué
contre `fake_codex` et des transcriptions écrites depuis le README de `codex-rs/app-server` au tag
`rust-v0.130.0` (`tests/transcripts/codex/0.130.0/schema/PROVENANCE.md` liste ce qui n'est PAS établi) :

- **`hooks` = `none`**, pas `command` comme la table de référence : pas d'exécutable relais en v1 (A40) ;
  `SessionHooks` ignoré, `provider_notice { hooks_not_supported }` en tête d'`out_of_band()`.
- **`permission_scopes` = `once`, `session`, `always`** (la table disait `once`, `session`) : chaque
  `permission_ask` n'offre que les portées que le serveur annonce (approbation de commande ou de fichier :
  `once` + `session` ; élicitation MCP : selon `_meta.persist`) ; une portée non offerte répond
  `Unsupported { permission_scope }`. Codex ne réécrit pas l'entrée d'un appel approuvé : `Allow` avec
  `updated_input` répond `Unsupported { permission_updated_input }`.
- **`native_question` = `false`** : aucun `question` n'est émis (repli §5 : le backend synthétise la question,
  réponse par un tour) ; `item/tool/requestUserInput` est expérimental et jamais activé.
- **`subagents` = `separate_thread`** : `parent` = identifiant du fil enfant (`collabToolCall`) ; la livraison à
  ce client des événements du fil enfant n'est PAS vérifiée.
- **`context_window`** : celle de la configuration (`configured`), sinon `None` ; la fenêtre rapportée par le
  serveur n'entre pas dans les capacités figées.
- **`cost`** : `unknown` par défaut, `free` ou `priced` selon la configuration, jamais `reported` (jetons
  seuls, A40). `done.usage` = différence du cumul `total` de `thread/tokenUsage/updated` entre le début et la
  fin du tour (l'usage rejoué après `thread/resume`, avant `turn/started`, n'est pas celui du tour) ;
  `input_tokens` exclut les jetons en cache (`cache_read_tokens`), comme pour Claude Code.
- **Politique** (§6) : `ask` et `auto_edits` → `on-request` + `workspaceWrite` ; `plan_only` → `on-request` +
  `readOnly` ; `trust` → `never` + `workspaceWrite`, jamais `dangerFullAccess`. `set_policy_mode` et `set_model`
  agissent au tour suivant (surcharges collantes de `turn/start`). La politique neutre s'applique d'abord
  localement aux demandes d'approbation (un `deny`, `plan_only` pour un non-lecture : refus sans demander ;
  un `allow`, `auto_edits` pour une édition : accord) ; ce qu'un outil fait sans demander n'est pas vu par les
  motifs (`provider_notice { policy_patterns_partial }`).
- **Limites** : `limits.turn_timeout_ms` tenu (`done { error: timeout }`) ; `max_turns`, `max_tokens`,
  `max_cost_usd`, `max_tool_iterations` → `Unsupported { limits }` à l'ouverture.
- **Fin de tour** : `turn/completed { failed }` → `done { is_error, error }` (`codexErrorInfo` :
  `ContextWindowExceeded` → `context_too_small`, `UsageLimitExceeded` → `rate_limited`, `HttpConnectionFailed` /
  `ResponseStreamDisconnected` / `ResponseStreamConnectionFailed` / `ResponseTooManyFailedAttempts` →
  `endpoint_unreachable`, `Unauthorized` → `unauthorized` ou `auth_required`, `BadRequest` → `invalid_request`,
  `SandboxError` / `InternalServerError` / `Other` → `protocol`) ; `error { process_exited }` terminal
  seulement quand le processus meurt (la session est alors morte).
- **Processus** : un `codex app-server` par session, lancé par `isolated_command` (liste blanche d'environnement,
  `CODEX_HOME` et `CODEX_API_KEY` posés explicitement) ; `CODEX_HOME` persistant **par instance** (créé `0700`) ;
  serveurs MCP de la session par `-c mcp_servers.<nom>.…` (jamais de secret sur argv : `env_vars`,
  `bearer_token_env_var`, `env_http_headers` par nom de variable) ; `close` tue le processus **et ses
  descendants** (groupe de processus et table des processus).
- **Variables MCP générées** (`NEXUS_MCP_<NOM>_BEARER`, `…_HEADER_<n>`) : le nom est replié en `[A-Z0-9_]` ; deux
  serveurs dont les noms se replient pareil (`a-b` / `a_b`), ou un serveur stdio qui poserait lui-même la même
  variable, sont refusés (`invalid_request`) au lieu de se partager le secret de l'autre.
- **`open` / `resume`** contrôlent la version comme `health()` : < `MIN_APP_SERVER_VERSION` →
  `Unsupported { app_server }`, avant toute résolution de clé ou tout lancement du serveur.
- **`health()`** : `codex --version` seul ; version < `MIN_APP_SERVER_VERSION` (0.130.0) → `Unavailable`,
  `error: Unsupported { app_server }` ; binaire absent → `cli_not_found` ; ni clé configurée ni
  `<CODEX_HOME>/auth.json` → `auth_required` avec `login_hint` = `CODEX_HOME=<dir> codex login` (jamais exécutée,
  A27).
- **Non vérifié** : tout le protocole (aucune session réelle) ; en particulier l'orthographe d'`approvalPolicy`
  et de `sandbox` à `thread/start`, la forme de `thread/tokenUsage/updated`, `_meta` de l'élicitation MCP et
  la réponse d'une approbation persistante, `-c` avant `app-server`, `CODEX_API_KEY` lue par `app-server`.

Écarts constatés à l'implémentation d'ACP (`claude-code-sdk-rs/src/providers/acp/`, feature `provider-acp`,
`AcpProvider` / `AcpConfig`, client générique de l'Agent Client Protocol, protocole version 1) ; **aucune session
réelle** : opencode (`opencode acp`) et Gemini CLI n'ont PAS été exécutés, tout est joué contre `fake_acp` et des
transcriptions écrites depuis les pages publiques d'`agentclientprotocol.com`
(`tests/transcripts/acp/1/schema/PROVENANCE.md` liste ce qui n'est PAS établi) :

- **`command` obligatoire** (programme + arguments) ; un argument qui ressemble à un secret est refusé
  (`invalid_request`, rien de répété) ; `env_inherit` (noms) + `env` explicite de l'instance + `SessionSpec::env.set`
  sur une liste blanche ; `cost_basis` `unknown` / `free` / `priced` (jamais `reported` : jetons seuls, quand l'agent
  en donne, A40).
- **`resume` = `agentCapabilities.loadSession`**, **appris** par `health()` (ou par le dernier `open`) : `capabilities()`
  est synchrone et lit ce qui a été appris, `false` tant que rien ne l'a été (écart à la table, qui disait « selon
  `loadSession` »). Sans `loadSession` : `resume()` → `Unsupported { resume }`. L'historique que l'agent rejoue à
  `session/load` est écarté (`provider_notice { history_replayed }`).
- **`images` = `false`** (A12) quoi que dise `promptCapabilities.image`. **`thinking`** = `AcpConfig::thinking` ou un
  `agent_thought_chunk` déjà vu par ce provider (ACP n'a aucun indicateur de capacité pour cela) ; un chunk de
  réflexion non déclaré est écarté avec `provider_notice { thinking_not_declared }`.
- **`permission_scopes` = `once`, `always`** ; chaque `permission_ask` n'offre que les portées pour lesquelles l'agent
  publie une option `allow_once` / `allow_always` ; `session` et une portée non offerte répondent `Unsupported
  { permission_scope }`. Un refus choisit `reject_once`, sinon `reject_always`, sinon répond `cancelled`. `Allow` avec
  `updated_input` → `Unsupported { permission_updated_input }`.
- **`hooks` = `none`**, `subagents` = `none`, `compaction_signal`, `background_tasks`, `tool_cancel`, `native_question`
  = non ; `sandbox` = `none` (information : `trust` s'ouvre ; à chaud il passe par un mode que l'agent publie, sinon `Unsupported { set_policy_mode }`) ;
  `per_session_mcp` / `tools` oui : `mcpServers` de `session/new` (env et en-têtes en tableaux `{name, value}`, dans
  ce JSON seulement ; HTTP / SSE selon `mcpCapabilities`, sinon `Unsupported { mcp_http | mcp_sse }`).
- **`context_window`** : celle de la configuration (`configured`), sinon `None`. **`set_model_live` = non** :
  `session/set_model` est instable, `set_model` → `Unsupported { set_model_live }` ; `SessionSpec::model` n'est
  qu'une étiquette de `done.model` et du prix (`provider_notice { model_not_applied }`).
- **Politique** (§6) : `set_policy_mode` = `session/set_mode` quand l'agent publie un mode qui correspond
  (`policy.native_mode` exact, sinon les ids conventionnels `plan` / `ask` / `acceptEdits` / `bypassPermissions`…),
  sinon `Unsupported { set_policy_mode }` ; la politique neutre s'applique aussi localement aux demandes de permission
  (`provider_notice { permission_allowed_by_policy | permission_denied_by_policy }`).
- **Refusés à l'ouverture** : `limits.max_*` et `max_turns` (`Unsupported { limits }` ; `turn_timeout_ms` tenu),
  `system_prompt` (`Unsupported { system_prompt }`), `extra_dirs` (`Unsupported { extra_dirs }`).
- **Événements** : `kind` ACP → `category` (`read`→read, `edit`/`delete`/`move`→edit, `execute`→command, `search`→search,
  `fetch`→web, autres→other) ; `canonical` = `mcp__<serveur>__<outil>` quand le titre est `<serveur>_<outil>` d'un serveur
  MCP de la session (nommage d'opencode), sinon `Read` / `Edit` / `Bash` / `Grep` / `WebFetch` par `kind` ; `plan` →
  `provider_notice { plan }` (aucun événement neutre de plan) ; `stopReason` : `end_turn` → `completed`, `max_tokens`,
  `max_turn_requests` → `max_turns`, `refusal` → `done { refusal }` sans `is_error` (comme le natif), `cancelled` →
  `interrupted` ; une erreur JSON-RPC de `session/prompt` → `done { is_error, error }` classé (`-32000` → `auth_required`,
  message `rate limit` → `rate_limited`, `overloaded` → `overloaded`…) ; `error { process_exited }` terminal seulement
  quand le processus meurt.
- **Annulation** : `interrupt` répond `cancelled` à chaque permission en attente puis envoie la notification
  `session/cancel` ; le tour finit quand l'agent répond `cancelled`. Le client n'annonce ni `fs` ni `terminal` : toute
  requête `fs/*` / `terminal/*` de l'agent reçoit `-32601` (method not found).
- **Authentification** : `authMethods` est lu, jamais utilisé (PO n'appelle jamais `authenticate`) ; `session/new` refusé
  par `-32000` → `AuthRequired { login_hint }` (`login_hint` de la configuration, sinon une phrase nommant les méthodes) ;
  `health()` n'essaie `session/new` que si `authMethods` n'est pas vide (il crée puis jette une session vide).
- **Processus** : un agent par session, lancé par `isolated_command` ; `close` tue le processus **et ses descendants**
  (groupe de processus et table des processus). Le transport est une copie adaptée de celui de Codex (non factorisée).
- **Non vérifié** : tout le protocole (aucune session réelle) ; en particulier `usage` du résultat de prompt et
  `usage_update` (instables), les ids de modes, `-32000` comme code d'`auth_required`, les arguments de lancement
  d'opencode / Gemini CLI, `env` / `headers` en tableaux chez un agent réel, le nommage `serveur_outil` d'opencode.


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
| `model_protocol_mismatch` | `harness`, `provider`, `protocol`, `accepts: Vec<String>` | non | 422 |

Règles : `detail` passe par `agent::redact()` avant construction (en-têtes d'autorisation, valeurs
de `Secret` enregistrées, URL avec identifiants) ; **jamais d'identifiant dans un message
d'erreur**, corps HTTP tronqué à 512 octets. `From<SdkError> for ProviderError` vit dans
`providers/claude_code/` : `CliNotFound → cli_not_found`, `Timeout → timeout`,
`ProcessExited → process_exited`, `NotSupported → unsupported`, le reste → `protocol`. La
classification des erreurs Anthropic (`overloaded`, `rate_limited`, « prompt is too long » →
`context_too_small`, 401 → `unauthorized`) est faite par l'adaptateur Claude Code à partir du texte
du résultat quand `is_error`, et non par le backend : elle est portée par **`done.error`** (le tour
se termine par `done`, pas par `error`, pour ne perdre ni usage, ni coût, ni durée). Le backend
décide de réessayer sur `done.error.retryable()` comme il le fait aujourd'hui sur le texte.

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
  authentification y vit). Natif : un répertoire temporaire par serveur MCP. **Codex** :
  `<CODEX_HOME>/home`, créé `0700` à l'ouverture. **ACP** : `AcpConfig::home` (défaut
  `<répertoire de données>/nexus/acp/<instance>/home`, extension de registre `acp_home`), créé `0700`
  avant le lancement ; un humain s'y connecte avec `HOME=<ce répertoire> <agent> login`. Avant le
  05/10/2026, Codex et ACP tournaient avec le `HOME` réel : écart relevé par la suite de sécurité
  (§11.1), pas par un test propre au provider.
- `Debug` d'une configuration d'instance : les NOMS des variables d'environnement s'affichent, jamais
  leurs valeurs (`CodexConfig::env_set`, `AcpConfig::env`) ; même règle que `EnvSpec`.
- Secrets MCP hors argv : la configuration MCP est écrite dans un fichier `0600` d'un répertoire
  `0700`, passé par chemin (`--mcp-config <fichier>`), supprimé à la fermeture de la session.
- Compatibilité : `ClaudeCodeOptions` (ancien chemin, clients directs) garde l'héritage complet
  **par défaut** tant que le backend n'a pas migré (`EnvPolicy::InheritAll`) ; `ClaudeCodeProvider`
  et tous les autres providers utilisent `EnvPolicy::Allowlist` par défaut.

### 11.1 Scénarios de sécurité obligatoires (A32, A33, A35)

> **Décision du 2026-10-07 — un provider tiers se comporte comme Claude Code.** `trust` est un mode
> comme les autres : aucun provider ne le refuse faute de bac à sable (`Capabilities.sandbox` informe
> l'utilisateur, il ne verrouille rien). Un provider qui ne peut pas l'honorer le dit avec son propre
> refus typé. Le scénario `trust_without_sandbox` (A35) est retiré ; la suite de conformité échoue
> désormais si un provider répond `Unsupported { sandbox }` à `trust`.

`testkit::security` — derrière la feature `testkit`, joué par `tests/agent_security.rs` contre CHAQUE
provider livré et contre un provider volontairement fautif. Contrairement à la suite de conformité
(§5), **aucune capacité ne dispense d'un scénario** : une cible qui ne peut pas être montée est un
échec, pas un succès.

| Scénario | Règle |
|---|---|
| `unknown_policy_refused` | un mode inconnu, un motif mal formé ou une politique au-dessus de son plafond sont refusés, jamais lus comme « autoriser » |
| `isolation_env` | aucune variable de l'hôte n'atteint l'enfant ; un tiers a un `HOME` qui n'est pas celui de l'hôte |
| `argv_sans_secret` | la valeur secrète n'est nulle part sur la ligne de commande |
| `erreur_sans_identifiant` | la valeur secrète n'est dans aucun `Debug`, aucune erreur (`Display`, `Debug`, JSON), aucun événement |
| `outil_hors_profil_refuse` | le harnais natif n'exécute jamais un outil hors du profil de la session (compteur du serveur MCP factice à 0) ; seul scénario qui peut valoir « sans objet », pour un provider qui n'exécute aucun outil lui-même |

**Rouge d'abord.** `every_scenario_is_red_against_the_faulty_provider` exige une violation de
chaque scénario contre un provider qui hérite l'environnement, écrit le secret sur argv, le laisse
dans un `Debug` et dans une erreur, et ignore un plafond ;
`each_scenario_names_the_fault_it_found` exige que chacun soit rouge pour SA raison (un premier jet
passait pour une mauvaise raison : le provider fautif refusait les serveurs MCP et ne s'ouvrait
jamais). Les constructeurs `ProviderError::invalid/protocol/unreachable` masquent déjà les valeurs de
forme clé (A10) : la fuite d'erreur se simule en construisant la variante.

**Ce que cela ne prouve pas** : rien n'a parlé à un vrai Codex, un vrai agent ACP ou un vrai
endpoint de modèle ; chaque provider est observé par son faux exécutable, qui enregistre ce que
l'enfant a reçu.

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
- Écarts constatés à l'implémentation (`claude-code-sdk-rs/src/model/`, feature `provider-native`) :
  `CompletionChunk` est sérialisé en `{"type","data"}` (variantes tuple) ; l'ordre émis est texte et
  raisonnement, puis `tool_call`, puis `usage`, `finish` en dernier ; `Usage.input_tokens` compte les
  jetons **hors cache** (le cache lu est dans `cache_read_tokens`) pour que `PriceTable` multiplie sans
  double compte ; `PriceTable::cost(model, usage, basis) -> Cost` (`usd: None` sans prix) ;
  `EndpointGuard` ajoute `allow_private_network` (défaut `false`, absent d'A36 : un vLLM sur LAN en a besoin ;
  n'ouvre pas le lien-local ni `http` hors boucle locale) ; aucune connexion ne passe par un proxy (il
  résoudrait lui-même le nom) ; un 404 est `invalid_request` (pas de variante « modèle absent ») ;
  `EndpointProbe.parallel_tools` reprend le drapeau déclaré (`explicit_parallel_tool_calls`), il n'est pas
  mesuré ; `CompletionRequest.stream = false` est réservé (le fil est toujours en flux).

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

Écarts et précisions constatés à l'implémentation (`claude-code-sdk-rs/src/agent/registry.rs`) :

- `ProviderInstanceConfig` est `#[non_exhaustive]` (constructeurs `new`, `claude_code`, `native`, `with_*`),
  désérialisé avec champs inconnus refusés (une clé collée sous `api_key` échoue au lieu d'être jetée) ;
  `from_json` renvoie `invalid_request` à message fixe (mauvaise référence d'identifiant, kind inconnu).
- `SecurityGate` ne se construit que par `SecurityGate::attest(&'static str /* lot */)`. `attest` est une
  **simple déclaration de l'hôte** : rien dans nexus ne vérifie que le lot de sécurité du backend existe ni
  qu'il est actif ; la sûreté repose sur l'hôte. L'activation est
  irréversible. La porte garde aussi `test_connection` ; `list()` n'est jamais gardée.
- `remove(id)` rend `false` pour `claude-code` (refus) et pour un id inconnu. `claude-code` peut être
  reconfigurée (même kind), pas remplacée par un autre kind.
- Ajouts : `register_kind_factory(kind, factory)` (point d'extension des kinds codex/acp, ou rejeu en test),
  `test_connection(&config) -> ProviderHealth` (A30 : construit une instance éphémère, `health()`, puis sonde
  d'outil du modèle par défaut pour un native ; n'enregistre rien), `refresh_capabilities(id, model)`,
  `capabilities_for(id, model)`, `config(id)`, `set_transcript_store` (feature `provider-native`).
- `price_table()` rend un `PriceBook` (une `PriceTable` par instance) : le coût d'un modèle est celui des prix de
  SON instance. Chaque `PriceTable` reste la seule à calculer un coût.
- `resolve_alias` : alias connu → cible ; sinon le nom passe tel quel s'il est connu de la configuration (cible
  d'alias, `default_model`, modèle tarifé) ou si l'instance ne déclare aucun alias ; sinon `invalid_request`.
  Le catalogue (asynchrone) n'est pas consulté.
- `quirks` = préréglage ∪ surcharge : booléens en OU, `explicit_parallel_tool_calls` et `reasoning_field` pris
  de la surcharge quand elle les fixe. Une surcharge ne retire pas un drapeau du préréglage.
- Clés d'`extensions` lues : `allow_private_network` (bool), `cli_path` (chaîne), `compaction_keep_recent`
  (entier), `codex_home` (chaîne), `env` (objet de chaînes, acp), `thinking` (bool, acp), `login_hint` (chaîne, acp) ; une clé dont le nom évoque un identifiant est refusée.
- Kinds : `claude_code`, `native`, `codex` et `acp` construits ; `scripted` sans constructeur répond
  `Unsupported { provider_scripted }` ; sans la feature `provider-native`, `native` répond
  `Unsupported { provider_native }`, sans `provider-codex`, `codex` répond `Unsupported { provider_codex }`, sans `provider-acp`, `acp` répond `Unsupported { provider_acp }`.
  Une instance `codex` se décrit par `command` (le programme seul : des arguments en plus sont refusés,
  `invalid_request`, ils sont ceux de l'adaptateur), `credential` (résolu à chaque `open`, posé dans
  `CODEX_API_KEY` du seul processus), `cost_source` (`unknown`, `free`, `priced` ; `reported` est lu comme
  `unknown`), `prices`, `context_window`, `default_model`, `env_inherit` et l'extension `codex_home`. La porte
  A32 garde `codex` comme `native` : `upsert` répond `Unsupported { security_gate }` tant que le lot sécurité
  n'est pas actif.
  Une instance `acp` se décrit par `command` (programme + arguments, sans secret : un argument qui ressemble à un secret
  est refusé), `cost_source` (`unknown`, `free`, `priced`), `prices`, `context_window`, `default_model`, `env_inherit` et
  les extensions `env` (objet de variables non secrètes ; un nom évoquant un identifiant est refusé), `thinking` (bool),
  `login_hint` (chaîne) ; la porte A32 la garde comme `codex`.

### 13.1 Harnais et fournisseur de modèle (N16, décision `130fa134`)

Un **harnais** exécute la session (Claude Code, Codex, un agent ACP, la boucle native) ; un
**fournisseur de modèle** sert le modèle (Anthropic, OpenAI, DeepSeek, Ollama, vLLM…). Avant N16 les
deux étaient fusionnés dans une instance. Ils se rencontrent par un **protocole** : le format de fil
qu'un harnais parle à son modèle.

| Harnais | Protocole consommé (`ProviderKind::model_protocols`) | Lien appliqué par `open_session` |
|---|---|---|
| `claude_code` | `anthropic_messages` | oui : `ANTHROPIC_BASE_URL` et la clé en variables explicites, l'autre variable d'authentification posée **vide** (§13.2) |
| `native` | `openai_chat` | oui : provider composé sur l'endpoint du fournisseur de modèle ; identifiant résolu **pour le fournisseur de modèle** |
| `codex` | `openai_responses` | non : `Unsupported { model_binding }` — la configuration de fournisseur personnalisé de Codex n'est pas vérifiée contre un vrai `app-server` |
| `acp`, `scripted` | l'agent choisit son modèle | refusé : `ModelProtocolMismatch` avec `accepts` vide |

- `SessionSpec.model_binding: Option<ModelBinding { provider, model }>` ; absent, `open_session` est
  `get(harnais).open(spec)` **inchangé**. Un provider qui reçoit un lien directement (hors registre)
  le refuse (`Unsupported { model_binding }`) plutôt que de l'ignorer.
- `ProviderRegistry::{upsert_model_provider, remove_model_provider, model_provider,
  list_model_providers, open_session}`. `anthropic` est intégré (API du fournisseur, première
  partie) : il ne s'enlève pas et ne se redéfinit pas.
- **Paire vérifiée avant tout lancement** : un protocole que le harnais ne consomme pas est
  `ProviderError::ModelProtocolMismatch { harness, provider, protocol, accepts }` (HTTP 422, `kind`
  `model_protocol_mismatch`), qui nomme les deux côtés.
- **Modèle d'une session** : `spec.model` d'abord, puis `binding.model`, puis le défaut du
  fournisseur de modèle (alias résolus).
- **Identifiants et consentement suivent le fournisseur de modèle** : le `CredentialResolver` est
  appelé avec l'id du fournisseur de modèle, jamais celui du harnais (grant `Provider(<id du
  fournisseur de modèle>)`, A26). Le consentement par origine (A28) suit l'URL du fournisseur de
  modèle : c'est au backend de le lier à cet id.
- **Porte (A32)** : un fournisseur de modèle tiers est refusé tant que la porte de sécurité est
  fermée (`Unsupported { security_gate }`) ; `anthropic` n'en a pas besoin.
- **Lecture sans migration** : `ProviderInstanceConfig::model_provider()` lit le côté modèle d'une
  instance existante (mêmes champs, vus comme un couple) ; `None` pour `acp` et `scripted`.
- Un fournisseur de modèle ne porte aucun secret : `credential` est une référence, et un champ
  inconnu (un `api_key` collé) est refusé.

### 13.2 Mesure sur le vrai CLI Claude Code (2026-10-05, version 2.1.287)

Pourquoi l'autre variable d'authentification est posée vide. `claude -p` lancé contre un serveur
HTTP local qui capture les en-têtes (les valeurs ne sont comparées qu'à celles du test, jamais
affichées) :

| Cas | Variables | En-têtes reçus |
|---|---|---|
| A | `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` | `Authorization: Bearer <jeton>` seul |
| B | `ANTHROPIC_BASE_URL` + `ANTHROPIC_API_KEY` | `x-api-key` seul |
| C | jeton **et** une clé d'hôte présente | **les deux** : la clé de l'hôte part vers l'URL de base |
| D | jeton et `ANTHROPIC_API_KEY=""` | `Authorization` seul : la valeur vide supprime l'en-tête |

Le cas C est le risque : un Claude Code lié à une passerelle tierce enverrait la clé Anthropic de
l'hôte à cette passerelle, parce que la politique d'environnement de Claude Code hérite les
variables `ANTHROPIC_*`. Le cas D est le remède, appliqué par `open_session`. Rejouable :
`cargo test --test real_claude_gateway -- --ignored` (nécessite `claude` installé ; aucune requête ne
quitte la machine, l'URL de base est locale). **Non prouvé** : aucune passerelle réelle, aucun
Bedrock ni Vertex.

### 13.3 Outils livrés avec nexus (N24)

Une instance native qui le demande a des outils sans que la session ait à les configurer :
`extensions.nexus_tools` = `true` (cherche `nexus-tools` à côté du programme puis dans le `PATH`) ou
`{"program": "...", "args": [...], "env": {...}}`. Le harnais lance alors `nexus-tools` comme **son enfant**
(MCP sur stdio, `--trust-harness` : le harnais est l'unique client et applique la politique avant tout appel, il
n'y a pas de profil signé à fabriquer) sous le nom de serveur `nexus`, avec pour périmètre le répertoire de la
session et ses `extra_dirs`. Une session qui nomme déjà son propre serveur `nexus` garde le sien. L'attache n'a
lieu que pour un modèle capable d'appeler des outils ; sinon `provider_notice { default_tools_skipped }` et la
session reste un simple échange de texte.

**Noms canoniques.** Les outils de ce serveur (`Read`, `Write`, `Edit`, `NotebookEdit`, `Glob`, `Grep`, `Bash`,
`Monitor`, `TaskStop`, `WebFetch`, `WebSearch`) portent, en plus de `mcp__nexus__<outil>`, le nom que Claude Code
leur donne. Un motif écrit `Read(.env*)` ou `Bash(git *)` s'applique ; l'exposition suit aussi (`allow: ["Read"]`
n'offre que `Read`, `deny: ["Bash"]` retire `Bash`). Un outil d'un AUTRE serveur qui s'appellerait `Read` n'hérite
de rien : le nom canonique n'est donné qu'au serveur `nexus`. `tool_call` et `permission_ask` portent le nom
canonique et la vraie catégorie (lecture, édition, recherche, commande, web).

**Ce à quoi un motif est comparé** (`providers/native/policy_args.rs`) : jamais le JSON entier.
- *Commande* (`Bash`, `Monitor`) : la ligne est coupée en commandes simples (`;`, `&`, `|`, retour à la ligne,
  hors guillemets, sous-shells et groupes aussi). Un `deny` qui reconnaît la ligne ou **une** partie refuse ; un
  `allow` exige **chaque** partie permise. Une substitution (`$(…)`, accents graves, `<(…)`, guillemet non
  fermé) n'est jamais autorisée par un motif à argument (elle tombe sur le mode) et les commandes qu'elle contient
  sont soumises aux `deny`. `Bash(git *)` ne couvre donc pas `git status && rm -rf ~`.
- *Chemin* (`Read`, `Write`, `Edit`, `NotebookEdit`) : sous sa forme normale (relatif au répertoire de la
  session, `.` et `..` résolus). Un `deny` essaie aussi le chemin tel qu'écrit et son dernier composant (un motif
  sans `/` vaut « n'importe où ») ; un `allow` n'a que la forme normale. `./.env`, `src/../.env` et le chemin
  absolu sont refusés par `Read(.env*)`, et `src/../.env` n'est pas couvert par `Edit(src/*)`.
- *Web* : `domain:<hôte>` pour `WebFetch` ; la requête pour `WebSearch`. Les deux **demandent** par défaut
  (catégorie web) ; `auto_edits` laisse passer les éditions, pas les commandes ni le web.
- Un champ absent ou illisible n'est jamais autorisé par un motif à argument ; un `deny` à argument le reconnaît
  (échec fermé, A35).

Limites dites : la comparaison est lexicale (un lien symbolique innocent vers `.env` est jugé sur son nom ; ce qui
borne les outils de fichiers est le périmètre de `nexus-tools`, qui résout les liens) ; un shell lit tout ce que son
utilisateur système lit : la politique décide de ce qui est *demandé*, ce n'est pas un bac à sable (capacité
`sandbox: none`, information et non barrière : `trust` s'ouvre).

**Fermeture.** `close` d'une session native ferme l'entrée du serveur et lui laisse 2,5 s pour partir avant de le
tuer : `nexus-tools` arrête alors les commandes de la session et leurs descendants. Un `SIGKILL` du serveur ne
leur laisse pas cette chance (limite).

**Un serveur par session, borné au lancement (N27).** Le harnais ne partage jamais un `nexus-tools` entre
sessions : chaque session native a **son** processus, et ce processus est borné par construction, en défense en
profondeur derrière la politique du harnais.
- *stdio seulement* : `--trust-harness` vaut pour un client unique sur un tube privé ; combiné à `--listen`
  (HTTP), le programme refuse de démarrer (code 2, « --trust-harness is for one harness over a private pipe; it
  cannot be combined with --listen »).
- *portée fixée au lancement* : `--cwd` et `--add-dir` (le répertoire de la session et ses `extra_dirs`) sont tout
  le périmètre des outils de fichiers de ce processus ; deux sessions de répertoires différents ne lisent pas les
  fichiers l'une de l'autre (ce que borne `Read`/`Write`/`Edit`, pas ce qu'un shell lit).
- *`--tools`* : `--tools Read,Grep,…` (noms canoniques, casse comprise) est la liste des seuls outils du
  processus : `tools/list` n'en montre pas d'autre et un `tools/call` d'un autre outil est refusé (`unknown tool`,
  erreur MCP `-32602`) même demandé directement. Un nom inconnu est une erreur de démarrage (code 2) qui liste les
  noms valides ; un nom canonique que la plate-forme n'a pas (`Bash` sous Windows) est accepté et simplement absent.
  Répété, chaque `--tools` restreint le précédent ; avec un profil signé ou `--unrestricted`, c'est une
  intersection, jamais plus large que le profil. Le harnais passe l'ensemble que la politique de la session
  **peut exposer** (`nexus_tools_bound` : règle d'exposition appliquée aux onze outils, avec le mode le plus haut
  que la session peut atteindre par `set_policy_mode` sous son plafond — `plan_only` ne borne le processus aux
  lectures que si le plafond est `plan_only`) ; un `--tools` dans `extensions.nexus_tools.args` restreint encore.
- *jetons signés réservés au HTTP* (et au stdio d'un client qui n'est pas le harnais : `NEXUS_TOOLS_PROFILE`) :
  `--listen` vérifie un jeton à chaque requête, `--trust-harness` n'en demande aucun.

Coût mesuré d'un processus par session (binaire `release`, macOS, M4 Max chargé) : environ 4,2 Mo de RSS au repos
après `initialize` ; démarrage jusqu'à la réponse `initialize` en moyenne 7 à 40 ms sur 20 lancements (médiane
3 à 10 ms). Le **premier** lancement du binaire depuis un processus neuf paie environ 300 ms côté système (même
`--version`, qui ne démarre rien). Rejouable : `cargo test -p nexus-tools --release --test session_bound --
--ignored --nocapture`.

**`WebSearch`** n'existe que si le serveur a au moins un moteur configuré (`args: ["--search-engine", …]`).

**Navigateur optionnel (N23).** `extensions.browser` = `true` (cherche `obscura` dans le `PATH`) ou
`{"program": "...", "args": [...]}` : Obscura est **attaché** comme serveur MCP externe `browser` (`obscura mcp`),
jamais compilé chez nous (V8 en C++, hors règle « Rust seul » ; V8 hors de notre processus). Configurer
l'instance vaut autorisation. Exécutable absent : aucun outil `browser_*`, `provider_notice { browser_unavailable }`,
et un appel direct est refusé (`unknown tool`). Jamais `--stealth` ni `--allow-private-network` (refusés à la
configuration), `OBSCURA_ALLOW_PRIVATE_NETWORK=0` posé et non modifiable. Les outils, relevés dans la documentation
d'Obscura (non annotés par le serveur), sont classés par leur nom : lecture (`browser_snapshot`, `_markdown`,
`_links`, `_extract`, `_get_cookies`…) en lecture seule, sans approbation et offerts en `plan_only` ; navigation
(`browser_navigate`, `_back`, `_tab_new`…) en catégorie web, qui demande ; tout le reste (clic, saisie,
`browser_evaluate`, écriture de cookies) demande, et un `browser_*` inconnu est traité en interaction. Une
destination de `browser_navigate`/`browser_tab_new` est jugée **avant** toute demande : schéma autre que http(s),
nom local (`localhost`, `.local`, `.internal`, sans point), adresse IP littérale hors des plages publiques
(boucle locale comprise). La protection SSRF d'Obscura reste la seconde ligne. Non établi : la liste d'outils vient de
la documentation, aucun Obscura n'est installé ici ; le test réel (`#[ignore]`, `NEXUS_REAL_OBSCURA`) n'a pas été joué.

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

- Historique : v1 (e15cd6e, 81ad207) ; **v2** = v1 + champ optionnel `done.error` (ajout compatible :
  un pair v1 qui l'ignore reste correct) ; **v3** = v2 + variante `ProviderError::ModelProtocolMismatch`
  (kind `model_protocol_mismatch`, N16 ; ajout compatible : un pair v2 qui ne la connaît pas la traite
  comme une erreur inconnue). Le fichier `tests/snapshots/agent_contract_v2.json` est conservé : le
  backend et le frontend, tant qu'ils sont en v2, le copient comme fixture.
  L'instantané v2 porte aussi `provider_instance_config` (natif avec préréglage, prix et extension ; ACP ; Claude Code) : une entrée d'instantané ajoutée pour une
  forme que le registre sérialisait déjà, **sans changement de forme ni de `CONTRACT_VERSION`**.
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
| `provider-native` | EXISTE : client HTTP + SSE de `model/` (`reqwest` : `guard`, `sse`, `wire`, `openai`), `providers/native/` ; les types, `quirks` et `pricing` de `model/` sont toujours compilés (le registre les porte) | non |
| `provider-codex` | EXISTE : `providers/codex/` (`CodexProvider` sur `codex app-server`, surface stable ; aucune dépendance de plus, le processus passe par `transport::spawn`) ; `src/bin/fake_codex.rs` ; schéma versionné et transcriptions dans `tests/transcripts/codex/<version>/` | non |
| `provider-acp` | EXISTE : `providers/acp/` (`AcpProvider` client générique de l'Agent Client Protocol, JSON-RPC stdio ; aucune dépendance de plus, le processus passe par `transport::spawn`) ; `src/bin/fake_acp.rs` ; schéma versionné et transcriptions dans `tests/transcripts/acp/<version du protocole>/` | non |

- Côté backend : `nexus-claude = { …, features = ["memory", "auto-download", "provider-native",
  "provider-codex", "provider-acp"] }` et `features = ["testkit"]` en `dev-dependencies`. Pendant
  l'intégration : surcharge locale non commitée `[patch]` vers le worktree nexus (B3) ; l'épingle
  `rev` n'est montée qu'à la fin, sur un sha poussé de `integration/harness-multi-provider`.
- Faux exécutables (`fake_claude`, `fake_openai`, `fake_codex` (existe), `fake_acp` (existe)) : binaires du crate,
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
- Exemple de `done` avec cause classée (v2, `done.error` : objet `ProviderError` avec `kind` ;
  absent quand `is_error` est faux ou que la cause n'est pas classée ; instantané :
  `tests/snapshots/agent_contract_v2.json`, entrée `done_with_error`) :
  ```json
  {"type":"done","stop_reason":"error","is_error":true,"result_text":"Overloaded",
   "error":{"kind":"overloaded"},"usage":{"input_tokens":12,"output_tokens":0,"by_model":[]},
   "cost":{"usd":null,"basis":"unknown"},"duration_ms":812,"num_turns":1}
  ```
