# Release readiness report

**Verdict: NOT READY.**

**No release, no tag, no version bump and no publication was performed to
produce this report.** Nothing here authorises one. The decision to release,
and the end-to-end test that precedes it, belong to the repository owner.

This document answers one question with evidence: *if a release were cut today,
would it be honest?* It is produced by task 5.1 of the living-documentation
plan, and it is kept exact by a gate — see [How this stays true](#how-this-stays-true).

- Diagram: [`docs/diagrams/release-readiness.mmd`](diagrams/release-readiness.mmd)
- Gate: `claude-code-api/tests/release_readiness.rs`
- Facts below were read at nexus `dc510d5`; cross-repository facts were read on
  2026-10-01 and each cites the command or identifier that produced it.

---

## How this stays true

A readiness report that is written once rots immediately, and a rotten one is
worse than none: it reads as reassurance. So the findings are not prose. The
gate recomputes them from the repository on every CI run and asserts that this
document lists **exactly** the findings that exist:

- a new drift appears → the gate fails until a row is added here;
- a drift is fixed → the gate fails until its row is **deleted** from here.

A report that overstates the problems is as useless as one that hides them, so
both directions fail. Each row carries an HTML comment marker — the word
`finding:` followed by the id — which the gate reads, so it checks intent
rather than guessing from the wording. (This paragraph deliberately avoids
writing a complete marker: the gate would read it as a claim, and did, the
first time this document was drafted.)

The gate never touches the network. It walks the working tree, parses the
manifests, the READMEs, the changelog and the release workflow, and compares.
It therefore gives the same verdict offline.

### Why the forbidden-reference check is an allowlist

A public repository must not reference private or machine-local services.
Writing the names of those services into a checker would publish them — the
exact thing being prevented. The rule is therefore inverted:
[`docs/allowed-hosts.txt`](allowed-hosts.txt) lists every host this repository
may reference over http(s), and the gate reports anything else as
`foreign-host`. A leaked private host fails CI without ever being named in
tracked code. Adding a host is a deliberate act, done in the pull request that
introduces the reference.

---

## nexus — open findings

| id | severity | what is wrong | how it was established |
| --- | --- | --- | --- |
| `member-version-not-inherited` <!-- finding: member-version-not-inherited --> | medium | `claude-code-sdk-rs/Cargo.toml` hard-codes `version = "0.5.0"` instead of `version.workspace = true`. It agrees with the workspace *today*, which is exactly the trap: nothing makes the next bump update both. | `member_package_version` in the gate; `script/check-version-consistency.sh` prints it as a note |
| `changelog-behind-workspace` <!-- finding: changelog-behind-workspace --> | **high** | The newest `CHANGELOG.md` entry is `[0.1.10] - 2025-01-15` while the workspace is at `0.5.0`. Everything released since is undocumented, so release notes generated from the file would describe the wrong software. | `changelog_top_version` vs `workspace_version` |
| `release-no-checksums` <!-- finding: release-no-checksums --> | **high** | `.github/workflows/release.yml` uploads binaries with no `SHA256SUMS`, so a download cannot be verified by whoever installs it. | `missing_release_guards`; no `sha256`/`shasum`/`checksum` step exists in the workflow |
| `release-no-smoke-test` <!-- finding: release-no-smoke-test --> | **high** | No job installs a built artifact and runs it. A binary that cannot start on its own target would be published and only fail on a user's machine. | `missing_release_guards`; no step runs `--version` on an artifact |

### Closed in this slice

| id | what changed | proof |
| --- | --- | --- |
| `release-tag-version-unchecked` | The published version was taken from the git tag and never compared with the manifests. A mistyped tag would have published a crate at a version nobody chose — and a crates.io version can never be reused. Added `script/check-version-consistency.sh` and a `version-consistency` job that every other release job depends on. | `script/check-version-consistency.sh v9.9.9` exits non-zero against a `0.5.0` workspace; the gate no longer reports the finding |

### What nexus already satisfies

- **CI green on `main`.** `gh run list --repo this-rs/nexus --branch main` — the
  most recent `CI` runs are all `success`. The workflow triggers on
  `push: [main, 'v*']`, so `main` genuinely has runs.
- **Version coherence across documentation.** The badge, the heading and the
  copyable dependency snippet in `README.md`, `README_CN.md` and `README_JA.md`
  all read `0.5.0`, matching `[workspace.package]`. Checked by
  `documented_versions`, including the localised badge labels.
- **No reference to a non-public host.** `foreign_hosts` reports nothing across
  every tracked text file. The check is proven by a negative control: appending
  a URL on an unlisted host to `script/run.sh` makes the gate fail, and it is
  the same check that would have caught the backend leak recorded below.
- **No CI job skipped without justification.** The one tolerated failure —
  `continue-on-error` on the nightly toolchain — carries a written reason in
  `.github/workflows/ci.yml`: nightly rustc is prone to internal compiler
  errors unrelated to this code.

### Known gaps not yet expressed as gate findings

These are real but belong to other tasks; the gate does not compute them, so
they are recorded here in prose rather than as rows:

- Both Codecov gates in `codecov.yml` carry `informational: true`, so coverage
  cannot fail a build. Nothing currently stops a release from shipping less
  tested than its predecessor. Task 3.4 owns making them blocking.
- `Cargo.lock` is listed in `.gitignore`, so CI resolves dependencies fresh on
  every run. A new release of any transitive dependency can break `main`
  overnight with no code change — this has already happened once (`time
  0.3.48` breaking `cookie 0.18.1` through `axum-test`, now pinned in
  `claude-code-api/Cargo.toml` with a comment). A release built from
  unpinned dependencies is not reproducible.
- `tests/api_tests.rs` sits at the workspace root, where the manifest is
  virtual. `cargo metadata` lists **no** test target for `claude-code-api`:
  the file is compiled by nothing and has never run.
- The latest tag is `v0.0.5` (`git tag`), five minor versions behind the
  manifests. No release has ever been published at `0.5.0`.
- `RELEASE_v0.3.0.md` remains at the repository root, describing a version two
  minors old.
- The distribution matrix covers five targets
  (`x86_64`/`aarch64` × linux-gnu/darwin, plus `x86_64-pc-windows-msvc`). There
  is no `aarch64-pc-windows-msvc`, no musl variant, and no signing or
  notarisation step. Completing and judging that matrix is task 2.G.

---

## project-orchestrator (backend) — open findings

| id | severity | what is wrong | how it was established |
| --- | --- | --- | --- |
| `backend-main-references-local-tooling` <!-- finding: backend-main-references-local-tooling --> | **high** | A vault test fixture on `origin/main` still carries the name of a machine-local design service in its description string. PR #475 (`chore/remove-local-tooling`) removes the helper scripts but **not** this occurrence: the same scan run against `origin/chore/remove-local-tooling` still reports the file. Merging #475 as it stands would leave the leak in place. | `git grep -lI <pattern> origin/main` reports 4 files; the same command against the PR branch still reports `src/vault/store.rs` |

### What the backend already satisfies

- **CI green on `main`.** `gh run list --repo this-rs/project-orchestrator
  --workflow CI` — `main` at `962e2119` is `success`. The workflow triggers on
  `push: [main]`.
- **No leak in the working tree.** The scan reports nothing in the checked-out
  tree; the finding above is about `origin/main` and the open PR branch.

### Not evaluable yet

- Five pull requests are open (#469, #471, #473, #474, #475). "Active branches
  merged or explicitly set aside" cannot be asserted while they are in flight,
  and the task constraints forbid touching them.
- `CI` on `feat/attention-backend` is `failure`. That is an active branch, left
  alone deliberately.
- The history still contains the local tooling in earlier commits
  (`git log -S` finds `05294a51` and `d39d265a`). Rewriting published history
  is not done without an explicit decision from the owner.

---

## project-orchestrator-frontend — open findings

| id | severity | what is wrong | how it was established |
| --- | --- | --- | --- |
| `frontend-ci-never-runs-on-main` <!-- finding: frontend-ci-never-runs-on-main --> | **high** | `.github/workflows/ci.yml` declares `on: pull_request: branches: [main]` and nothing else. There is no `push` trigger, so **`main` has never had a CI run**. The acceptance criterion "CI green on `main` for all three repositories" is not merely unmet — it is unmeasurable for this repository. Post-merge breakage (a bad merge, a dependency resolved differently) is invisible. | `gh run list --repo this-rs/project-orchestrator-frontend --branch main` returns zero runs, while the same command without `--branch` returns many on feature branches |

### Not evaluable yet

- Six pull requests are open (#184–#189); `CI` on `fix/sl-28-crud-events` is
  `failure`. All are active branches the constraints protect.

---

## Acceptance criteria of task 5.1, judged

| criterion | state | why |
| --- | --- | --- |
| Every high bug fixed with a merged PR and a red-without-the-fix test, or deferred by a written decision | **not met** | <!-- finding: bug-registry-incomplete --> Task 1.3 (bug registry, deduplicated against the other plans) is `blocked`. Without it there is no agreed list of high bugs, so the criterion has no denominator. Judging it from the notes alone would reproduce the failure this plan exists to fix: an agent's assertion treated as proof. |
| CI green on `main` for all three repositories, no job skipped without written justification | **not met** | Green on nexus and the backend. For the frontend, `main` has no runs at all — see `frontend-ci-never-runs-on-main`. |
| Distribution matrix (2.G) with no red cell; version coherence checked by a script | **partly met** | <!-- finding: distribution-matrix-incomplete --> Version coherence is now checked by a script, in CI, offline, with a negative control (`script/check-version-consistency.sh`). The matrix itself does not exist: task 2.G is `pending`. nexus already shows red cells (no checksums, no smoke test). |
| No release, no tag, no version bump, no publication performed | **met** | Nothing was published. `git tag` is unchanged, every manifest version is untouched, and the only release-workflow change *adds* a refusal. Asserted by `the_report_states_that_nothing_was_released`. |
| Maps and diagrams touched by the fixes updated in the same PR | **met for this slice** | `docs/diagrams/release-readiness.mmd` ships in this change and records `release-tag-version-unchecked` as closed, with how it was verified. |

---

## What would have to become true

In rough dependency order, and none of it is a release step:

1. Add a `push: branches: [main]` trigger to the frontend CI, so the criterion
   becomes measurable at all.
2. Extend PR #475 to the remaining occurrence in the vault fixture, or open a
   follow-up, so the backend's `main` stops naming a machine-local service.
3. Reconstruct `CHANGELOG.md` from `0.1.10` to `0.5.0` (task 2.H owns the
   documentation sweep).
4. Add checksums and a per-artifact smoke test to the nexus release workflow,
   then judge the full matrix (task 2.G).
5. Make `claude-code-sdk-rs` inherit `version.workspace = true`.
6. Finish task 1.3 so "every high bug fixed" acquires a denominator.
7. Commit `Cargo.lock` so a release is reproducible, or write down why not.

Only then is the question worth asking again — and the answer is still the
owner's to give, after their own end-to-end test.
