# Diagrammes du dépôt

Les diagrammes sont des fichiers Mermaid **natifs** : `docs/diagrams/<nom>.mmd`.
GitHub les rend directement. Aucun outil externe, aucun service, aucun réseau :
le gate de CI doit fonctionner hors ligne.

## Ce qu'un diagramme affirme

Un diagramme décrit **ce que le code fait aujourd'hui**, pas ce qu'on aimerait
qu'il fasse. Chaque nœud est donc prouvé par un **nom de fonction**, jamais par
un numéro de ligne (les lignes bougent, les noms se cherchent avec `grep`).

### Légende — obligatoire dans chaque diagramme

| Marque | Sens |
|---|---|
| ✅ | écrit **et** vérifié par un test ; le libellé dit comment (transport factice, service réel, test rejoué sans le correctif) |
| 🟠 | décidé, pas encore écrit |
| 🔴 | bug confirmé, ou code écrit mais non branché |
| ⚪ | hors de ce dépôt (amont / appelant) |

Un ✅ sans moyen de vérification nommé n'est pas un ✅.

## En-tête obligatoire

Les trois premières lignes d'un `.mmd` sont des commentaires Mermaid lus par
`scripts/derive_diagram_index.py` :

```
%% name: nexus-sdk-transport
%% covers: claude-code-sdk-rs/src/transport/*.rs
%% verified: 2026-10-01
```

- `name` : identique au nom de fichier, sans extension.
- `covers` : un ou plusieurs globs, séparés par des virgules. La règle voulue : tout
  fichier modifié par une PR et capté par un `covers` oblige à toucher le diagramme
  correspondant dans la même PR, ou à porter le trailer de commit
  `Diagram-Unchanged: <nom> — <raison>`. La règle est appliquée sur chaque PR par
  `scripts/check_diagram_drift.py` (job CI `diagram-drift`) ; la raison est obligatoire,
  un trailer sans raison ou nommant un diagramme inexistant fait échouer le job. En
  local : `python3 scripts/check_diagram_drift.py --base origin/main`. Ce que ce job ne
  prouve PAS : que le diagramme est exact — il garantit seulement que la question a été
  posée. La CI vérifie par ailleurs : en-têtes complets et bien formés, chaque glob
  matche un fichier, un fichier a un seul propriétaire, `INDEX.yml` à jour.
- `verified` : date ISO (`YYYY-MM-DD`) du dernier recoupement avec le code, ou sha git
  court. Ce n'est plus une convention : `derive_diagram_index.py` refuse toute autre valeur
  (`yesterday`, `TODO`, une date impossible), parce que `verified` n'a de sens que si un
  relecteur peut rejouer ce que l'auteur a vu — une date se retrouve par `git log --until`,
  un sha par `git show <sha>:<fichier>`.

## Cycle de vie

1. **Créer** — relever le code, nommer les fonctions, poser les statuts.
2. **Recouper** — toute divergence code↔diagramme est un bug : une tâche, une
   note `gotcha`, un nœud 🔴, un test qui échoue sans le correctif.
3. **Mettre à jour** — dans la même PR que le code qu'il décrit.
4. **Archiver** — un diagramme qui ne couvre plus rien sort de `INDEX.yml`.

## Index

`INDEX.yml` fait la liste des diagrammes et de ce qu'ils couvrent. Un fichier
source sans diagramme propriétaire est un trou connu, listé par le gate.
