# Parité des outils : enregistrements du vrai Claude Code 2.1.287 (N17, N25)

Ces fichiers sont ce que les outils de Claude Code ont **réellement** répondu, lu dans le flux
`stream-json` de `claude -p`. Ils servent de référence aux outils de `nexus-tools` (plan Nexus N19,
N20) : un appel rejoué sur nos outils doit donner le même résultat, ou un écart **volontaire**
consigné dans `docs/agent-tools-parity.md`.

| Fichier | Outil(s) | Appels |
|---|---|---|
| `read.json` | `Read` : cas courants et erreurs | 7 |
| `read_limits.json` | `Read` : fichier de 3 000 lignes, ligne de 60 000 caractères, très gros fichier, décalages | 8 |
| `edit.json` | `Edit`, `Write`, avec l'état « lu » par session | 14 |
| `bash.json` | `Bash` : sortie, code, durée, répertoire persistant, arrière-plan | 10 |
| `bash_limits.json` | `Bash` : seuil de sortie persistée, durée, erreurs | 7 |
| `notebook.json` | `NotebookEdit` et la lecture d'un notebook | 6 |

Chaque fichier : `calls` (l'outil, son `input` tel que le modèle l'a envoyé, `is_error`, le
`result`), et `final_files` (le contenu des fichiers après la session, pour prouver qu'un échec ne
modifie rien). Un résultat de plus de 6 000 caractères est réduit à sa longueur, son début et sa fin.

## Normalisation

Le dossier de travail est remplacé par `<FIXTURE>`, les chemins d'état de Claude Code par
`<CLAUDE_STATE>/…` et `<CLAUDE_TMP>/…`, les UUID par `<UUID>`, l'identifiant d'une tâche
d'arrière-plan par `<TASK>`, l'identifiant d'une cellule insérée par `<CELL>`. Le coût n'est pas
conservé.

## Les fichiers de test

Synthétiques : aucune donnée de l'utilisateur. `a.txt` (12 lignes `line N`), `empty.txt`, `long.txt` (une
ligne de 2 500 `x` entre deux courtes), `dup.txt` (`alpha`, `beta`, `alpha`, `gamma`, `alpha`),
`uniq.txt` (`one`, `two`, `three`), `unread.txt` (`untouched`), `nb.ipynb` (deux cellules `c1` code et `c2`
markdown), `sub/inner.txt`, `a3000.txt` (3 000 lignes `row N`), `line60k.txt` (60 000 `y` sur une ligne),
`big.txt` (3 300 lignes de quatre mots répétés `wordNNNNN`).

## Comment c'est enregistré (et ce qui reste à faire)

```
claude -p "<invite>" --model haiku --output-format stream-json --verbose \
  --permission-mode bypassPermissions --tools "<outils>" --max-turns 40     # cwd = le dossier de test
```

L'invite demande **exactement** une suite d'appels, un à la fois, avec les paramètres donnés, sans
commentaire. Les enregistrements ont été faits avec un script **provisoire hors dépôt** (Python) :
la règle de nexus est « pas de Python », donc l'enregistreur sera réécrit en Rust dans `xtask` par
la tâche N25, avec ces mêmes invites.

Coût : 0,42 dollar au total, avec l'accord explicite de l'utilisateur (consomme des jetons de son
compte). `WebFetch` n'existe pas dans `claude -p` 2.1.287 : non enregistré.
