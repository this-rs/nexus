# Baseline de couverture honnête (nexus)

Mesuré le 2026-10-01 sur `origin/main` @ 3abe13e, avec la commande de la CI (`cargo llvm-cov --all-features`, workspace entier). Tous les tests passent (0 échec).

| Crate | Fichiers | Lignes | Couvertes | % brut | Lignes gated | % gated |
|---|---|---|---|---|---|---|
| `claude-code-sdk-rs` | 23 | 10 081 | 6 946 | **68,9 %** | 9 976 | **69,1 %** |
| `claude-code-api` | 41 | 5 659 | 2 215 | **39,1 %** | 5 659 | **39,1 %** |
| **Total** | 64 | 15 740 | 9 161 | **58,2 %** | 15 635 | **58,2 %** |

« gated » = lcov privé des fichiers de la section `ignore:` de `codecov.yml` (`examples/**`, `tests/**`, `**/test*.rs`, `**/mod.rs`). Les exclusions sont minces ici : seuls 2 fichiers (105 lignes) sont ignorés, `src/bin/test_interactive.rs` (0 %) et `claude-code-sdk-rs/src/transport/mod.rs` (80,9 %). Le vrai risque est ailleurs : les gates project et patch sont en `informational: true`, donc rien n'échoue jamais.

Constats :

- `claude-code-api` est à 39 % : 25 fichiers sont à 0 % dont `api/chat.rs` (444 lignes), `core/claude_manager.rs` (266), `core/process_pool.rs` (152), `core/cache.rs` (116), `core/model_registry.rs` (115), `main.rs` (102).
- Gros fichiers faibles du SDK : `transport/subprocess.rs` 998 lignes à 23,9 %, `client.rs` 485 à 24,3 %, `optimized_client.rs` 322 à 18,3 %, `cli_download.rs` 280 à 18,9 %.
- Les fichiers `neo4j_*` du crate api (hooks, storage) sont à 0 à 23 % : ils ont besoin d'un Neo4j vivant, non provisionné dans ce job CI.

## Commandes exactes

```bash
export PATH=$HOME/.cargo/bin:$PATH
git worktree add --detach /chemin/wt-cov-nx origin/main && cd /chemin/wt-cov-nx
export CARGO_TARGET_DIR=$PWD/../target-cov-nx   # un target dir par worktree
cargo llvm-cov --all-features --lcov --output-path ../lcov-nx.info

# ignores de codecov.yml -> fichier, puis agrégation brut / gated par crate et par fichier
python3 - <<'PY'
import re
y=open('codecov.yml').read().split('ignore:')[1]
open('../ignores.txt','w').write('\n'.join(re.findall(r'^\s*-\s*"([^"]+)"',y,re.M))+'\n')
PY
python3 scripts/coverage_by_module.py ../lcov-nx.info "$PWD" ../ignores.txt          # par crate
python3 scripts/coverage_by_module.py ../lcov-nx.info "$PWD" ../ignores.txt files    # par fichier
```

Durée : environ 3 min.
