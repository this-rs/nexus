# RELEVE — « le mode décide » : le CLI lâche-t-il `Task`, et le modèle appelle-t-il un substitut ?

**Mesure du 15/10/2026, claude 2.1.296, modèle résolu `claude-haiku-5-5` (alias `haiku`),**
`--output-format stream-json`, `--permission-mode bypassPermissions`. Enregistrement complet :
`task_disallowed.json`.

## Verdict (trois lignes)

1. **OUI, le CLI lâche `Task`.** `--disallowedTools Task` retire `Task` de `system_init.tools`
   (présent sans retrait, absent avec) — effet de bord mesuré : `WebFetch` apparaît en remplacement.
2. **INDÉTERMINÉ : le modèle n'a émis aucun appel d'outil.** Les trois tours sont morts sur
   « You've hit your monthly spend limit » (limite de dépense mensuelle du compte claude.ai),
   `terminal_reason: api_error`, coût 0. Appel d'un substitut, refus, ou bascule : non mesurable.
3. **Pour la décision AUTO-SUB :** le fait (1) — le CLI accepte de se voir retirer `Task` — est
   CONFIRMÉ ; le fait (2) — un modèle à qui `Task` est retiré appelle un substitut `Agent` — reste
   NON VÉRIFIÉ. Rejouer après relevé de la limite, ou avec une clé API hors abonnement.

## Mesuré vs déduit

| Fait | Statut | Preuve |
|---|---|---|
| `disallowed_tools` du SDK → `--disallowedTools` | MESURÉ (code) | `claude-code-sdk-rs/src/transport/subprocess.rs:420-423`, test `:2294-2302` |
| `--disallowedTools` existe dans le CLI 2.1.296 | MESURÉ | `claude --help` |
| `Task` présent dans `system_init.tools` sans retrait | MESURÉ | tour témoin |
| `Task` absent quand `--disallowedTools Task` | MESURÉ | tour retrait |
| `WebFetch` apparaît quand `Task` est retiré | MESURÉ (effet de bord) | diff témoin↔retrait |
| le substitut `mcp__fake_mcp__Agent` est offert | MESURÉ | tour substitut (fake_mcp + `FAKE_MCP_EXTRA_TOOLS`) |
| le modèle appelle le substitut (ou refuse, ou bascule) | **INDÉTERMINÉ** | limite de dépense : aucun appel d'outil |

## Le blocage

Le compte claude.ai a atteint sa **limite de dépense mensuelle**. Le CLI démarre, émet `system_init`
(liste d'outils correcte), puis le premier tour du modèle échoue en `api_error` avec :
« You've hit your monthly spend limit · raise it at https://claude.ai/settings/usage · your weekly
limit resets Oct 17 at 10am (Europe/Paris) ». Aucun jeton n'est consommé (`total_cost_usd: 0`).

Pour débloquer : relever la limite (décision de l'utilisateur), ou fournir une clé API hors abonnement
(le trousseau ne contient que le jeton OAuth, aucune clé API).

## Le substitut, par quel moyen

Tour substitut : `Task` retiré ET un outil nommé `Agent` offert. Moyen SANS code de production :
le binaire de test existant `claude-code-sdk-rs/src/bin/fake_mcp.rs` accepte une liste d'outils
supplémentaires via `FAKE_MCP_EXTRA_TOOLS` (fichier JSON). L'outil apparaît sous le nom
`mcp__fake_mcp__Agent` dans `system_init.tools`. Montage :
`--strict-mcp-config --mcp-config '{"mcpServers":{"fake_mcp":{"command":<fake_mcp>,"args":["--stdio"],"env":{"FAKE_MCP_EXTRA_TOOLS":<extra.json>}}}}'`.

## Coût

0,00 $ (la limite de dépense a bloqué avant toute consommation).
