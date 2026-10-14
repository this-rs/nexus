# Parité des outils — MESURE `Task` / `--disallowedTools` sur le vrai Claude Code 2.1.296

Ce dossier est le relevé d'une MESURE (pas une livraison) qui fonde la décision « le mode décide qui
fournit l'outil de sous-agent » (AUTO-SUB). Question : le CLI accepte-t-il de se voir retirer son outil
intégré `Task`, et un modèle à qui on le retire appelle-t-il un substitut ?

- `task_disallowed.json` : les trois tours (témoin, retrait, substitut), normalisés — `system_init.tools`
  complet, la commande exacte de chaque tour, le blocage, le diff.
- `RELEVE.md` : le verdict en trois lignes, mesuré vs déduit, le blocage et le moyen du substitut.

Méthode (identique à 2.1.287, voir `../claude-code-2.1.287/README.md`) :
`claude -p "<invite>" --model haiku --output-format stream-json --verbose --permission-mode bypassPermissions`,
avec `--disallowedTools Task` pour le retrait, et `--strict-mcp-config --mcp-config <fake_mcp+Agent>`
pour le substitut.

Normalisation : aucun chemin utilisateur dans le relevé ; la liste des outils est brute (noms seuls,
comme le CLI les émet dans `system_init.tools`). Le coût n'est pas conservé (0 $ ici : la limite de
dépense a bloqué avant toute consommation).

## Résultat attendu de la relecture

Une fois la limite de dépense relevée, rejouer les trois tours tels quels (invites et commandes sont dans
`task_disallowed.json`) ; seul le champ `model_behavior` passera de `BLOCKED` au comportement réel.
