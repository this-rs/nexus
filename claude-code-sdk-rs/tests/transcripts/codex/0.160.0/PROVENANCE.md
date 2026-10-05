# Provenance — Codex 0.160.0 (réel)

Contrairement à `../0.130.0/` (dérivé de la documentation), ces fichiers viennent d'un **vrai**
`codex app-server`. Rien ici n'est écrit à la main.

| Fichier | Origine |
|---|---|
| `schema/*.json` | `codex app-server generate-json-schema --out <dir>` avec codex-cli 0.160.0 (celui embarqué dans l'application ChatGPT) ; seuls les fichiers que visent nos requêtes sont gardés : `ThreadStartParams`, `ThreadResumeParams`, `TurnStartParams`, `TurnInterruptParams` (v2) et `InitializeParams` (v1) |
| `real/initialize_result.json` | vraie réponse à `initialize` |
| `real/thread_start_result.json` | vraie réponse à `thread/start` |

Les réponses ont été capturées avec un `CODEX_HOME` et un `HOME` jetables, **sans connexion** : aucun
modèle n'a été appelé, aucune identité de l'utilisateur n'y figure. Les chemins temporaires sont
remplacés par `/TEMP/work`, les identifiants par des UUID nuls, l'horodatage du fichier de session
par `TS`. Les valeurs du modèle (`gpt-6.1-sol`) et du fournisseur sont celles que le serveur a
renvoyées.

## Ce que ces fichiers ont déjà prouvé

`thread/start` et `thread/resume` veulent `sandbox` en **kebab-case** (`read-only`,
`workspace-write`, `danger-full-access`) ; `turn/start` veut `sandboxPolicy` en objet **camelCase**
(`{"type":"workspaceWrite"}`). Le schéma dérivé de la documentation (0.130.0) et le faux exécutable
avaient les deux en camelCase : un vrai serveur a refusé `workspaceWrite` (`-32600`).

## Régénérer

```
C=/chemin/vers/codex            # >= 0.160.0
$C app-server generate-json-schema --out /tmp/codex-schema
```

Puis recopier les fichiers listés ci-dessus et refaire les captures `real/` avec un `CODEX_HOME`
jetable. Les tests `tests/codex_real_schema.rs` (hors ligne) et `tests/codex_real.rs` (ignoré, vrai
Codex) s'en servent.
