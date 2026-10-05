# Harness multi-provider — consolidation de la relecture (2026-10-05)

Ce document fait autorité sur les trois plans (Nexus `7487dba7-cbbd-43cb-b898-3f11d76092fb`, Backend `4b764ed6-f5e7-478d-b19c-8928a11a39f5`, Frontend `c3b8001c-93c4-4da3-b47f-5f604de610b9`), la RFC `b53e11c7` et la note `1babfff3` : en cas de contradiction, c'est lui qui gagne. Détail et preuves (fichier:ligne) dans les huit rapports voisins `review-1-contrat.md` … `review-8-executabilite.md` (même dossier). Les identifiants B20–B25 / N14 / F12–F13 proposés par les rapports se CHEVAUCHENT entre rapports : utiliser les noms de ce document.

Mandat de l'utilisateur : « complètement en place demain sur cet environnement », ressources libres, décisions produit tranchées ici (choix prudent), révisables.

## A. Décisions tranchées

### Contrat (nexus `agent/`)
- A1. Un seul `ProviderRegistry` et une seule table de prix, dans nexus. Le backend n'a qu'un RÉSOLVEUR (quel provider/modèle pour cette demande) et aucun `pricing.rs`.
- A2. Deux traits : `AgentProvider`/`AgentSession` (harness) et `ModelEndpoint` (API brute). Harness natif = AgentProvider sur ModelEndpoint.
- A3. `ResumeToken` opaque, sérialisable, persisté sur la session. Conformité dans un module publié `testkit/` (pas seulement `tests/`).
- A4. `Capabilities` est PAR MODÈLE (`provider.capabilities(model)`), instantané figé sur la session à l'ouverture. Champs : `interactive_permissions`, `permission_scopes`, `sandbox`, `secret_isolation`, `per_session_mcp`, `hooks {InProtocol|Command|None}`, `subagents {Nested|SeparateThread|None}`, `compaction_signal`, `thinking`, `images`, `tools`, `context_window {value, source}`, `set_model_live`, `native_question`, `tool_cancel`, `background_tasks`, `resume`, `cost {Reported|Priced|Free|Subscription|Unknown}`. Chaque capacité absente a un repli écrit et un scénario de conformité ; jamais de succès silencieux.
- A5. Aucune information Claude Code affichée aujourd'hui ne passe par `ProviderNotice(Value)`. `Done` porte usage détaillé, `cost {usd, basis}`, durée, nombre de tours, sous-type, `is_error`, texte de résultat, modèle effectif. `SessionStarted` porte le mode de permission ; `Compaction` porte `trigger` ; tâches de fond = variante typée `BackgroundTasks`. `init → SessionStarted` (pas ProviderNotice).
- A6. Annulation : `interrupt(scope)` + `cancel_tools(CancelScope::{All, Task(id)})` qui PRÉSERVE le tour. Le suivi par PID descend dans l'adaptateur Claude Code ; les champs PID du fil restent optionnels (compat).
- A7. Hooks : trait neutre `{before_tool, after_tool, before_compaction}` dans `SessionSpec` ; repli côté backend quand la capacité est `None` (injection par tour / observation de ToolResult). Pas d'exécutable relais pour les hooks Codex en v1.
- A8. `ToolPolicy { mode: Ask|AutoEdits|PlanOnly|Trust, allow, deny }` avec `restrict(&parent)` monotone ; `ToolPattern` neutre (`outil` + glob d'argument optionnel), syntaxe Claude acceptée en entrée. `ToolCall`/`PermissionAsk` portent une catégorie (`command|read|edit|search|web|mcp|agent|other`) et un alias canonique fournis par l'adaptateur. Six modes Claude mappés (`acceptEdits, auto, bypassPermissions, manual, dontAsk, plan`, + `default` hérité).
- A9. `SessionSpec` complet : cwd, modèle, system_prompt, policy, mcp_servers, extra_dirs, max_turns, env (liste blanche + ajouts explicites), hooks, `limits`, `lineage`, extensions par provider.
- A10. `ProviderError` typé : `CliNotFound, AuthRequired{login_hint}, CredentialsLocked, Unauthorized, EndpointUnreachable, ModelNoTools, ContextTooSmall, RateLimited{retry_after}, Overloaded, Timeout, ProcessExited, Protocol, Unsupported{capability}`, avec `retryable()`. Jamais d'identifiant dans un message d'erreur.
- A11. Identifiants : trait `CredentialResolver` (rappel fourni par le backend), type `Secret` sans `Debug`/`Display`. Références `vault:<nom>`, `env:<VAR>`, `none`.
- A12. `CONTRACT_VERSION` + `#[non_exhaustive]` sur les énumérations publiques. Images : hors v1 (capacité `images=false` partout côté backend).
- A13. Concurrence : un seul tour à la fois (`send_turn` pendant un tour → erreur typée) ; les permissions hors tour sortent par `out_of_band()`.
- A14. N5 est ADDITIF : `clone_stdin_sender`, `take_sdk_control_receiver`, `MockTransport`, `nexus_claude::memory` restent publics jusqu'à ce que le backend n'en dépende plus.

### Topologie « master ou non »
- A15. Pas d'objet « master » : deux rôles, `pilot` (session ouverte par un humain) et `executor` (runner, délégation, protocoles, one-shot). Configurables en global (`chat.roles`) et par projet (`llm_roles`). Absents = provider unique, comportement actuel.
- A16. Résolution provider+modèle : session existante (figée, 409 si conflit) > requête explicite > tâche (`Task.model_alias`) > persona (`model_preference`) > run (`RunPlanRequest.provider/model`) > règle projet > règle globale > défaut configuré > `claude-code` s'il est sain > `no_provider`. Le niveau retenu est persisté dans `routed_by`. Une reprise ne re-résout jamais.
- A17. Enveloppe d'une session enfant : parent lu dans le jeton signé (jamais dans le corps), projet et cwd du parent, politique `restrict` du parent (jamais plus permissive), profondeur 1, 4 enfants vivants, budget = reste du parent, annulation en cascade, coût des descendants compté. `SpawnedBy::Delegation`.
- A18. Pilote indisponible : erreur explicite en chat ; exécutant : niveau suivant s'il passe les filtres, consigné, sinon `Failed`. `Capabilities.subagents = None` → repli par session enfant PO.

### Spécialisation modèle ↔ tâche et coût
- A19. Déclaratif en v1, pas d'apprentissage : `model_aliases` (alias → instance + modèle ; `fast`, `default`, `deep`, `utility`) et `model_policy { mode: off|shadow|enforce, rules: rôle → alias, fallback: [...], caps }`. Rôles de règle : `chat`, `runner.simple|complex|creative`, `runner.retry`, `utility.feature_graph`, `utility.compaction`. Livré en `off`. `shadow` enregistre `shadow_model` sans l'appliquer.
- A20. Repli : filtres durs (outils, fenêtre ≥ schémas MCP, images, endpoint autorisé) puis chaîne `fallback` déclarée, raison typée. Pas de substitution d'un choix explicite ; jamais vers un endpoint non autorisé ; chaîne épuisée = échec avant ouverture.
- A21. Coût : `cost_source` par instance (`reported|priced|free|subscription`) ; un coût rapporté pour un modèle sans prix connu vaut `None`, pas 0 ni fictif. Deux compteurs (marginal réel / notionnel) ; seul le marginal coupe un run. Plafond par tâche en `warn`. Sans prix : budget en tokens, sinon refus du run budgété.
- A22. Enregistrement par exécution : provider, modèle demandé et effectif, alias, `routed_by`, `route_rule`, `fallback_reason`, `shadow_model`, classe de tâche (champ propre), tentative, base_sha, tokens, coût + base, détail des contrôles. Corriger `update_agent_execution` en retry. Brancher les champs persona morts (`model_preference`, `max_cost_usd`, `timeout_secs`).
- A23. Reporté hors v1 : bandit, tirage 50/50, escalade en cours de tâche, routage par étape.

### Première connexion, instances, identifiants
- A24. Instances de provider persistées dans Neo4j, CRUD par API + UI, rechargement à chaud ; `claude-code` est une instance intégrée. Aucun secret accepté dans ces corps : seulement une référence.
- A25. Routes de mutation (instances, consentement, rôles, politique) : jeton HUMAIN seulement ; un jeton `agent_session` reçoit 403. Pas de système de rôles administrateur en v1 (reporté) ; propriété = serveur.
- A26. Lecture d'une clé : `read_for_provider` sous grant `Provider(instance)`, valeur enregistrée au masqueur ; vault verrouillé → `CredentialsLocked`, SANS repli vers un autre provider.
- A27. Login des CLI (Claude, Codex) : PO ne le pilote pas ; `health()` rend l'état et la commande à lancer, l'UI a « revérifier ». PO n'installe que Claude Code (existant).
- A28. Consentement par projet pour tout endpoint, local compris ; lié à l'ORIGINE de l'URL (invalidé si elle change), donné par un humain, projet dérivé du cwd/parent. Session sans projet : `claude-code` seulement.
- A29. Erreurs d'ouverture typées : `ProviderError` → statut HTTP + `code` ; le frontend a un `catch` et une carte d'erreur par code ; état vide « aucun provider ».
- A30. Préflight commun chat + runner : modèle avec outils, fenêtre suffisante, sonde d'appel d'outil mise en cache par (instance, modèle). Test de connexion possible AVANT enregistrement.
- A31. Assistant desktop : étape chat passable ; `generate_config` préserve les sections inconnues.

### Sécurité (lot placé AVANT tout provider tiers)
- A32. PORTE : le registre refuse toute instance non-Claude tant que le lot sécurité n'est pas actif (test `registry_refuses_third_party_instance_without_security_gate`).
- A33. Environnement du processus : `env_clear` + liste blanche PAR DÉFAUT pour tout provider (un lanceur unique dans nexus ; `Command::new` interdit dans `providers/`). `HOME` dédié hors Claude. Secrets MCP hors argv (fichier 0600 supprimé à la fermeture).
- A34. Jeton de session lié (session, plafond de politique, profil d'outils signés), `jti` révoqué à la fermeture, 403 sur admin/consentement/instances. Le profil MCP est lu du jeton, pas d'un en-tête.
- A35. Mode `Trust` pour un modèle tiers : refusé sans `Capabilities.sandbox` (v1). Harness natif = outils MCP seulement, profil restreint par défaut (sans `delegate_task`, `plan run`, `chat send_message`). Runner sur provider tiers : `Ask`/profil restreint, pas de bypass.
- A36. Garde d'endpoint : https hors boucle locale, plages internes refusées après résolution DNS, redirections coupées. Erreurs expurgées, masquage en échec FERMÉ.
- A37. Journal d'envoi (qui, quel projet, quelle origine) ; échec d'écriture = refus d'ouverture pour un tiers.

### Providers
- A38. v1 : Claude Code (façade), harness natif sur OpenAI-compatible (préréglages deepseek, vllm, ollama, llama_server, nim), Codex `app-server` (surface stable, sans `experimentalApi`), et un `AcpProvider` générique stdio (opencode par `opencode acp` ; Gemini CLI par configuration) à la place d'un adaptateur opencode HTTP dédié. Pas de port local en v1.
- A39. Quirks par drapeaux d'instance + sonde : DeepSeek exige le renvoi de `reasoning_content` quand `tools` est présent (transcript et compaction le conservent) ; `tool_choice` absent (Ollama) ; `parallel_tool_calls` explicite ; champ `reasoning` (vLLM) ; appels d'outils entiers ou fragmentés → même ToolCall ; message système tardif replié dans `system`.
- A40. Codex : version minimale vérifiée par `health()`, schéma versionné, approbations MCP par élicitation, `CODEX_HOME` persistant PAR INSTANCE, coût `None` (tokens seuls), outils PO par MCP. La version installée ici (0.38.0) n'a pas `app-server` : livrer contre faux exécutable + schéma, l'essai réel attend une mise à jour.
- A41. Essai réel de bout en bout visé : llama-server local (déjà essayé le 03/10) par le harness natif — sans clé.

### Contrat front↔back
- A42. `GET /api/chat/providers` (instances, santé, capacités par modèle, alias, défaut, autorisé pour le projet) : les capacités sont connues AVANT la session. `system_init` porte `provider`, `capabilities`, `tool_policy`. DTO de liste des sessions : `provider_id`, modèle. `CreateSessionRequest.provider`.
- A43. Le backend accepte les modes neutres EN PLUS des chaînes héritées ; émet les deux formes.
- A44. Test de contrat au niveau des CHAMPS avec trames d'exemple partagées (fixtures JSON générées par le backend, copiées dans le frontend avec somme de contrôle).
- A45. `session_closed` : émis par `close_session`. Question sans support natif : événement `ask_user_question` synthétique, réponse par tour utilisateur.

## B. Méthode d'exécution

- B1. Pas de `plan(run)` ni `delegate_task` (le moteur ignore les dépendances inter-plans et fait une PR par plan). Pilotage tâche par tâche, statuts tenus à la main dans le PO.
- B2. Une branche `integration/harness-multi-provider` par dépôt, dans un worktree créé depuis `origin/main` sous `/Users/triviere/projects/project-orchestrator/_wt/harness-<dépôt>`. Ne JAMAIS toucher aux checkouts principaux (travail en cours de l'utilisateur), ni aux autres branches, ni à `main`. Un commit par tranche (message `feat(harness): <id tâche> …`), pour pouvoir redécouper en PR. Pas de fusion ; pas de `push --force`.
- B3. Backend ↔ nexus pendant l'intégration : surcharge locale NON commitée (`.cargo/config.toml` du worktree backend, `[patch]` vers le worktree nexus). L'épingle `Cargo.toml` n'est montée qu'à la fin, sur un sha poussé.
- B4. Chaque tranche : test rouge sans le correctif quand c'est un correctif, tests du périmètre verts, diagramme `docs/diagrams/*.mmd` + `INDEX.yml` mis à jour dans le même commit (nexus : `cargo xtask diagrams`), pas de Python ajouté dans nexus.
- B5. Ne pas toucher aux tâches du plan « Documentation vivante » `22f58265` ; si une tâche le recouvre (B19↔`1d0a95de`, F1/B8↔`c2139265`, B2↔`e6919f99`, B4–B6↔`756b38ac`), faire le travail sur la branche d'intégration et le noter au journal.
- B6. Ne redémarrer aucun service de l'environnement, ne modifier aucune configuration vivante : la mise en service est faite par l'orchestrateur principal après revue.
- B7. Secrets : jamais affichés ; besoin d'une clé → le noter au journal comme blocage, continuer contre faux exécutable.

## C. Ordre global (paliers)

P0 — correctifs de failles existantes (backend, indépendants) : enveloppe de délégation (A17), jeton de session lié (A34), enveloppe monotone sur `chat send_message`/`delegate_task`.
P1 — nexus : spécification gelée `docs/agent-contract.md` (A1–A14) → `agent/` types → `env_clear`+liste blanche dans le transport → testkit + FakeProvider scriptable + rejeu → façade `ClaudeCodeProvider` (aucun changement de comportement) → contrôle/annulation/tâches de fond typées → version du contrat.
P2 — backend : surcharge nexus → sécurité env/MCP/consentement → contrat de fil (fixtures) → résolveur + instances CRUD + identifiants → provider persisté → `agent_event_to_chat_event` → `ChatManager` sur `AgentSession` DERRIÈRE `CHAT_PROVIDER_PATH=legacy|agent` (ancien chemin conservé ; retrait = tâche séparée, hors v1 si le temps manque) → erreurs typées → préflight.
P3 — nexus : `ModelEndpoint` OpenAI-compatible + quirks + sonde → harness natif → compaction/transcript → registre + prix + identifiants. Backend : porte sécurité, garde d'endpoint, journal, politique de modèle + rôles, arbre (cascade, coût), runner (provider/modèle par run et par tâche, enregistrement A22).
P4 — frontend (démarre dès P1 sur le contrat écrit) : types provider/capacités, contrat par champs, politique neutre, registre de rendu (provider, outil), création de session avec provider+modèle, badges, erreurs/états vides, dégradation par capacité, coût par base, réglages (instances, rôles, alias, politique, consentement), run avec provider, arbre multi-provider, desktop.
P5 — Codex (faux exécutable + schéma), `AcpProvider` (faux + opencode si installé), essai réel llama-server, documentation, diagrammes cibles passés en ✅.

Critère de « fini » v1 : avec `CHAT_PROVIDER_PATH=agent`, une session Claude Code se comporte comme aujourd'hui (tests existants verts) ; une instance OpenAI-compatible locale créée depuis l'UI, consentie pour un projet, ouvre une session par le harness natif, appelle un outil MCP PO, affiche son coût par base ; les rôles pilote/exécutant et la politique de modèle sont réglables et persistés ; un provider tiers est refusé tant que la porte sécurité n'est pas active.

## D. Journal et compte rendu
Chaque couloir tient `lane-<nexus|backend|frontend>-journal.md` dans ce dossier : tranche, commit, tests lancés et résultat, blocages, écarts à ce document. Un compte rendu ne dit « fait » que pour ce qui est commité ET testé ; le reste est listé « non fait » ou « non vérifié ».
