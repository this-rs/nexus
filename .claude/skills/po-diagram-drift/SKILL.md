---
name: po-diagram-drift
description: Keep docs/diagrams/*.mmd in sync with code - headers, status legend, the drift gate (cargo xtask diagrams), the Diagram-Unchanged trailer, linting Mermaid offline. Use when a PR touches a file listed in docs/diagrams/INDEX.yml, when drift CI fails, or when drawing/updating a Mermaid diagram.
---

# po-diagram-drift — diagramme = code d'aujourd'hui

Charte complète : `docs/diagrams/README.md`. Gabarit : `docs/diagrams/_TEMPLATE.mmd`.

## Quand tu modifies du code
1. Trouver le propriétaire : `docs/diagrams/INDEX.yml` (champ `covers`). Un fichier a un seul propriétaire.
2. Mettre à jour le `.mmd` **dans la même PR** : un nœud par fonction réelle, prouvée par un **nom de fonction** (jamais un numéro de ligne).
3. Mettre à jour `verified: YYYY-MM-DD` (ou sha court) ; refusé sinon (`yesterday`, `TODO`, date impossible).
4. Régénérer l'index : `cargo xtask diagrams index --write`.
5. Rejouer le portillon : `cargo xtask diagrams drift --base origin/main`.
6. Diagramme resté exact ? Trailer de commit `Diagram-Unchanged: <nom> — <raison>` ; la raison est obligatoire et le nom doit exister.

## Légende obligatoire
✅ écrit et vérifié par un test (le libellé dit lequel) ; 🟠 décidé non écrit ; 🔴 bug confirmé ou code non branché ; ⚪ hors dépôt. Un ✅ sans moyen de vérification nommé n'est pas un ✅.

## Linter hors ligne
Pas de `mmdc` : suivre la recette de la note « Linter un .mmd hors ligne avec le vrai parseur Mermaid v11 » (`note(action:"search_semantic", query:"linter mmd jsdom parseur mermaid")`). Toujours inclure le contrôle négatif.
Pour la palette, les styles `classDef` et l'outil de dessin de l'utilisateur, charger le skill utilisateur `mermaid-design`.

## Pièges mesurés
- Le portillon prouve que la question a été posée, pas que le diagramme est exact. Recouper soi-même chaque nœud avec `grep -n "fn <nom>"`.
- Ligne `%%` nue : parseur et GitHub divergent ; écrire `%% ---`.
- `dist/mermaid.min.js` v11 n'est plus un IIFE ; `globalThis.navigator` est en lecture seule sous Node ≥ 21 (utiliser `Object.defineProperty`).
- Les cartes `po-*` vivent dans le dépôt **backend** (`backend/docs/diagrams/`), pas ici.
- Dépôts publics : ne mettre ni nom d'hôte ni nom d'outil de dessin personnel dans diagrammes, fixtures ou docs ; utiliser « outil de dessin ».
- Divergence code ↔ diagramme = bug : une tâche, une note `gotcha` (`po-capture`), un nœud 🔴.
