#!/usr/bin/env python3
"""Per-file coverage with a logic-only denominator.

`cargo llvm-cov` instruments the test binary, so the lines of every inline
`#[cfg(test)] mod tests` block are counted as both measured AND covered. On a
crate whose tests live next to the code, that inflates the reported figure: the
test code grades itself. This script removes those lines from the numerator and
the denominator, leaving the coverage of the production logic only.

Test functions are identified from the v0-mangled symbol names in the llvm
export (a `tests` path segment is encoded as `5tests`), so no demangler is
required.

The figure this prints is the repo's reference number, and the `Coverage` job in
`.github/workflows/ci.yml` is the one place that produces it. Read the exact
invocation there rather than here: a usage line in a docstring is copied by hand
and drifts, which is how two people once quoted two different "96.7 %" an hour
apart. The exclusion that makes the figure comparable is no longer advice — it is
enforced below, by `check_exclusions`.
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import re
import sys

# Paths that must not reach the logic denominator. Scaffolding and entry points
# are not the logic under measurement, and counting them makes a figure that
# cannot be compared with any other.
EXCLUDED_FRAGMENTS = ("examples/", "/tests/", "src/bin/")

# Printed verbatim when the guard refuses, so the fix is in the error, not in a
# doc somebody has to find. Kept in sync with the `Coverage` job.
CANONICAL_INVOCATION = """\
    cargo llvm-cov report --json \\
        --ignore-filename-regex '(examples/|/tests/|src/bin/)' \\
        --output-path cov-logic.json
    python3 scripts/coverage_logic_only.py cov-logic.json"""

# A v0-mangled symbol encodes each path segment as <len><name>; `tests`/`test`
# modules therefore appear as `5tests` / `4test`. Legacy mangling and already
# demangled names are matched textually.
TEST_SYMBOL = re.compile(r"(5tests|4test|::tests?::|^tests?::)")


def test_lines_by_file(functions):
    """Lines covered by the body of a test function, per file."""
    out = collections.defaultdict(set)
    for fn in functions:
        if not TEST_SYMBOL.search(fn.get("name", "")):
            continue
        names = fn.get("filenames") or []
        if not names:
            continue
        for region in fn.get("regions", []):
            start, end = region[0], region[2]
            out[names[0]].update(range(start, end + 1))
    return out


def check_exclusions(files):
    """Refuse a report that kept paths the logic figure has to exclude.

    Two things this deliberately does NOT do.

    It does not scan the export textually. `data["functions"]` names excluded
    files even in a perfectly filtered report — a correct run of the canonical
    invocation still mentions `src/bin/fake_claude.rs` there — so a `grep` over
    the JSON would refuse valid input. A guard that rejects a valid report is
    worse than no guard, so only `data["files"]`, the set this script actually
    sums, is inspected.

    It does not match on absolute paths either. The fragments are matched against
    each path *relative to the common root of every reported file*, so a checkout
    that happens to live under a directory called `examples/` is not refused for
    its parent's name.
    """
    names = [entry["filename"] for entry in files]
    if not names:
        return
    root = os.path.commonpath(names) if len(names) > 1 else os.path.dirname(names[0])
    offenders = sorted(
        rel
        for rel in (os.path.relpath(name, root) for name in names)
        if any(fragment in rel for fragment in EXCLUDED_FRAGMENTS)
    )
    if not offenders:
        return
    listed = "\n".join(f"      {rel}" for rel in offenders)
    sys.exit(
        f"error: this export is not a logic report — {len(offenders)} of "
        f"{len(names)} files are paths the logic figure must exclude:\n"
        f"{listed}\n\n"
        "Any number computed from it is not comparable with the repo's reference\n"
        "figure. Regenerate the export with the exclusion:\n\n"
        f"{CANONICAL_INVOCATION}\n\n"
        "The `Coverage` job in .github/workflows/ci.yml does exactly this and is\n"
        "the producer of record."
    )


def analyze(path, strip_prefix=""):
    export = json.load(open(path))
    data = export["data"][0]
    check_exclusions(data["files"])
    test_lines = test_lines_by_file(data.get("functions", []))

    rows = []
    for entry in data["files"]:
        name = entry["filename"]
        counts = {}
        for line, _col, count, has_count, _entry, is_gap in entry.get("segments", []):
            if not has_count or is_gap:
                continue
            counts[line] = max(counts.get(line, 0), count)

        excluded = test_lines.get(name, set())
        total = covered = 0
        uncovered = []
        for line, count in sorted(counts.items()):
            if line in excluded:
                continue
            total += 1
            if count:
                covered += 1
            else:
                uncovered.append(line)

        summary = entry["summary"]
        rows.append(
            {
                "file": name[len(strip_prefix):] if name.startswith(strip_prefix) else name,
                "logic_lines": total,
                "logic_covered": covered,
                "logic_pct": 100.0 * covered / total if total else 100.0,
                "llvm_lines": summary["lines"]["count"],
                "llvm_covered": summary["lines"]["covered"],
                "llvm_pct": summary["lines"]["percent"],
                "region_pct": summary["regions"]["percent"],
                "test_lines": len(excluded),
                "uncovered": uncovered,
            }
        )
    return rows


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("export", help="llvm-cov json export produced by cargo llvm-cov --json")
    ap.add_argument("--strip-prefix", default="", help="drop this prefix from reported paths")
    ap.add_argument("--fail-under", type=float, help="exit 1 if total logic coverage is below PCT")
    ap.add_argument("--json", dest="json_out", help="also write the rows as JSON here")
    args = ap.parse_args(argv)

    rows = analyze(args.export, args.strip_prefix)
    rows.sort(key=lambda r: (r["logic_pct"], -r["logic_lines"]))

    head = f"{'file':<58}{'logic':>7}{'cov':>7}{'%logic':>8}{'%llvm':>8}{'%region':>9}{'testL':>7}"
    print(head)
    print("-" * len(head))
    logic = covered = llvm = llvm_cov = 0
    for r in rows:
        print(
            f"{r['file']:<58}{r['logic_lines']:>7}{r['logic_covered']:>7}"
            f"{r['logic_pct']:>8.1f}{r['llvm_pct']:>8.1f}{r['region_pct']:>9.1f}{r['test_lines']:>7}"
        )
        logic += r["logic_lines"]
        covered += r["logic_covered"]
        llvm += r["llvm_lines"]
        llvm_cov += r["llvm_covered"]
    print("-" * len(head))
    pct = 100.0 * covered / logic if logic else 100.0
    print(f"{'TOTAL':<58}{logic:>7}{covered:>7}{pct:>8.1f}{100.0 * llvm_cov / llvm:>8.1f}")
    print(
        f"\nllvm reports {llvm_cov}/{llvm} lines ({100.0 * llvm_cov / llvm:.1f} %) — "
        f"logic only: {covered}/{logic} ({pct:.1f} %), {logic - covered} lines uncovered."
    )

    if args.json_out:
        json.dump(rows, open(args.json_out, "w"), indent=1)

    if args.fail_under is not None and pct < args.fail_under:
        print(f"\nFAIL: logic coverage {pct:.2f} % < required {args.fail_under} %", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
