---
name: po-warmup
description: Load project knowledge from Project Orchestrator before starting work. Use at the start of a session, when the user names a feature, bug, file or crate you have not read yet, or before proposing a design. Triggers - "où en est", "reprends", "continue le plan", "what do we know about", any task touching transport/, interactive/, claude-code-api or CI.
---

# po-warmup — lire avant d'écrire

Slug du projet : `nexus-sdk` (id `6fc5c5f7-959f-4a20-b4c3-28aac024c580`). Backend : `project-orchestrator-backend`.

## Séquence
1. `note(action:"search_semantic", query:"<sujet en une phrase>", project_slug:"nexus-sdk", limit:8)` — d'abord les `gotcha` et `guideline`.
2. `decision(action:"search_semantic", query:"<sujet>", project_id:"<uuid>")` — ce qui est déjà tranché (une décision `superseded` ne fait plus foi).
3. Fichier connu : `note(action:"get_context", entity_type:"file", entity_id:"<chemin absolu>")` puis `decision(action:"get_affecting", entity_type:"File", entity_id:"<chemin absolu>")`.
4. Travail en cours : `plan(action:"list", project_id:"<uuid>", status:"in_progress")` puis `task(action:"get_next", plan_id:"<uuid>")`.
5. Diagramme propriétaire : lire `docs/diagrams/INDEX.yml` (voir `po-diagram-drift`).

## Pièges
- Tous les `*_id` sont des UUID, jamais des slugs. Un slug donne une erreur de parsing ; `project(action:"get", slug:"…")` donne l'UUID.
- `search_semantic` classe par similarité, pas par fraîcheur : regarder `created_at` et `status`. Une note vieille de plus de 60 jours peut être `low_energy` et obsolète.
- Une note du backend peut décrire un code déjà refondu : recouper avec `grep` avant de s'y fier. Une note n'est pas une preuve.
- Mesurer sur `origin/main`, pas sur l'arbre local, qui peut avoir des dizaines de commits de retard (un `grep` local vide ne prouve rien).
