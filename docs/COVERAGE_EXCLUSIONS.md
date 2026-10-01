# Coverage exclusions (nexus)

Every line excluded from coverage measurement is listed here, with the reason
and an owner. Two rules govern this file:

1. **The count can only go down.** Adding an exclusion is a change to this
   file, reviewed like any other; removing one needs no ceremony.
2. **A number is never raised by excluding code.** If a module's percentage
   improves because something stopped being measured, that is a regression
   wearing a better number. Both figures — *raw* (every line) and *gated*
   (what survives the exclusions below) — are reported, always.

An exclusion is legitimate when the code cannot be meaningfully executed by a
test: a `main` that binds a socket, a generated file. It is **not** legitimate
because the code is merely hard to test. "Hard to test" is a finding, not an
exemption; it belongs in a task.

## Current exclusions

Source: the `ignore:` section of `codecov.yml`. Applied at upload — note that
`cargo llvm-cov` itself measures everything, so the raw number is always
available locally.

| Pattern | Why | Owner | Status |
|---|---|---|---|
| `examples/**` | Examples exist to be read. They are compiled (so they cannot rot) but not executed by the suite; covering them would measure the examples, not the library. | theotime | justified |
| `tests/**` | Test code measuring itself tells you nothing about the library. | theotime | justified |
| `**/test*.rs` | Same reason, for test helpers that live beside the code they exercise. | theotime | justified |
| `**/mod.rs` | Inherited rationale: "module files typically don't have logic". **Questionable** — a `mod.rs` in this workspace can carry re-exports, `impl` blocks and constructors, which is real logic being hidden. Re-examine and most likely remove. | theotime | to review (task 3.4) |

## Not an exclusion, but worth knowing

- `claude-code-api` and `claude-code-sdk-rs` are measured under separate
  Codecov flags (`api`, `sdk`), both with `carryforward: true`. A run that
  uploads only one flag leaves the other's last known value standing rather
  than reporting zero.
- The **project** gate is still `informational: true`: this repository has no
  measured baseline yet, and a blocking gate on an unmeasured number is a
  number nobody chose. Task 3.4 measures both crates, raw and gated, and
  removes that line.
- The **patch** gate is blocking at 80%. It judges only the lines a pull
  request changes, so it is fair without a repository-wide baseline.

## How to measure

```bash
# One target directory per worktree (see CONTRIBUTING.md).
export CARGO_TARGET_DIR="$PWD/target"

# Gated-equivalent and raw both come from the same run; the exclusions above
# are applied by Codecov at upload, not by the tool.
cargo llvm-cov --all-features --workspace --lcov --output-path lcov.info
cargo llvm-cov --all-features --workspace --summary-only
```
