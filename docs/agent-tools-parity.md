# Outils livrés avec nexus : matrice de parité avec Claude Code (N17)

**Statut : brouillon du 2026-10-05.** Source : la liste d'outils réelle de la session Claude Code
2.1.287 qui a produit ce document (schémas lus dans la session, pas de mémoire). Là où le schéma d'un
outil n'était pas visible, la case dit **« à relever »** : elle se remplit par l'étape 2 de N17 ou par
le banc de N25, jamais par supposition.

Consigne de l'utilisateur : les outils MCP sont livrés **au minimum à équivalence** de ceux de Claude
Code, de façon **exhaustive**, recherche internet comprise, **sans autre dépendance que ce qui se
compile en Rust**.

## 1. Classes

| Classe | Sens | Où |
|---|---|---|
| **A** | outil MCP livré par `nexus-tools` | plan Nexus N18 à N23 |
| **B** | capacité du harnais, déjà dans le contrat | `Capabilities`, `PolicyMode`, événements |
| **C** | couvert par le MCP du PO | tâches, étapes, déclencheurs |
| **D** | à construire plus tard | délégation parallèle de type Fusion |
| **E** | hors périmètre, avec la raison | propre au produit Claude |

## 2. Inventaire

### Outils chargés d'emblée dans la session

| Outil | Classe | Raison, ou ce que le contrat couvre déjà |
|---|---|---|
| `Read` | **A** (N19) | fichiers, images, PDF, notebooks |
| `Write` | **A** (N19) | |
| `Edit` | **A** (N19) | |
| `Bash` | **A** (N20) | |
| `AskUserQuestion` | B | événement `question` et capacité `native_question` |
| `Agent` | B / D | capacité `subagents` ; la délégation parallèle est la classe D |
| `Skill` | B | chargement d'instructions : à porter au niveau du harnais, pas en outil MCP |
| `ToolSearch` | B | chargement différé d'outils : utile dès que le catalogue grossit (coût de contexte), voir §5 |
| `ScheduleWakeup` | C | déclencheurs de plans du PO |
| `Workflow` | D | orchestration multi-agents |
| `ListAgents`, `SendMessage` | C | communication entre sessions : MCP du PO |
| `Artifact` | E | produit Claude (pages hébergées) |
| `ReportFindings` | E | produit Claude (revue de code) |

### Outils chargés à la demande (déclarés dans la session)

| Outil | Classe | Raison |
|---|---|---|
| `WebFetch` | **A** (N21) | |
| `WebSearch` | **A** (N22) | l'éditeur fournit le service de recherche ; nous non : voir N22 |
| `NotebookEdit` | **A** (N19) | |
| `Monitor` | **A** (N20) | flux de sortie d'un script en arrière-plan |
| `TaskStop` | **A** (N20) | arrêt d'une tâche d'arrière-plan |
| `EnterPlanMode`, `ExitPlanMode` | B | `PolicyMode::PlanOnly` |
| `EnterWorktree`, `ExitWorktree` | A ou D | worktree par tâche : outil de nexus-tools, ou comportement du runner du PO ; à trancher |
| `CronCreate`, `CronDelete`, `CronList` | C | déclencheurs du PO |
| `RemoteTrigger` | C | déclencheurs du PO |
| `PushNotification` | E | notification du produit Claude |
| `DesignSync` | E | produit Claude (design) |

### Outils que Claude Code a en général et qui ne figuraient PAS dans cette session

| Outil | Classe | Note |
|---|---|---|
| `Glob` | **A** (N19) | cette session passe par `Bash` ; ajouté par exhaustivité, **à confirmer** par un relevé |
| `Grep` | **A** (N19) | idem ; sémantique de ripgrep |
| `TodoWrite` | C | tâches et étapes du PO |
| `MultiEdit` | **A** ou refusé | non présent ici ; à relever |

## 3. Ce que « équivalence » veut dire, outil par outil

Relevé dans les schémas et les descriptions de la session. **Ce n'est pas encore une preuve de
comportement** : la preuve, c'est le banc de N25.

### Read

- `file_path` absolu ; `offset` et `limit` pour de gros fichiers ; `pages` pour un PDF (plage, au
  plus 20 pages par appel ; obligatoire au-delà de 10 pages).
- Jusqu'à 2000 lignes par défaut ; lignes numérotées au format `cat -n` ; les lignes de plus de
  2000 caractères sont tronquées.
- Lit aussi les images (rendues en blocs image), les PDF et les notebooks (cellules et sorties).
- Un fichier vide produit un avertissement, pas un contenu vide silencieux.

### Write

- `file_path` absolu, `content`. Écraser un fichier existant exige de l'avoir **lu** dans la
  session (sinon l'outil échoue).

### Edit

- `file_path`, `old_string`, `new_string`, `replace_all` (faux par défaut).
- Exige une lecture préalable du fichier dans la session.
- `old_string` doit être **unique** dans le fichier, sauf `replace_all` ; remplacement identique
  refusé ; le préfixe de numéro de ligne de la sortie de `Read` n'appartient pas au contenu.

### Bash

- `command`, `description`, `timeout` en millisecondes (120 000 par défaut, 600 000 au plus),
  `run_in_background`, `dangerouslyDisableSandbox`.
- Le répertoire de travail persiste d'un appel à l'autre ; l'état du shell ne persiste pas.
- Sortie tronquée au-delà d'un plafond ; une tâche d'arrière-plan notifie sa fin.

### WebSearch

- `query` (2 caractères au moins), `mode` obligatoire (`standard` ou `extended`),
  `allowed_domains`, `blocked_domains`.
- Rend des blocs de résultats avec titres et URL ; la description impose de terminer la réponse par
  une liste « Sources » avec des liens. Service limité aux États-Unis chez l'éditeur.

### Monitor

- `command`, `description`, `timeout_ms` (300 000 par défaut ; plafonné à 1 800 000), ou `ws`
  (`url`, `protocols`) ; chaque ligne de sortie est un événement ; la sortie sur `stderr` ne notifie
  pas.

### À relever

`WebFetch`, `NotebookEdit`, `TaskStop`, `Glob`, `Grep`, `EnterWorktree` / `ExitWorktree` :
schémas absents de la session. Étape 2 de N17, par exécution du vrai `claude` sur des fichiers de
test, ou par N25.

## 4. Politique de dépendances

Tout se compile avec `cargo`.

- **Permis** : des crates Rust (`regex`, `ignore`, `globset`, `grep-searcher`, `serde`, `reqwest` en
  `rustls` déjà optionnel dans le crate, un convertisseur HTML vers Markdown en Rust pur choisi par
  mesure).
- **Interdit** : Node, Python, un navigateur ou une bibliothèque C ou C++ à installer ou à compiler ;
  l'appel du binaire `rg` pour `Grep`.
- **Vérification automatique** (étape 3 de N17) : refuser dans `nexus-tools` un `build.rs` externe et
  une crate `-sys` qui n'est pas listée.

### Le navigateur : Obscura

Mesuré le 2026-10-05 (lecture du dépôt, non reproduite) : Rust, Apache-2.0, v0.2.4, mais son moteur
JavaScript est **V8 via `deno_core`**, compilé depuis les sources (C++) ; la variante furtive exige
CMake et Clang ; sa bibliothèque n'est pas sur crates.io ; sa documentation de sécurité dit qu'il ne
contient pas une page hostile qui exploiterait V8. **Décision proposée** : ne pas l'embarquer, **l'attacher**
comme serveur MCP externe optionnel (`obscura mcp`), sans dépendance de build chez nous, mode furtif
désactivé, réseau privé interdit, outils d'interaction soumis à approbation. Voir N23 ; **à valider
par l'utilisateur**.

## 5. Ce que ce document ne règle pas

- Le **coût de contexte** d'un grand catalogue d'outils : `ToolSearch` existe chez Claude Code pour
  cette raison. Une dizaine d'outils tient sans lui ; au-delà, le chargement différé devient une
  question de conception, pas de confort.
- La **parité d'un comportement** : seul le banc de N25, qui enregistre le vrai Claude Code une fois
  (avec l'accord de l'utilisateur, car cela consomme des jetons) puis rejoue hors ligne, la prouve.
- `WebFetch` applique chez Claude Code une consigne d'extraction avec un petit modèle. Ici l'outil
  rend le Markdown et la consigne est appliquée par le modèle de la session, ou par un modèle de
  résumé **optionnel** lié par `ModelBinding` ; jamais par un appel de modèle caché.
