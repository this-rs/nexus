# RELEVÉ — les hooks SessionStart, UserPromptSubmit, Stop, SubagentStop, Notification du vrai Claude Code

**Mesure du 18/10/2026, claude 2.1.296, modèle `haiku` (claude-haiku-5-5).** Fonde ISO1 (contrat v7 :
`SessionHooks::session_start` / `user_prompt_submit` / `before_stop` / `before_subagent_stop` /
`notification`). Enregistrement normalisé : `hooks.json` (aucun chemin, aucun identifiant de session).
Coût total : 0,041 $ (11 scénarios).

## Méthode

Le chemin de nexus et du backend : des **callbacks SDK**, pas des commandes. Un hôte de mesure (script
Node hors dépôt, dans le scratchpad de la session) lance

```
claude -p --input-format stream-json --output-format stream-json --verbose --model haiku --setting-sources project
```

déclare les hooks dans la requête de contrôle `initialize` (`hooks: {Event: [{matcher: null,
hookCallbackIds: ["cb_Event"]}]}`), envoie l'invite, répond à chaque `hook_callback` par la réponse du
scénario et enregistre tout le flux. Un seul scénario (s10) passe par un hook de commande
(`.claude/settings.json`), pour SessionStart.

## Verdicts

| Événement | Réponse du hook | Effet mesuré |
|---|---|---|
| `Stop` | `{"decision":"block","reason":R}` | Le tour **continue** dans le MÊME `result` : le CLI ajoute un message utilisateur synthétique `Stop hook feedback:\nR` (`isSynthetic`), le modèle répond de nouveau, le hook est rappelé avec `stop_hook_active: true` et `last_assistant_message` = la nouvelle réponse. Un seul `result` (`num_turns: 2`). Le CLI émet aussi une `system/notification` `stop-hook-error` (sans effet). (s1) |
| `Stop` | bloque toujours | Le CLI passe outre au **9ᵉ blocage d'affilée** : `system/informational` + `system/notification` `stop-hook-block-cap` (« A hook blocked the turn from ending 9 consecutive times — overriding and ending turn … `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP` »), puis `result success`. 9 appels du hook, 8 relances. (s5) |
| `Stop` | `{"systemMessage":M}` | `system/informational` « Stop says: M » (niveau `notice`), montré à l'utilisateur, pas au modèle. (s6) |
| `UserPromptSubmit` | `hookSpecificOutput.additionalContext: C` | Le modèle lit C avec l'invite (il répond « ZEPHYR-42 »). (s2) |
| `UserPromptSubmit` | `{"decision":"block","reason":R}` | **Aucune requête** : `system/informational` « UserPromptSubmit operation blocked by hook:\nR\n\nOriginal prompt: P » (`prevent_continuation: true`), puis `result success`, `num_turns: 0`, `result` = ce texte, coût 0. L'invite n'est pas gardée ; l'invite suivante passe de nouveau par le hook. (s3) |
| `UserPromptSubmit` | `{"systemMessage":M}` | `system/informational` « UserPromptSubmit says: M ». (s6) |
| `SessionStart` | callback SDK, toute réponse | **Jamais appelé** : le CLI lance les hooks SessionStart au démarrage, avant de lire `initialize` ; aucun `hook_callback` SessionStart dans s4 et s11. |
| `SessionStart` | hook de **commande**, `hookSpecificOutput.additionalContext: C` + `systemMessage` | `hook_started` / `hook_response` dans le flux ; le modèle connaît C au premier tour. Le `systemMessage` n'apparaît pas dans le flux. (s10) |
| `SubagentStop` | `{"decision":"block","reason":R}` | Le **sous-agent** continue : message `Stop hook feedback:\nR`, sa réponse finale tient compte de R (le parent rapporte « ZEPHYR-42 »). Entrée : `stop_hook_active`, `last_assistant_message`, `agent_id`, `agent_type`. En 2.1.296 le sous-agent `Agent` part en arrière-plan (`is_backgrounded: true`). (s8) |
| `Notification` | déclaré en callback SDK | **Jamais appelé** dans les scénarios s9 (refus du CLI lui-même) et s11 (`--permission-prompt-tool stdio`, `can_use_tool` refusé par l'hôte). La doc embarquée du CLI : « Exit code 0 - stdout/stderr not shown » — observation seule ; `notification_type` ∈ `permission_prompt`, `idle_prompt`, `auth_success`, `elicitation_dialog`, `agent_needs_input`, … |

Entrées relevées : `Stop` = `{session_id, prompt_id, permission_mode, effort, hook_event_name, stop_hook_active,
last_assistant_message, background_tasks, session_crons}` ; `UserPromptSubmit` = `{…, prompt}`.

## Ce que nexus en fait (contrat v7)

- `before_stop` bloqué → le harnais natif ajoute le message `Stop hook feedback:\n<raison>` et relance la
  boucle ; plafond `STOP_HOOK_BLOCK_CAP = 9` (le 9ᵉ blocage d'affilée est passé outre, notice
  `stop_hook_block_cap`). La façade Claude Code répond `{decision:"block", reason}` et laisse le CLI faire.
- `user_prompt_submit` bloqué → natif : aucune requête, `prompt_blocked`, `done { num_turns: 0 }` avec le
  texte exact du CLI ; façade : `{decision:"block", reason}` (le CLI fait le reste) + la même notice.
- `session_start` → appelé par le harnais lui-même avant le premier tour, SUR LES DEUX moteurs (le CLI ne
  l'envoie pas aux callbacks SDK) ; son contexte ouvre le premier message.
- `notification` → `permission_prompt` dit par le harnais à chaque demande de permission, sur les deux
  moteurs (le CLI ne l'envoie pas aux callbacks SDK) ; la façade retransmet les autres types si le CLI en envoie.
- `system_message` → notice `hook_system_message`, jamais au modèle.

## Pas mesuré

`Notification` d'un autre type que `permission_prompt` (`idle_prompt` demande 60 s d'inactivité en mode
interactif) ; SubagentStop sur un sous-agent au premier plan ; `continue: false` / `stopReason`.
