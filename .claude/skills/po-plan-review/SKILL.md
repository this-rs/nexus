---
name: po-plan-review
description: Review a PO plan before running or delegating it - dependency graph, waves, critical path, file ownership, run status, delegation to agents or the plan runner. Use before plan(action:"run") or delegate_task, before launching several agents on one repo, or when asked "est-ce que le plan tient", "découpe en vagues", "lance le plan".
---

# po-plan-review — vérifier avant de lancer

## Revue
1. `plan(action:"get", plan_id)` puis `task(action:"list", plan_id)` — chaque tâche a `acceptance_criteria` vérifiables et `affected_files`.
2. `plan(action:"get_dependency_graph", plan_id)` ; `plan(action:"get_critical_path", plan_id)` ; `plan(action:"get_waves", plan_id)` — les tâches d'une vague doivent être indépendantes.
3. **Disjonction des fichiers** : deux tâches d'une même vague ne partagent aucun élément de `affected_files`. Sinon `task(action:"add_dependencies", …, dependency_ids:[…])`.
4. `plan(action:"predict_run", plan_id)` ; historique : `plan(action:"list_runs", plan_id)`, `compare_runs` avec `run_ids:[…]`.

## Lancer
- **Avant tout** : `plan(action:"run_status", plan_id)` et `plan(action:"list_triggers", plan_id)`. Puis, côté git : `git rev-parse --abbrev-ref HEAD | grep '^runner/'` et `git status --porcelain`.
- Runner : `plan(action:"run", plan_id, cwd:"<abs>", project_slug:"nexus-sdk")` ; suivi `run_status` ; arrêt `cancel_run` ; PR `auto_pr`.
- Une tâche à un agent : `plan(action:"delegate_task", plan_id, task_id, cwd, project_slug)` ; `parent_session_id` crée la relation SPAWNED_BY.
- Plan à `draft` : passer `approved` puis `in_progress` avant de lancer.

## Pièges mesurés
- « Un fichier = un propriétaire » ne protège que dans le périmètre qui l'applique. Le 01/10/2026 un run autonome et 12 agents ont édité 6 fichiers communs ; le run a aussi marqué en échec une tâche `in_progress`. Rétablir les statuts à la main après coup, et réconcilier fichier par fichier.
- Un fichier qui change sur disque dans l'arbre principal alors que personne ne doit y travailler est le signal à ne pas ignorer.
- Un run termine souvent `completed_with_errors` : lire `get_run` avant de croire aux statuts.
- Fichiers à travailler dans un worktree neuf issu de `origin/main` si l'arbre principal est sale.
- Mesures de couverture : un `target` copié ou `cargo llvm-cov` qui note ses propres tests faussent les chiffres (écart de 21 points sur nexus).
- Vérifier qu'une tâche est bien terminée par `commit(action:"get_task_commits", task_id)`, pas par son statut.
