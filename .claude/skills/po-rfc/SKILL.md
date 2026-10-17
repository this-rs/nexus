---
name: po-rfc
description: Write and advance an RFC through the PO rfc-lifecycle protocol (note_type rfc, rfc-run tag, advance_rfc, status, supersession). Use when proposing a cross-cutting design (provider boundary, transport change, public API break), when asked "écris une RFC", "fais avancer la RFC", or "où en est la RFC".
---

# po-rfc — proposer, instruire, trancher

## Écrire
1. `po-warmup` d'abord : `note(action:"list_rfcs", project_id)` pour ne pas dupliquer ; `decision(action:"search_semantic")`.
2. `note(action:"create", project_id, note_type:"rfc", importance:"high", content, tags:[…])`. Le contenu : contexte mesuré, proposition, alternatives écartées, risques, plan de migration, critères d'acceptation vérifiables.
3. À la création, le serveur lance un run `rfc-lifecycle` et ajoute le tag `rfc-run:<uuid>` — **seulement si** le projet possède un protocole nommé `rfc-lifecycle`. Sinon la note est créée sans cycle de vie, sans erreur : vérifier avec `note(action:"get_rfc_status", note_id)` (`rfc_run: null` = pas de run) et `protocol(action:"list", project_id)`.

## Faire avancer
- `note(action:"get_rfc_status", note_id)` donne l'état courant du run.
- `note(action:"advance_rfc", note_id, trigger:"<nom de transition>")`. Le nom exact vient de `protocol(action:"list_transitions", protocol_id)` ; ne pas l'inventer. États : draft, proposed, under_review, accepted, planning, in_progress, implemented, rejected, superseded.
- `accepted` : enregistrer les décisions (`po-capture`) puis créer plan et tâches (`po-task-run`, `po-plan-review`).
- Remplacement : `note(action:"supersede", old_note_id, note_type:"rfc", content)`. Ne jamais supprimer une RFC dépassée.

## Pièges
- `advance_rfc` échoue avec « no rfc-run:UUID tag » sur une note sans run : ce n'est pas un bug du transition, la note n'a pas de cycle de vie.
- Une RFC décrit des états futurs : les marquer 🟠 dans tout diagramme lié (`po-diagram-drift`), jamais ✅.
- Une RFC qui cite des chiffres doit dire comment les rejouer ; les corrections se font par `supersede` ou `invalidate`, avec la raison.
- Frontière provider (ChatManager, SDK) : lire la RFC « Harness multi-provider » et `nexus-sdk-provider-boundary.mmd` avant d'en ouvrir une concurrente.
