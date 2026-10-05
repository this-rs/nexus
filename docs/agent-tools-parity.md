# Outils livrés avec nexus : matrice de parité avec Claude Code (N17)

**Statut : brouillon du 2026-10-05, corrigé le même jour par un relevé sur le vrai CLI.** Sources :
(1) la liste d'outils de la session Claude Code 2.1.287 qui a produit ce document (schémas lus dans la
session) ; (2) le message d'initialisation du vrai `claude -p` 2.1.287, qui liste ses outils (§2.1).
Là où le schéma d'un outil n'était pas visible, la case dit **« à relever »** : elle se remplit par
l'étape 2 de N17 ou par le banc de N25, jamais par supposition.

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

## 2.1 Relevé réel : l'initialisation de `claude -p` 2.1.287

`claude -p … --output-format stream-json --verbose` émet un message `system/init` qui liste les
outils. Relevé le 2026-10-05 (modèle `claude-haiku-4-5`, un tour, 0,022 $) : **40 entrées**.

`Task`, `Artifact`, `ArtifactComments`, `ArtifactData`, `Bash`, `CronCreate`, `CronDelete`,
`CronList`, `DesignSync`, `Edit`, `EnterWorktree`, `ExitWorktree`, `ListAgents`, `LSP`, `Monitor`,
`NotebookEdit`, `PushNotification`, `Read`, `RemoteTrigger`, `ReportFindings`, `ScheduleWakeup`,
`SendMessage`, `Skill`, `TaskCreate`, `TaskGet`, `TaskList`, `TaskStop`, `TaskUpdate`, `ToolSearch`,
`WebSearch`, `Workflow`, `Write`, et huit outils `mcp__claude_ai_Claude_Docs__*`.

Ce que ce relevé corrige dans l'inventaire écrit de mémoire :

- **`Glob` et `Grep` n'existent PAS dans Claude Code 2.1.287** : il passe par `Bash`. Ils restent
  dans notre plan (N19) comme **extras** utiles à un harnais qui n'a pas de shell, avec la
  sémantique de ripgrep et de `.gitignore`, **sans référence Claude Code à égaler**.
- L'outil de sous-agent s'appelle **`Task`** dans cette version (il s'appelait `Agent` dans la session
  qui a servi à écrire la première version de ce document : le nom change d'une version à l'autre).
- **`LSP`** existe : il parle à des serveurs de langage externes (rust-analyzer…), donc une
  dépendance d'exécution non Rust. Classe D, hors du premier périmètre.
- **`TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate`** sont le suivi de tâches de Claude Code
  (successeur de `TodoWrite`) : classe C, couverts par les tâches et étapes du PO.
- **`WebFetch`, `AskUserQuestion`, `EnterPlanMode`, `ExitPlanMode` n'apparaissent pas** dans cette
  initialisation non interactive : ils dépendent du mode (interactif ou `-p`) et des outils chargés
  à la demande. Ils figuraient dans la liste de la session interactive : on les garde, en notant que
  leur disponibilité dépend du mode.
- Coût d'un tour trivial : environ 32 000 jetons de contexte (9 258 écrits en cache, 22 376 lus),
  soit 0,022 $ avec le modèle le moins cher. Le prix d'un enregistrement de parité (N25) se calcule
  sur cette base.

## 2. Inventaire

### Outils chargés d'emblée dans la session

| Outil | Classe | Raison, ou ce que le contrat couvre déjà |
|---|---|---|
| `Read` | **A** (N19) | fichiers, images, PDF, notebooks |
| `Write` | **A** (N19) | |
| `Edit` | **A** (N19) | |
| `Bash` | **A** (N20) | |
| `AskUserQuestion` | B | événement `question` et capacité `native_question` |
| `Agent` (nommé `Task` dans 2.1.287) | B / D | capacité `subagents` ; la délégation parallèle est la classe D |
| `Skill` | B | chargement d'instructions : à porter au niveau du harnais, pas en outil MCP |
| `ToolSearch` | B | chargement différé d'outils : utile dès que le catalogue grossit (coût de contexte), voir §5 |
| `ScheduleWakeup` | C | déclencheurs de plans du PO |
| `Workflow` | D | orchestration multi-agents |
| `ListAgents`, `SendMessage` | C | communication entre sessions : MCP du PO |
| `TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate` | C | suivi de tâches : tâches et étapes du PO |
| `LSP` | D | serveurs de langage externes : dépendance d'exécution non Rust |
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
| `Glob` | **A, extra** (N19) | **absent de 2.1.287** (relevé §2.1) : Claude Code passe par `Bash`. Extra de nexus, sémantique de `.gitignore` et du motif ; aucune référence Claude Code à égaler |
| `Grep` | **A, extra** (N19) | idem ; sémantique de ripgrep |
| `TodoWrite` | C | remplacé par `TaskCreate` et suivants dans 2.1.287 ; tâches et étapes du PO |
| `MultiEdit` | non | absent de 2.1.287 |

## 3. Ce que « équivalence » veut dire, outil par outil

**Relevé réel** : `claude -p` 2.1.287, modèle `claude-haiku-4-5`, sur des fichiers synthétiques, résultats
d'outils lus dans le flux `stream-json`. Les enregistrements normalisés sont sous
`claude-code-sdk-rs/tests/parity/claude-code-2.1.287/` (README : comment les reproduire). **Là où le
relevé contredit la description de l'outil, c'est le relevé qui fait foi** : on égale ce que fait le
vrai Claude Code, pas ce qu'il dit faire.

### Read

- Sortie : une ligne `N<TAB>contenu` par ligne (format `cat -n`). Un fichier qui finit par un retour à
  la ligne rend une dernière ligne numérotée **vide** (`13<TAB>` pour un fichier de 12 lignes).
- `offset` est le numéro de la première ligne rendue (1 = la première). `offset: 0` est pris
  littéralement et **décale la numérotation** (0, 1, 2). `limit` coupe sans ligne vide finale. Un
  `offset` au-delà de la fin n'est pas une erreur : `<system-reminder>Warning: the file exists but is
  shorter than the provided offset (5000). The file has 3001 lines.</system-reminder>`.
- Un chemin **relatif** est accepté (résolu depuis le répertoire de travail), bien que la description
  exige un chemin absolu.
- Fichier absent : erreur `File does not exist. Note: your current working directory is <cwd>.` ;
  dossier : erreur `EISDIR: illegal operation on a directory, read '<chemin>'` ; fichier vide : **pas**
  une erreur, `<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>`.
- **Pas de plafond de 2000 lignes** (un fichier de 3000 lignes est rendu en entier, 3001 lignes
  numérotées) et **pas de troncature des lignes longues à 2000 caractères** (lignes de 2 500 et de
  60 000 caractères rendues entières), contrairement à la description de l'outil. Le plafond réel est
  **en jetons** (de l'ordre de 25 000) : un fichier de 3 300 lignes est coupé à 1 250 lignes
  (56 392 caractères), à une frontière de ligne, **silencieusement** (aucune mention de coupe). Un
  tokenizer n'étant pas disponible en Rust pur, N19 devra approcher ce plafond et le dire.
- Notebooks : `<cell id="c1">source</cell id="c1">` ; une cellule markdown ajoute
  `<cell_type>markdown</cell_type>` avant sa source.

### Write et Edit

- Les échecs sont `is_error` avec le corps `<tool_use_error>…</tool_use_error>`.
- **État « lu » par session** : `Edit` ou `Write` d'un fichier existant non lu échoue avec
  `File has not been read yet. Read it first before writing to it.` ; une lecture suffit ensuite. `Write`
  d'un **nouveau** fichier n'exige aucune lecture et crée les dossiers parents.
- `Edit` — non unique : `Found 3 matches of the string to replace, but replace_all is false. To replace
  all occurrences, set replace_all to true. To replace only one occurrence, please provide more context
  to uniquely identify the instance.\nString: alpha` ; identique : `No changes to make: old_string and
  new_string are exactly the same.` ; absent : `String to replace not found in file.\nString: zzz`.
- **Aucun retrait automatique du préfixe de numéro de ligne** : un `old_string` écrit
  `3<TAB>line 3` est introuvable.
- Succès : `The file <chemin> has been updated successfully. (file state is current in your context — no
  need to Read it back)` ; avec `replace_all` : `The file <chemin> has been updated. All occurrences were
  successfully replaced. (file state is current …)` ; création : `File created successfully at:
  <chemin> (file state is current …)`.
- Un échec ne modifie pas le fichier (vérifié sur les fichiers finals).

### Bash

- Sortie standard et d'erreur **fusionnées** (`out\nerr`). Aucune sortie : `(Bash completed with no
  output)`. Code de sortie non nul : `is_error` et `Exit code 3` (suivi de la sortie s'il y en a) ; le
  code retenu est celui de la **dernière** commande (`echo a; false; echo b` n'est pas une erreur).
- Dépassement de durée : `Exit code 143\nCommand timed out after 1s` (SIGTERM). Une `timeout` énorme
  (99 999 999) est acceptée sans erreur ni plafonnement signalé.
- **Sortie persistée au-delà de 30 000 caractères** : 29 000 caractères passent en ligne ; 31 000 donnent
  `<persisted-output>\nOutput too large (30.3KB). Full output saved to: <chemin>\n\nPreview (first
  2KB):\n…\n...\n</persisted-output>` avec un aperçu de 2 Ko.
- Le **répertoire de travail persiste** d'un appel à l'autre (`cd sub` puis `pwd` rend `<…>/sub`).
- Arrière-plan : `Command running in background with ID: <id>. Output is being written to: <chemin>.
  You will be notified when it completes. To check interim output, use Read on that file path.`
  Pas d'outil de lecture de sortie dédié dans 2.1.287 : on lit le fichier avec `Read`.
- Le shell est celui de l'utilisateur (`$0` = `/bin/zsh` sur la machine de relevé).

### NotebookEdit

- Même règle « lu d'abord » que `Edit`. Remplacement : `Updated cell c1 with <source>` ; insertion
  (**après** la cellule `cell_id`) : `Inserted cell <id aléatoire de 8 hex> with <source>` ;
  suppression : `Deleted cell c2` ; cellule inconnue : `<tool_use_error>Cell with ID "nope" not found in
  notebook.</tool_use_error>`.
- Le fichier est réécrit en JSON indenté d'un espace, la `source` stockée **comme une chaîne** (pas une
  liste), les clés d'une cellule insérée dans l'ordre `cell_type`, `id`, `source`, `metadata`,
  `execution_count`, `outputs`.

### Non relevé, et pourquoi

- **`WebFetch`** : absent de `claude -p` 2.1.287 (aucun appel possible, même avec `--tools WebFetch`).
  N21 ne peut pas s'appuyer sur un enregistrement ; le comportement attendu vient de la description de
  l'outil et reste à confirmer en session interactive.
- `Monitor`, `TaskStop`, `EnterWorktree` / `ExitWorktree` : non essayés (processus longs ou état de
  dépôt). `Glob` et `Grep` : n'existent pas (voir §2.1).

Coût total des enregistrements : 0,42 dollar (modèle `haiku`, 36 appels d'outils en 6 sessions) ; le plus cher
(0,17 dollar) est la lecture de très gros fichiers.

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

## 5. État de la livraison (N19 terminé, N20 livré)

Livrés dans `nexus-tools` (module `files`, tests `tests/files.rs`) : `Read` (texte, notebooks), `Write`,
`Edit`, état « lu » par session, périmètre (`--cwd`, `--add-dir`, liens symboliques et `..` résolus
composant par composant), écriture atomique (fichier temporaire voisin, `fsync`, `rename`), sauvegarde
optionnelle (`--backup-dir`). Mutations jouées : état « lu » ignoré, symlinks non suivis, écriture non
atomique — chacune fait échouer des tests.

Écarts **volontaires** avec le vrai Claude Code :
- Plafond de `Read` en **caractères** (56 400, calé sur le relevé : 1 250 lignes / 56 392 caractères) et
  non en jetons ; une coupe est **annoncée** (`[output truncated: … offset N …]`), là où Claude Code coupe en
  silence. Une ligne seule est toujours rendue entière : le fichier d'une ligne de 60 000 caractères perd
  donc sa dernière ligne vide, remplacée par l'annonce.
- `Write`/`Edit` refusent un fichier **modifié depuis sa lecture** (`File has been modified since read…`) :
  règle de Claude Code, non enregistrée (texte du message à confirmer par un relevé).
- Un chemin hors périmètre est refusé (`Path is outside the directories this session may access`) ; Claude
  Code demande une permission à la place.

`NotebookEdit` : un type JSON ordonné local conserve l'ordre des clés (la feature `preserve_order` de
`serde_json` est inutilisable : Cargo l'unifie sur tout le workspace et réordonne les messages de contrôle
octet pour octet du harnais). Une cellule de code remplacée voit ses `outputs` vidés et son
`execution_count` remis à `null` (non relevé).

`Glob` et `Grep` (crates `ignore`, `globset`, `regex`, sans binaire `rg`) : `claude -p` 2.1.287 n'expose pas ces
outils, donc **aucun relevé** ; la référence est un vrai ripgrep 15.2 (`tests/search.rs` compare les sorties,
goldens `tests/golden_grep/` quand `rg` est absent). Étiquettes de sortie (`Found N files`, pied du mode count,
`No files found`) d'après le comportement documenté de Claude Code, non relevées. Fichiers cachés inclus, dossiers
VCS exclus, liens symboliques non suivis (jamais d'évasion du périmètre).

Restent pour N19 : images et PDF dans `Read` (blocs image : `ToolResult` ne porte que du texte pour l'instant).

### N20 : outils shell (`shell/`, tests `tests/shell.rs` et `tests/monitor.rs`)

`Bash`, `TaskStop`, `Monitor`. Messages et seuils repris du relevé (`bash.json`, `bash_limits.json`) : fusion
des deux flux, `Exit code N`, `Command timed out after Ns` (code 143), sortie au-delà de 30 000 octets
persistée avec aperçu de 2 Ko coupé à une ligne, `(Bash completed with no output)`, répertoire persistant,
tâche d'arrière-plan avec identifiant et fichier de sortie. Groupe de processus : timeout, appel abandonné,
`TaskStop` et fin de session tuent la commande **et ses descendants** (ce que l'interruption de premier plan
du SDK actuel ne faisait pas).

Écarts **volontaires** :
- **Pas de notification de fin de tâche** : Claude Code promet « You will be notified when it completes » ; un
  serveur MCP sur stdio n'a pas ce canal pour `Bash`, le message dit donc d'utiliser `Read` sur le fichier et
  `TaskStop`. `Monitor` envoie, lui, une notification `notifications/message` par ligne (stdio seulement ; en
  HTTP, la sortie n'est lisible que dans son fichier).
- **Environnement vierge** (PATH, LANG, LC_*, TZ + ajouts explicites) et `HOME` propre au serveur : Claude Code
  hérite de l'environnement de l'utilisateur. Un `git` ou un `npm` qui lit `~/.gitconfig` ou `~/.npmrc` ne les
  verra pas : à ouvrir par `--env`/`EnvPolicy` si voulu.
- Un `cd` hors périmètre est **annulé** (`Shell cwd was reset to …`).
- Délai plafonné à 600 s sans le dire (le relevé accepte 99 999 999 sans plafond signalé).
- Unix seulement (groupes de processus) : le module est absent sous Windows.

**Bash n'est pas un bac à sable.** Le périmètre confine les outils de fichiers, pas ce qu'un shell peut lire.
La protection est la politique d'approbation du harnais (motifs `Bash(git *)`, évalués côté harnais, N24) et
l'environnement vierge. Un vrai bac à sable est une capacité distincte.

Non relevé : `Monitor` et `TaskStop` (processus longs) ; leurs formats sont les nôtres.

## 6. WebFetch (N21)

### Choix du convertisseur HTML vers Markdown : mesure

Corpus : `nexus-tools/tests/data/html/` (article avec liens relatifs et bruit, tableau, code, HTML cassé,
page entière dans un `<form>`). Contrôles : 36 assertions (structure conservée, code de la page et décor
absents, entités décodées). Mesuré le 05/10/2026 avec un binaire jetable hors dépôt, sur sortie normalisée
(espaces et tirets répétés réduits) :

| Crate | Réussi | Échecs notables |
|---|---|---|
| `htmd` 0.2 + `skip_tags` | **34/36** | liens perdus dans les cellules de tableau |
| `htmd` 0.2 sans `skip_tags` | 30/36 | texte de `<script>`/`<style>` dans la sortie |
| `mdka` 1 | 29/36 | listes ordonnées, entités dans le code, liste après élément mal fermé |
| `html2text` 0.14 | 28/36 | pas de liens ni d'images en Markdown, pas de code clôturé, tableaux en texte |
| `html2md` 0.2 | 24/36 | pas de titres `#`, tableaux perdus, `<script>` injecté dans le texte |

Retenu : **`htmd`**, avec `script style noscript iframe svg head template` ignorés. **Pas** `form` : en
l'ignorant, une page ASP.NET entière (enveloppée dans un `<form>`) disparaissait ; sonde faite et test de
non-régression `a_page_wrapped_in_a_form_is_not_lost`. Ajouts : liens et images rendus absolus (une ancre `#x` ou
une URL `javascript:` devient du texte), décodage du charset (`encoding_rs`).
Limite connue : `htmd` perd les liens à l'intérieur des cellules de tableau.

### Écarts volontaires avec Claude Code

- **Pas de petit modèle** : Claude Code applique la consigne (`prompt`) par un second modèle ; ici l'outil rend le
  Markdown et la consigne est appliquée par le modèle de la session. Aucun appel de modèle caché.
- Redirection vers un autre hôte (ou retour de https à http) : **signalée** avec l'URL cible, comme Claude Code ;
  `http` vers `https` du même hôte est suivi.
- Garde SSRF : toutes les adresses d'un nom contrôlées, connexion épinglée, chaque saut recontrôlé — plus
  strict que ce qui est documenté pour Claude Code.
- **https : pas de moteur TLS dans ce build** (décision en attente, voir ci-dessous). `http` est réécrit en
  `https` comme Claude Code, donc tout échoue pour l'instant avec `connect_failed … no TLS support`, jamais en
  clair.
- Non fait : PDF (voir N19b), contenu compressé (on envoie `Accept-Encoding: identity`).

### Décision en attente : TLS

Aucun moteur TLS n'est « compilable en Rust seul » sans réserve : `rustls` s'appuie sur `ring` (ou `aws-lc`),
qui compile du C et de l'assembleur à la construction (déjà le cas, via `reqwest`, pour le harnais natif et
`provider-native`). L'alternative pure Rust (`rustls` + fournisseur RustCrypto) est expérimentale et non auditée.
Choisir entre : (a) `rustls`+`ring` derrière une feature `tls` désactivée par défaut, exception à la politique
justifiée ; (b) fournisseur pur Rust ; (c) pas de https tant que la politique n'est pas assouplie.

## 7. WebSearch (N22)

Claude Code s'appuie sur un service de recherche de son éditeur ; il n'y en a pas ici. La recherche est donc
un **trait** (`SearchBackend`) avec plusieurs moteurs, essayés dans l'ordre configuré (`--search-engine`, répétable) :

| Moteur | Réglage | Remarque |
|---|---|---|
| API à clé (forme Brave) | `brave:NOM_DE_VARIABLE` | la clé est lue dans l'environnement du serveur ; seul le **nom** est configuré |
| SearXNG auto-hébergé | `searxng:URL` | le format JSON doit être activé dans l'instance ; adresse privée : `--search-allow-private` |
| HTML d'un moteur sans clé | `html` | **fragile**, désactivé par défaut, avertissement au démarrage, conditions du moteur à respecter |
| Navigateur MCP externe | N23 | pour les pages qui exigent du JavaScript |

Garanties : domaines autorisés/bloqués appliqués **après** le moteur ; URL équivalentes fusionnées ; repli,
disjoncteur et limitation de débit par moteur ; clé refusée ou quota épuisé = erreur typée qui ne casse pas le
disjoncteur ; mode `standard` = une requête, `extended` = plusieurs variantes (celles du modèle dans
`additional_queries`, sinon mots-clés et phrase exacte) fusionnées par rang réciproque ; résultats étiquetés
**données non fiables**. Aucun appel de modèle caché. L'outil n'est enregistré que si un moteur est configuré.

Écarts avec Claude Code : pas de synthèse par un second modèle (le modèle de la session lit les résultats) ;
pas de moteur par défaut ; https dépend de la décision TLS en attente (§6), donc pour l'instant seuls les
moteurs en `http` (SearXNG sur un réseau privé) fonctionnent de bout en bout.
