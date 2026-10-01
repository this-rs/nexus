# Diagram charter

The diagrams in this repository are the documentation of record for what the
code does. They are plain Mermaid files, `docs/diagrams/<name>.mmd`, rendered
by GitHub, reviewed like code, and changed in the same pull request as the code
they describe. No service, no account, no network: everything here is checked
from a clone.

The rules below are **enforced**, not advised, by
[`claude-code-api/tests/diagram_index.rs`](../claude-code-api/tests/diagram_index.rs).
That test is the authority — this page explains it, so if the two ever disagree,
the test is right and this page is a bug.

```bash
cargo test --test diagram_index      # the whole charter, offline
```

- Index: [`docs/diagrams/INDEX.yml`](diagrams/INDEX.yml)
- Files no diagram owns: [`docs/diagrams/ORPHANS.md`](diagrams/ORPHANS.md)

## 1. Naming

| Kind | Form | Example |
|---|---|---|
| Design of a feature or subsystem | `<subject>-<aspect>` | `nexus-model-catalogue` |
| Architecture cartography | `po-<domain>` | `po-bugs` |

kebab-case, ASCII, one subject per diagram. The file name **is** the diagram
name: `docs/diagrams/nexus-model-catalogue.mmd`.

## 2. The header is mandatory

The first three lines of every `.mmd`:

```
%% name: nexus-model-catalogue
%% covers: nexus:claude-code-api/src/core/model_registry.rs nexus:README.md
%% verified: 9540068
```

- `name` — identical to the file name and to the index entry.
- `covers` — space-separated globs, each prefixed with its repository
  (`nexus:` here). Must match the entry's `covers` in `INDEX.yml` exactly; the
  test compares them.
- `verified` — the **short sha** of the commit whose code the diagram was read
  against. Not a date: a date cannot be checked out. Update it whenever the
  diagram changes.

Further `%%` lines carry what the diagram proves, the sources read, and what is
not established. A blank comment line is `%% ` — with the space, because bare
`%%` breaks some Mermaid parsers.

## 3. Every node carries a status mark

| Mark | Meaning | The label must also say |
|---|---|---|
| ✅ | written and verified | **how**: fake transport, real service, test replayed without the fix |
| 🟠 | decided, not written | which task will write it |
| 🔴 | bug, or written but never wired up | the reason, and the `bug` task if there is one |
| ⚪ | upstream: exists outside this repository | which repo or system |

A node with no mark is a review failure, and the test fails the build on it. A
✅ with no named means of verification is not a ✅.

## 4. Function names, never line numbers

Cite `query_models`, `ModelRegistry::refresh`, `spawn_cli` — not
`model_registry.rs:142`. A line number is wrong at the next neighbouring
commit; a function name is wrong only when the function really changes, and
`grep` finds it. The test rejects any label that looks like `<file>:<line>`.

## 5. Only a `verified` entry owns anything

An index entry is `planned` or `verified`.

- `planned` — no file yet. The content has not been read against the code, so
  it is not exported. The entry **owns nothing**: its `covers` reserve a
  perimeter, they do not cover it.
- `verified` — `docs/diagrams/<name>.mmd` exists, its header is valid, and its
  content was read against the code at `verified`.

This distinction is the one that keeps the numbers honest. Counting a
`planned` entry's globs as ownership would lower the orphan count and silence
the drift check **without a single diagram being written** — the index would be
buying credit for intentions. Only `verified` counts, in this index and in the
main repository's.

## 6. One file, one owner

Two `covers` must never match the same file. A file owned by two diagrams has
no owner: when it changes, the gate cannot tell which diagram to require, and
each diagram can assume the other covers it.

The rule holds **between** repositories too. This index owns the diagrams whose
`.mmd` lives here; the main repository's index owns the `po-*` cartography
cards. Its checker reads this index, treats the files it owns as owned, and
fails on any claim both indexes make. An index only ever owns paths in its own
repository — a `frontend:` glob here would be a claim on a repository that
cannot see it.

## 7. Drift: the diagram changes in the same pull request

When a pull request touches a file some `verified` diagram covers, it updates
that diagram and its `verified` sha. If the change genuinely does not affect
what the diagram says, the commit says so in writing:

```
Diagram-Unchanged: nexus-model-catalogue — adds a test, no behaviour described by the diagram changes
```

The gate reads the pull request's changed files and commit trailers from the
environment, so it needs no network and no git subprocess. Outside a pull
request it reports instead of failing; a gate nobody can satisfy gets switched
off, and a gate that is switched off teaches nothing.

Files that no `verified` diagram owns are **reported, never failed**, and
listed in [`ORPHANS.md`](diagrams/ORPHANS.md). That ceiling is meant to fall by
writing diagrams, never by shrinking what counts as source.

## 8. A bug is a task, a note and a red node

1. a task tagged `bug`, with the regression test that fails without the fix;
2. a `gotcha` note tied to that task and to the files concerned;
3. a 🔴 node in the domain's diagram, carrying the reason.

When the fix lands, the node becomes ✅ and its label says *replayed without the
fix*. The registry itself is [`docs/BUGS.md`](BUGS.md) and
[`po-bugs.mmd`](diagrams/po-bugs.mmd), kept in step by
`claude-code-api/tests/bug_registry.rs`.

## 9. Life cycle

1. **Create** — read the code, name the functions, mark every node. Add the
   `.mmd`, set the entry to `verified` with its `file:` and `verified:` sha.
2. **Update** — in the **same** pull request as the code, refreshing
   `verified` (and `covers` in both the file and the index if the perimeter
   moved).
3. **Archive** — when the covered code goes, the `.mmd` and its entry go in the
   pull request that removes the code.

A pull request is one vertical slice: fix, regression test that fails without
the fix, diagram updated, documentation. Never a diagrams-only pull request
describing code nobody verified — that is how a wrong instruction gets written
down and then cited.

## 10. Why a Rust test and not a script

This is a pure Rust workspace. A Node checker would mean a `package.json`, a
lockfile and `actions/setup-node` in a repository that has none. As an
integration test the gate runs inside the existing CI `test` job, on all three
operating systems, with no new dependency — and `cargo test` is what a
contributor already runs.
