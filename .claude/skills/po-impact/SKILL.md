---
name: po-impact
description: Analyse blast radius before modifying code with the PO code graph - impact, references, co-changers, risk, topology. Use before editing a shared function/trait/type, renaming, changing a public signature in claude-code-sdk-rs or claude-code-api, or touching a hotspot like subprocess.rs. Triggers - "est-ce que ça casse", "qui appelle", "refactor", "impact".
---

# po-impact — mesurer avant de couper

## Séquence (project_slug = `nexus-sdk`)
1. `code(action:"get_file_symbols", file_path:"<abs>")` — ce que le fichier expose.
2. `code(action:"find_references", symbol:"<nom>")` — appelants directs.
3. `code(action:"analyze_impact", target:"<chemin ou symbole>", target_type:"file"|"function", multi:true, project_slug?)` — rayon d'impact ; inclut les décisions AFFECTS sur les fichiers touchés.
4. `code(action:"get_file_co_changers", file_path, min_count:3)` — fichiers qui bougent toujours ensemble (couplage invisible à l'import).
5. `code(action:"get_risk_assessment", project_slug)` / `code(action:"get_hotspots", project_slug)` pour les fichiers à fort churn. `subprocess.rs` est le hotspot n°1.
6. Trait : `code(action:"find_trait_implementations", trait_name)` ; type : `code(action:"find_type_traits", type_name)`.
7. Puis `po-warmup` étape 3 (notes et décisions ancrées sur ces fichiers).

## Pièges
- Le graphe est celui du **dernier sync**. Après des commits : `project(action:"sync", slug:"nexus-sdk")` ou `commit(action:"create", … project_id)`. Un graphe périmé donne une assurance fausse.
- `find_references` est syntaxique (Tree-sitter) : les appels via objet-trait `dyn`, macros et `serde` peuvent manquer. Recouper avec `grep -rn`.
- `analyze_impact` ne voit pas les contrats de comportement (ordre des messages, fail-open des gardes de permission). Les notes `gotcha` les portent.
- Modifier la signature d'un `Transport` ou du client ? Le diagramme propriétaire doit suivre : `po-diagram-drift`.
- Un nombre de références nul n'est pas une preuve d'absence d'appelants si le dépôt a été indexé avant un déplacement de fichier.
