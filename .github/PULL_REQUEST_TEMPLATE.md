<!--
A pull request is one vertical slice: the fix, a regression test that FAILS
without the fix, the diagram, and the documentation. An unchecked box in the
checklist means the pull request is not mergeable.

Keep the sections below. Replace the comments with your answers; write
"none" or "not applicable" where a section genuinely does not apply, rather
than deleting it — a missing section reads as an oversight.
-->

## Domain

<!-- Which part of the code: sdk transport, api models, chat, hooks, ci, ... -->

## Diagram(s) touched

<!--
Name the docs/diagrams/<name>.mmd file you changed.

If an existing diagram still describes the code exactly, add a commit trailer
instead and say so here:

    Diagram-Unchanged: <name> — <reason>

The reason is required: the CI drift check rejects a bare name. If no diagram
covers this code yet, write "none yet" — the check reports it without failing
until every path has an owning diagram.
-->

## Bug(s) fixed

<!-- What was broken, and the issue link (Fixes #123). "None" for a feature or a refactor. -->

## Proof of the regression test

<!--
A test that does not fail without the fix proves nothing. Paste:
  1. the command run WITHOUT the fix (reverted or stashed) and the failing output,
  2. the same command WITH the fix, passing.
-->

```text
```

## Module coverage, before / after

<!--
Give both numbers where they exist: "raw" (every line of the module) and
"gated" (what the coverage configuration still measures after exclusions).
Never raise a number by excluding code. Any new exclusion belongs in
docs/COVERAGE_EXCLUSIONS.md with its reason and owner.
-->

| Module | Raw before | Raw after | Gated before | Gated after |
|---|---|---|---|---|
|  |  |  |  |  |

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features` all pass locally
- [ ] A regression test exists and was proven to fail without the fix (section above)
- [ ] Documentation updated; diagram updated, or `Diagram-Unchanged: <name> — <reason>` in a commit, or "none yet"
- [ ] If this fixes a finding listed in `docs/RELEASE_READINESS.md`, its row is **deleted** in this pull request (the gate fails on a stale claim just as it does on a hidden one)
- [ ] If a new http(s) host is referenced, it is added to `docs/allowed-hosts.txt` in this pull request, with the reason it is public
- [ ] No release, no tag, no version bump, no registry publication
- [ ] Pushed with `git push origin HEAD:refs/heads/<branch>` — never directly to `main`
