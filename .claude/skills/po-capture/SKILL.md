---
name: po-capture
description: Record what was learned in Project Orchestrator - notes (gotcha, guideline, pattern, tip), decisions with AFFECTS links, supersession of stale knowledge. Use after fixing a bug with a non-obvious cause, after choosing between alternatives, when a note turns out wrong, or at the end of a task. Triggers - "retiens", "note ça", "documente la décision", "c'était faux".
---

# po-capture — ne pas laisser la connaissance dans le terminal

## Note
`note(action:"create", project_id, note_type:"gotcha"|"guideline"|"pattern"|"tip"|"observation"|"context", importance:"critical"|"high"|"medium"|"low", content, tags:[…], anchors:[{entity_type:"File", entity_id:"<chemin absolu>"}])`
- Contenu : symptôme, cause racine, correctif (sha), **comment vérifier**, ce qui reste ouvert.
- Les `anchors` rattachent la note au code ; sans ancre elle ne remonte que par similarité.
- Alternative après coup : `note(action:"link_to_entity", note_id, entity_type, entity_id)`.

## Décision
1. `decision(action:"add", task_id, description, rationale, alternatives:["option: pourquoi écartée"], chosen_option)` → `decision_id`.
2. Pour chaque fichier concerné : `decision(action:"add_affects", decision_id, entity_type:"File", entity_id:"<chemin absolu>", impact_description)`. Sans AFFECTS la décision est orpheline : `analyze_impact` et `get_propagated` ne la verront jamais.
3. Remplacement : `decision(action:"supersede", decision_id:"<ancienne>", superseded_by_id:"<nouvelle>")`.

## Corriger la connaissance
- Note fausse : `note(action:"supersede", old_note_id, note_type, content, …)` (crée la nouvelle dans le même appel) ; sinon `note(action:"invalidate", note_id, reason)`. Ne pas supprimer : la chaîne d'historique compte.
- Note toujours juste : `note(action:"confirm", note_id)`.

## Pièges
- `entity_type` est sensible à la casse côté graphe (`File`, pas `file`). Un `add_affects` sur un nœud absent a longtemps répondu `ok` sans rien créer ; le correctif fait maintenant échouer l'appel. Relire avec `decision(action:"list_affects", decision_id)`.
- Les chemins de fichier sont absolus pour `File`.
- Une correction publique doit dire ce qui était faux : voir la note « trois copies de useConversationWs » corrigée le 02/10/2026.
- Fuites de noms : le balayage de clôture porte sur le nom nu, pas sur ses formes qualifiées.
