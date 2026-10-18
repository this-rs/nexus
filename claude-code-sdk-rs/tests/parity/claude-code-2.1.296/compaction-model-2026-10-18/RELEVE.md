# RELEVE — le CLI accepte-t-il un modèle de compaction distinct ? (N-C1)

**Mesure du 18/10/2026, claude 2.1.296, modèle `haiku` (résolu `claude-haiku-5-5`).**
Enregistrement normalisé : `compaction_model.json`. Coût total du relevé : environ 0,03 $.

## Verdict

**NON.** Le CLI 2.1.296 résume toujours avec le modèle de la session. Il n'offre aucun moyen d'en
nommer un autre pour la compaction : ni option, ni variable d'environnement, ni clé de réglage, ni champ
dans la réponse d'un hook `PreCompact`.

Conséquence pour nexus (contrat v8) : la façade Claude Code ne déclare pas
`Capabilities::compaction_model`. Quand un hook demande un modèle de résumé, elle émet
`provider_notice { kind: "compaction_model_ignored" }`, résume avec le modèle de la session, et ne
renvoie aucune erreur.

## Mesuré vs déduit

| Fait | Statut | Preuve |
|---|---|---|
| `claude --help` : pour la compaction il n'y a que `--autocompact` (taille de fenêtre) ; pour le modèle, `--model` et `--fallback-model` | MESURÉ | `claude --help` |
| les variables d'environnement `*COMPACT*` du binaire règlent un seuil, une fenêtre ou une désactivation, jamais un modèle | MESURÉ (chaînes du binaire) | `static.compaction_env_vars_in_binary` |
| aucune clé `compactModel` / `compactionModel` / `summaryModel` dans le binaire | MESURÉ (chaînes du binaire) | `static.compaction_model_keys_in_binary` (vide) |
| la requête de résumé prend `cacheSafeParams.model ?? mainLoopModel`, c'est-à-dire le modèle de la boucle principale (elle partage son cache de prompt) | LU dans le code embarqué | `static.summary_request_model` |
| la réponse d'un hook `PreCompact` ne lit aucun champ de modèle | LU dans le code embarqué | `static.pre_compact_hook_output_fields` |
| un `/compact` réel, avec `ANTHROPIC_SMALL_FAST_MODEL=claude-sonnet-4-5`, n'utilise que `claude-haiku-5-5` (`modelUsage`) | MESURÉ | tour `compact_with_small_fast_model` |

## Méthode

Identique aux relevés précédents (`../README.md`) : `claude -p` avec le modèle le moins cher, deux tours
triviaux pour avoir un historique, puis `--resume <session> "/compact"` en `stream-json --verbose`.
Le `modelUsage` du résultat liste les modèles facturés. La compaction a bien eu lieu
(`compact_boundary`, 40 712 → 3 856 jetons).

Piège évité : un `/compact` sur une session déjà compactée échoue en « Not enough messages to compact »
et rend quand même `is_error: false` et le `modelUsage` CUMULÉ de la session. Pour mesurer, il faut un
historique neuf.

## À refaire quand

Une nouvelle version du CLI ajoute une option ou une clé de modèle de compaction : relancer les trois
tours tels quels (les commandes sont dans `compaction_model.json`). Si `modelUsage` liste le modèle
demandé, la façade peut déclarer la capacité.
