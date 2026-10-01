# Bug registry

One place to answer three questions about any bug in this workspace: **is it
real today**, **who is closing it**, and **what proves it is closed**.

The registry is the pair `docs/BUGS.md` (this file) and
`docs/diagrams/po-bugs.mmd` (the same facts as a graph). They are kept in step
by `claude-code-api/tests/bug_registry.rs`, which fails the build when the two
disagree. Nothing here is maintained by hand alone.

> **Scope.** The registry covers the whole project-orchestrator workspace —
> `backend`, `frontend` and `nexus` — because the bugs do: a route bug is a
> disagreement *between* two repositories and belongs to neither. Entries name
> the repository they live in.

---

## 1. The convention

A bug is tracked by **four linked artefacts**. Fewer than four means the bug is
not tracked, however well it is understood.

| # | Artefact | Where | Carries |
|---|----------|-------|---------|
| 1 | A PO task tagged `bug` + `<domain>` + a severity tag (`sev-high` / `sev-medium` / `sev-low`) | PO | ownership, status, the PR that closes it |
| 2 | A note of type `gotcha`, linked to that task and to the files/functions | PO | root cause and the shape of the fix |
| 3 | A node in the owning domain diagram | `docs/diagrams/<domain>.mmd` | where the bug sits in the mechanism |
| 4 | A row here + a node in `po-bugs` | this repository | the registry view: bug → domain → status |

**Severity.** `sev-high` = data loss, a security bypass, or a user-visible
feature that cannot work at all. `sev-medium` = wrong behaviour with a
workaround. `sev-low` = cosmetic or internal-only.

**Naming.** A bug id is kebab-case and describes the *defect*, not the fix:
`cypher-injection-wherebuilder`, not `add-cypher-params`. The id is stable
once published — it is the join key between the task, the note, the diagram
node and this table.

**Node statuses** (the diagram legend, identical to every other diagram here):

| Glyph | Meaning in `po-bugs` |
|-------|----------------------|
| 🔴 | **Open.** Reproducible on the repository's default branch right now. |
| 🟠 | **Fix exists but is not landed.** The correction lives on a branch or an open PR. The default branch is still broken. |
| ✅ | **Closed, with proof.** Merged into the default branch *and* the node says how that was verified. |
| ⚪ | **Upstream.** Owned by another task; not verified in this slice. |

A ✅ always states its evidence — which commit, and which test. "It looks
fixed" is a 🟠, not a ✅.

---

## 2. The cycle

```
discovery → task + gotcha → red test → PR → ✅ node with its proof
```

1. **Discovery.** Anything may surface a bug: a user report, an audit, a
   diagram that disagrees with the code. The *source* does not matter; the
   next step is the same for all of them.

2. **Verify before recording.** Read the current code on the default branch
   and reproduce. A finding copied from an older report is a **hypothesis**,
   not a bug — several entries in section 4 were already fixed by the time
   they were re-checked. Cite **function names, never line numbers**.

3. **Task + gotcha.** Open the PO task (artefact 1) and write the `gotcha`
   note (artefact 2) with the root cause. Before creating anything, search the
   existing tasks: a duplicate entry splits the evidence in two and both
   halves rot.

4. **Red test first.** Write a test that **fails on the unfixed code**, and
   record that you ran it against the unfixed code. A test that still passes
   when the fix is reverted proves nothing and does not count — not towards
   closing the bug, and not towards coverage (see
   `docs/COVERAGE_EXCLUSIONS.md` policy: honest numbers only).

5. **PR = one vertical slice.** Fix + the regression test + the updated
   diagram + the doc change, together. Never a diagram-only PR describing code
   nobody checked.

6. **Close with proof.** When the PR is **merged into the default branch**,
   flip the node to ✅ and write the evidence into it. Merged, not approved:
   the status field of a task is not evidence. Check with
   `git merge-base --is-ancestor <branch> origin/main`.

### The failure mode this registry exists to prevent

A task marked `completed` whose fix never landed. Three entries in section 4
are exactly that: the work was done, a branch exists, the task says
`completed`, and the default branch is **still vulnerable**. The registry
therefore takes its status from `git`, not from a task field — and that is why
🟠 exists as a distinct status from ✅.

---

## 3. How the registry is enforced

`claude-code-api/tests/bug_registry.rs` runs offline on every build and
asserts:

* every bug id in section 4 has a node in `po-bugs.mmd`, and vice versa;
* the status glyph in the table matches the glyph on the node;
* every id is kebab-case, unique, and carries a severity and a repository;
* every ✅ row cites evidence, and every 🟠 row names where the fix is waiting;
* `docs/diagrams/INDEX.yml` lists every `.mmd` in `docs/diagrams/`, each entry
  points at a file that exists, and each file's `%% name:` header matches its
  index entry.

So the table below cannot drift from the diagram, and neither can drift from
the set of files on disk. The charter rules themselves — header shape, a
status mark on every node, no line numbers — are enforced separately by
`claude-code-api/tests/diagram_index.rs`, which also fails a pull request that
touches a covered file without touching its diagram.

**The gate was replayed without each guarantee**, because a test that still
passes when you remove what it guards proves nothing:

| mutation applied to a green tree | result |
|---|---|
| flip `cypher-injection-wherebuilder` from 🟠 to ✅ in the table only | `the_registry_and_the_diagram_agree` fails |
| add a table row with no matching diagram node | `the_registry_and_the_diagram_agree` fails |
| drop an unindexed `.mmd` into `docs/diagrams/` | `the_index_lists_every_diagram_and_every_listed_diagram_exists` fails |
| replace a ✅ row's commit citation with "it seems fine now" | `every_closed_bug_cites_its_proof_and_every_open_one_says_where_the_fix_is` fails |

All four were restored and the suite is green again (18 tests), alongside the
charter gate's 39.

---

## 4. Inventory — verified 2026-10-01 against `origin/main`

The eleven findings of the 2026-09-30 report were re-checked against the code
rather than believed, and they resolved into **thirteen** of the entries
below: two reports split in half, because one half had landed and the other
had not. Four more cover this repository, bringing the table to **seventeen**.

* **Eight are closed** ✅ — one of them by deleting the caller rather than by
  adding the route it called.
* **Three are live** 🟠/🔴 with a finished fix sitting on an **unmerged**
  branch. These are the dangerous ones; see the note after the table.
* **Two are new** 🔴 — found only because the re-check read the code. They had
  no owner before this slice and have one now.

Four further entries cover `nexus` itself (PO task `1b4d27f2`), verified in
this repository on the same day. They are listed because a registry that
skipped the repository it lives in would be the least credible document in the
workspace.

One report was also inaccurate in its wording; the corrections are listed
below the table rather than quietly fixed.

<!-- BUG-TABLE-START -->
| id | repo | severity | status | finding | evidence / where the fix is |
|----|------|----------|--------|---------|------------------------------|
| `route-retry-task-missing` | backend | sev-high | ✅ | `runnerApi.retryTask` called `/plans/{id}/run/tasks/{taskId}/retry` with no backend route. | Route registered to `retry_plan_task`; fixed by backend `11da6159` (#468). Locked by frontend `apiContract.test.ts`. |
| `route-progress-batch-missing` | backend | sev-medium | ✅ | `progressApi.getBatch` called `/api/progress` with no backend route. | Route registered to `get_progress_batch`; fixed by backend `621765cc` (#452). |
| `route-graph-neighborhood-missing` | backend | sev-medium | ✅ | `neighborhoodApi.get` called `/api/graph/neighborhood` with no backend route. | Route registered to `graph_handlers::get_neighborhood`; fixed by backend `f863694f` (#449). |
| `route-protocol-run-history-missing` | frontend | sev-low | ✅ | `protocolApi.getRunHistory` called `/protocols/runs/{id}/history`, which never existed server-side. | Closed by **removing the dead caller**, frontend `ab15de4` (#180). The route is still absent by design. |
| `protocol-trigger-event-field-mismatch` | frontend | sev-high | ✅ | `protocolApi.triggerEvent` posted `{event}` while `FireTransitionBody` deserializes `{trigger}` — every transition failed. | Now posts `{trigger}`; fixed by frontend `ab15de4` (#180). Regression test asserts the body in `apiContract.test.ts`. |
| `cypher-injection-wherebuilder` | backend | sev-high | 🟠 | `WhereBuilder` interpolates `assigned_to`, `tags` and `search` straight into Cypher with naked quotes; `conditions` is a `Vec<String>` with no parameter map. | Fix on **unmerged** branch `fix/inject-escape` (`dfa21feb`). Branch is based on a stale main and is not a clean cherry-pick. PO task `efbb040e` says `completed` — it is not merged. |
| `auth-anonymous-without-config` | backend | sev-high | 🔴 | `require_auth` injects `Claims::anonymous()` and returns early when `auth_config` is `None`, bypassing the JWT, email-policy, MCP-revocation and vault-prefix checks below it. Documented as intentional, but silent at runtime. | Fix on **unmerged** branch `fix/auth-hardening` (`8ae6163f`). Awaiting the product decision on PO task `9f873bd5` (blocked): is open local mode wanted, and only on loopback? |
| `tombstone-placeholder-signature` | backend | sev-high | 🟠 | `retract_sharing` builds `SignedTombstone` with `signature_hex: "0".repeat(128)` and persists/broadcasts it, so a forged tombstone is indistinguishable from a real one. Issuer degrades to `did:local:unknown`. | Fix on **unmerged** branch `fix/tombstone-placeholder` (`46425fdc`, `b16048ab`, `25e496fd`). PO task `48b57611` says `completed` — it is not merged. Exposure is limited: no `api/p2p` route is wired. |
| `workspace-milestone-update-panic` | backend | sev-medium | ✅ | `update_workspace_milestone` ended in `.unwrap()` and panicked on an unknown id. | Returns `AppError::NotFound` twice; `neo4j::workspace` returns `Result<bool>` via `run_matched`, and the runner no longer emits a phantom `Updated` event. Fixed by backend `dde8bfbf` (#465). |
| `chat-input-request-never-emitted` | backend | sev-low | ✅ | `ChatEvent::InputRequest` was declared but only ever constructed in `#[cfg(test)]` modules since `217564e` — no production emitter ever existed. | Variant deleted outright; `ChatEvent` now goes `PermissionRequest` → `AskUserQuestion` → `Result`. Fixed by backend `dde8bfbf` (#465). |
| `chat-input-request-frontend-dead-listeners` | frontend | sev-low | 🔴 | The same commit left the consumers: `InputRequestBlock`, `useChat`, `chatAssembly`, `chatExport` and `types/chat.ts` still handle an event no producer emits — dead code that reads as a live feature. | **New**, no prior owner. PO task `96c8c56c` created by this slice. Decision in the PR: delete, or rewire onto `AskUserQuestion`. |
| `task-status-debug-in-api-payloads` | backend | sev-high | ✅ | `WaveTask.status` and `DependencyGraphNode.status` were `String`s built with `format!("{:?}")`, shipping `InProgress` where every other payload says `in_progress`; no frontend comparison matched. | Both fields typed `TaskStatus` so serde owns the wire form. Fixed by backend `03f78569` (#466), locked by `test_dependency_graph_node_enriched_serialization`. |
| `task-status-debug-in-ws-and-compaction` | backend | sev-medium | 🔴 | Two user-visible sites still Debug-format a `TaskStatus`, which has `rename_all = "snake_case"` and no `Display`. `update_task` emits a `CrudEvent` carrying `"Failed"` to the socket; `fetch_active_plans_and_tasks` yields `"inprogress"`, printed into the compaction prompt as `🔄 INPROGRESS`. | **New**, no branch fixes it. PO task `73300ba4` created by this slice. Load-bearing: `on_task_completed_cascade_steps` compares the Debug casing, so both sides must change together. |
| `image-url-ssrf` | nexus | sev-high | ✅ | `download_image` passed a client-supplied `http(s)` URL straight to `reqwest::get` with no scheme or host allowlist, so `http://169.254.169.254/latest/meta-data/` was reachable from the server. | `refuse_unless_public` resolves the host and refuses unless **every** address is publicly routable, before any request leaves the process. Proved by `download_image_consults_the_guard_before_fetching`, which fails when the call site alone is reverted. Residual: DNS rebinding, see below. |
| `image-url-local-file-read` | nexus | sev-high | ✅ | `process_image_url`'s final branch was `else { Ok(url.to_string()) }`: anything neither `data:image/` nor `http(s)://` was returned **as a local path** and injected into the prompt as `Image: {path}`. | The branch now returns `BadRequest`. Proved by `an_unrecognised_image_url_is_refused_rather_than_read_as_a_path` over `/etc/passwd`, `file://`, `~/.ssh/id_rsa` and a Windows path; it fails when that branch alone is reverted. |
| `mcp-secrets-in-logs` | nexus | sev-high | ✅ | `Debug for Command` prints every argument, so `info!("… with command: {:?}", cmd)` wrote the `--mcp-config` payload — which routinely carries tokens — verbatim. Four sites, one of them in the SDK's own `query.rs`. | All four now call `describe_command_redacted`, exported from the SDK with `SECRET_BEARING_ARGS` so the gateway reuses it instead of rewriting it. Pinned by `claude-code-api/tests/command_redaction.rs`, whose `debug_formatting_is_what_leaked_and_still_would` keeps the defect itself under test. Landed in `85ab6fc`. |
| `api-auth-never-wired` | nexus | sev-high | 🔴 | `AuthManager` and `auth_middleware` are referenced nowhere outside `core/auth.rs`; `create_app` layers only `add_request_id`, `handle_errors` and CORS. Setting `auth.enabled = true` therefore does nothing and the gateway serves everything anonymously. | **New to this registry.** PO task `1b4d27f2`. Same class as the backend's `auth-anonymous-without-config`, but here there is not even a documented intent — the switch simply lies. |
<!-- BUG-TABLE-END -->

### Residual risk left open by the `image-url-ssrf` fix

`refuse_unless_public` resolves the host, then `reqwest` resolves it again to
connect. A name that answers differently between those two lookups — DNS
rebinding — still reaches a private address. Closing it means pinning the
connection to the address that was checked, which is a custom `reqwest`
connector and a larger change than this slice. It is written here rather than
left implied: the fix removes the trivial attack (a literal metadata address,
a loopback URL) and narrows, but does not seal, the general one.

### Corrections to the nexus security report (task `1b4d27f2`)

Verified in this repository; two details did not survive the re-check, and
one finding is wider than reported.

* The report names `build_router` and a test
  `test_app_serves_the_production_route_table`. **Neither exists on this
  branch** — the router is built by `create_app` in `claude-code-api/src/main.rs`,
  and no such test is present. The conclusion stands (auth is unwired) but the
  evidence had to be re-derived, which is why the row cites `create_app`.
* The secrets-in-logs finding lists three sites in `claude-code-api`. There
  was a **fourth**, in `claude-code-sdk-rs/src/query.rs` — the same crate that
  already defined `describe_command_redacted`. A fix applied only to the three
  named sites would have left the SDK's own path leaking. All four were fixed
  together, which is why the row is ✅ rather than partially closed.
* `download_image` is **two** bugs, not one. An allowlist on the fetch closes
  the SSRF and leaves the local-file-read branch untouched, so they are
  tracked separately and must be closed separately.

### Corrections to the 2026-09-30 report

* `require_auth` injects **anonymous** claims, not admin ones:
  `Claims::anonymous()` sets `sub: ANONYMOUS_USER_ID` and no scope. It is
  still a full bypass of every check below it, so the severity stands, but
  the mechanism is "open access", not "privilege escalation".
* "`ChatEvent::InputRequest` never emitted" was true and is now fixed —
  but fixing it backend-side created the frontend half, which the report
  could not have seen.
* "`TaskStatus` formatted with `{:?}`" was one line in the report and is
  two bugs in the code: the API payloads (closed) and the WebSocket +
  compaction paths (open).

### Why three "completed" tasks are not ✅

`cypher-injection-wherebuilder`, `tombstone-placeholder-signature` and
`auth-anonymous-without-config` all have finished work on a branch, and two of
their PO tasks read `completed`. `git merge-base --is-ancestor` says none of
the three branches is an ancestor of `origin/main`, and each diverges by
thousands of lines from a stale base — they cannot be fast-forwarded as they
stand. Until a rebase lands, `origin/main` is vulnerable. They stay 🟠/🔴.

---

## 5. Deduplication

The task that produced this registry was required to introduce **no duplicate**
of plans `b36d511f` (*Durcissement issu de la cartographie architecture*) and
`3c0dcc12` (*Consolidation de main*).

* **`b36d511f` — 7 tasks, checked one by one.** Three of its tasks already own
  entries in section 4 (`efbb040e` → `cypher-injection-wherebuilder`,
  `9f873bd5` → `auth-anonymous-without-config`, `48b57611` →
  `tombstone-placeholder-signature`). The registry **points at those tasks**
  and creates nothing. Its four remaining tasks (status/energy coherence, CI
  toolchain, forgotten integration tests, unwired code) are not single bugs
  but work items, and are deliberately absent.
* **`3c0dcc12` — 0 tasks.** The plan is in `draft` and holds no task at all, so
  no duplication is possible. Its rules (no release, no tag, no bump; push
  `HEAD:refs/heads/<branch>`) are respected here.
* **No new PO task was created by this slice.** Every live bug already had an
  owner. Creating a second task for an owned bug is the duplication the
  criterion forbids, and the three stale branches are evidence of what happens
  when ownership is split.

---

## 6. Open gaps this registry does not close

* `po-bugs` and this file live in `nexus`, where this slice landed. The
  per-domain 🔴 nodes (artefact 3) belong in the `backend` and `frontend`
  diagrams, which do not exist yet — task 0.2 owns that, and task 1.1 owns the
  CI drift gate. Until then, artefact 3 is satisfied by the `po-bugs` node
  alone, and the registry says so rather than pretending otherwise.
* Two of the three live bugs have no **red test** on `origin/main`. Their
  branches carry tests, but an unmerged test does not protect the default
  branch. Rebasing those branches is the next action, not writing new fixes.
