# Provenance — Codex 0.162.1 (réel, 18/10/2026, tâche CODEX-163)

Comme `../0.160.0/`, ces fichiers viennent d'un **vrai** `codex app-server` ; rien n'est écrit à
la main. Binaire : `/opt/homebrew/bin/codex`, `codex --version` = `codex-cli 0.162.1` (macOS arm64).

| Fichier | Origine |
|---|---|
| `schema/*.json` | `codex app-server generate-json-schema --out <dir>` (39 fichiers produits) ; gardés : `InitializeParams` (v1), `ThreadStartParams`, `ThreadResumeParams`, `TurnStartParams`, `TurnInterruptParams` (v2) |
| `real/initialize_result.json` | vraie réponse à `initialize` |
| `real/thread_start_result.json` | vraie réponse à `thread/start` (`approvalPolicy: on-request`, `sandbox: workspace-write`) |

Capture : `codex app-server` **sans sous-commande** (transport par défaut `--listen stdio://`), avec
un `CODEX_HOME` et un `HOME` jetables, **sans connexion** : aucun modèle appelé (le serveur journalise
un 401 sur son websocket modèle, attendu), aucune identité de l'utilisateur. Chemins temporaires
remplacés par `/TEMP/work`, identifiants par des UUID nuls, horodatage du fichier de session par `TS`.

## Écart avec 0.160.0

Sur les cinq schémas de requête : `InitializeParams`, `ThreadStartParams`, `TurnInterruptParams`
identiques ; `TurnStartParams` ajoute `parentTurnId` et `rootTurnId` (optionnels, nullables) ;
`ThreadResumeParams` ajoute la valeur `partial_answer` à la phase d'un message et un élément
`additional_tools` à l'historique. Rien de requis n'est ajouté : ce que l'adaptateur envoie reste
valide (`tests/codex_real_schema.rs` le vérifie contre les deux versions).

## Ce qui n'est PAS ici

Le daemon partagé (`codex app-server daemon …`, paquet 0.163.0) et `codex app-server proxy --sock
<socket>` : l'adaptateur ne s'en sert pas. Par le proxy, une ligne `initialize` n'obtient aucune
réponse (stdout vide, exit 0) ; son protocole est NON VÉRIFIÉ.

## Régénérer

```
C=/chemin/vers/codex
$C app-server generate-json-schema --out /tmp/codex-schema
```

Puis recopier les fichiers listés ci-dessus et refaire les captures `real/` avec un `CODEX_HOME`
jetable (il doit exister : `codex app-server` refuse un `CODEX_HOME` absent).
