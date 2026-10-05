---
name: po-task-run
description: Execute one PO task end to end - plan, task, steps, code, verification, commit registration. Use when starting or finishing a task, when the user says "prends la tâche", "implémente T3", "marque comme fini", or when creating a plan/task/step structure.
---

# po-task-run — plan → task → step → commit

## Créer la structure (si absente)
1. `plan(action:"create", title, description, project_id, priority)` — statuts : `draft → approved → in_progress → completed` (ou `cancelled` depuis tout état non final). Pas de saut ni de retour : `plan(action:"update_status", plan_id, status)` refuse.
2. `task(action:"create", plan_id, title, description, acceptance_criteria:[…], affected_files:[…], depends_on:[uuid…], steps:[{description, verification}])` — `plan_id` obligatoire (erreur n°1).
3. Dépendances ajoutées après coup : `task(action:"add_dependencies", task_id, dependency_ids:[…])` — **pluriel**. Le singulier `dependency_id` ne sert qu'à `remove_dependency`.

## Exécuter
1. `task(action:"update", task_id, status:"in_progress", session_id:"<chat session>")` — le `session_id` lie session, tâche et plan.
2. Chaque step : `step(action:"update", step_id, status:"in_progress")` puis `"completed"` ou `"skipped"` une fois la `verification` jouée. `step(action:"create")` demande `task_id` et `description`.
3. Avant de modifier : `po-impact`. Après une décision de conception : `po-capture`.
4. Vérifier réellement : `cargo fmt --all -- --check`, `cargo clippy --all-targets -D warnings`, `cargo test -p <crate>`.
5. Commit git, puis `commit(action:"create", sha, message, author, files_changed:[…], project_id:"<uuid>")` — sans `project_id`, aucune resynchronisation incrémentale du graphe. Puis `commit(action:"link_to_task", task_id, sha)` (et `link_to_plan`).
6. `task(action:"update", task_id, status:"completed", actual_complexity)`.

## Pièges mesurés
- Ne jamais auto-compléter des steps ou la tâche sans commit réel : « completions fantômes » du runner (note 3ff31883), corrigées par `verify_has_commits`.
- Le garde de complétion refuse `0/N steps completed` si tout est `skipped` : sauter n'est pas finir.
- `cargo fmt` après tout renommage de chaînes : un nom plus long fait reformater des fichiers et rougit le portillon de format.
- Un `cmd | head && echo OK` teste `head`, pas `cmd`. Utiliser `if cmd >out 2>&1; then …`.
- Dépôt nexus : `Cargo.lock` ignoré, la CI résout à neuf ; une dépendance transitive peut casser `main` sans changement de code. Nommer le MSRV.
- Pousser avec un refspec explicite (`git push origin HEAD:refs/heads/<branche>`) ; `main` est protégée et une branche qui suit `origin/main` y pousserait.
