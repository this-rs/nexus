#!/usr/bin/env python3
"""Fail a pull request that changes code a diagram owns without saying so.

WHY. `derive_diagram_index.py` proves the ownership map is well formed: every
diagram has a header, every `covers` glob matches a file, no file has two
owners. It says nothing about whether a diagram still DESCRIBES its files. A
diagram goes wrong the day someone edits the code under it and nobody looks at
the picture. This gate makes that moment visible.

THE RULE. For every file a pull request changes, find the diagrams whose
`covers` match it. For each such diagram, the same pull request must either

  * change `docs/diagrams/<name>.mmd`, or
  * carry, in one of its commit messages, the trailer

        Diagram-Unchanged: <name> — <reason>

    which is the author stating they looked and the diagram is still exact.
    The reason is mandatory: a trailer without one is a way to switch the gate
    off, not an answer to it.

WHAT IT DOES NOT PROVE. That the diagram is right. A touched `.mmd` or a
trailer is a claim by the author, checked by the reviewer. The gate only
guarantees the question was asked.

A changed file no diagram owns is listed, never an error: `INDEX.yml` already
publishes the unclaimed files.

Usage:  python3 scripts/check_diagram_drift.py [--base <ref>]   (default origin/main)
Compares `<base>...HEAD` (what the branch changed since it left the base) and
reads the commit messages of `<base>..HEAD`. Exit 1 on drift. No network.
"""

from __future__ import annotations

import glob
import importlib.util
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location(
    "derive_diagram_index", os.path.join(HERE, "derive_diagram_index.py")
)
derive = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(derive)

TRAILER_KEY = "Diagram-Unchanged:"
# name, then a dash of any kind (—, –, -), then the reason.
TRAILER_RE = re.compile(r"^(\S+)\s+(?:—|–|-{1,2})\s+(\S.*)$")


def glob_to_regex(pattern: str) -> re.Pattern:
    """A `covers` glob as a regex over repository paths.

    Matching on the path, not on the disk: a file the pull request DELETES is
    still a change to what the diagram describes, and `glob.glob` cannot see it.
    `**/` spans any number of directories (including none), `*` and `?` stay
    inside one path segment.
    """
    out, i = "", 0
    while i < len(pattern):
        if pattern.startswith("**/", i):
            out += "(?:.*/)?"
            i += 3
        elif pattern.startswith("**", i):
            out += ".*"
            i += 2
        elif pattern[i] == "*":
            out += "[^/]*"
            i += 1
        elif pattern[i] == "?":
            out += "[^/]"
            i += 1
        else:
            out += re.escape(pattern[i])
            i += 1
    return re.compile(f"^{out}$")


def parse_trailers(messages: list[str]) -> tuple[dict[str, str], list[str]]:
    """`Diagram-Unchanged:` lines of the commit messages → {name: reason}, problems."""
    stated: dict[str, str] = {}
    problems: list[str] = []
    for message in messages:
        for line in message.split("\n"):
            line = line.strip()
            if not line.startswith(TRAILER_KEY):
                continue
            match = TRAILER_RE.match(line[len(TRAILER_KEY):].strip())
            if not match:
                problems.append(
                    f"trailer `{line}` is not `{TRAILER_KEY} <name> — <reason>`: "
                    "the reason is mandatory"
                )
                continue
            stated[match.group(1)] = match.group(2).strip()
    return stated, problems


def find_drift(
    changed: list[str], diagrams: list[dict], trailers: dict[str, str]
) -> tuple[list[str], list[str]]:
    """The rule, as a pure function. Returns (problems, unowned changed files).

    `diagrams` are `{name, file, covers}` as read from the .mmd headers; `file`
    is the repository path of the .mmd.
    """
    problems: list[str] = []
    changed_set = set(changed)
    known = {d["name"] for d in diagrams}
    for name in sorted(set(trailers) - known):
        problems.append(
            f"trailer `{TRAILER_KEY} {name}` names no diagram in {derive.DIAGRAM_DIR}/"
        )

    owned: set[str] = set()
    for diagram in diagrams:
        regexes = [glob_to_regex(p) for p in diagram["covers"]]
        touched = sorted(f for f in changed if any(r.match(f) for r in regexes))
        owned.update(touched)
        if not touched:
            continue
        if diagram["file"] in changed_set or diagram["name"] in trailers:
            continue
        problems.append(
            f"`{diagram['name']}` owns {', '.join(touched)} — changed without "
            f"touching {diagram['file']}. Update the diagram, or state "
            f"`{TRAILER_KEY} {diagram['name']} — <reason>` in a commit message."
        )

    diagram_dir = derive.DIAGRAM_DIR + "/"
    unowned = sorted(
        f for f in changed_set - owned
        if f.endswith(".rs") and not f.startswith(diagram_dir)
    )
    return problems, unowned


def load_diagrams() -> tuple[list[dict], list[str]]:
    """Every diagram's name, path and `covers`, from the headers on disk."""
    diagrams: list[dict] = []
    problems: list[str] = []
    for path in sorted(glob.glob(f"{derive.DIAGRAM_DIR}/*.mmd")):
        if "_TEMPLATE" in path:
            continue
        head, errs = derive.parse_header(path)
        problems += errs
        if not errs:
            diagrams.append({"name": head["name"], "file": path, "covers": head["covers"]})
    return diagrams, problems


def git(*args: str) -> str:
    out = subprocess.run(["git", *args], capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit(f"git {' '.join(args)} failed: {out.stderr.strip()}")
    return out.stdout


def main(argv: list[str]) -> int:
    base = "origin/main"
    if "--base" in argv:
        at = argv.index("--base")
        if at + 1 >= len(argv):
            sys.exit("--base needs a ref")
        base = argv[at + 1]
    os.chdir(derive.repo_root())

    changed = [f for f in git("diff", "--name-only", "-z", f"{base}...HEAD").split("\0") if f]
    messages = [m for m in git("log", "--format=%B%x00", f"{base}..HEAD").split("\0") if m.strip()]

    diagrams, problems = load_diagrams()
    trailers, trailer_problems = parse_trailers(messages)
    drift, unowned = find_drift(changed, diagrams, trailers)
    problems += trailer_problems + drift

    print(f"{len(changed)} changed file(s) against {base} | {len(diagrams)} diagrams")
    for name, reason in sorted(trailers.items()):
        print(f"  stated unchanged: {name} — {reason}")
    if unowned:
        print("  changed, owned by no diagram (not an error):")
        for f in unowned:
            print(f"    {f}")
    for p in problems:
        print(f"  ERROR {p}")
    if problems:
        print(f"\n{len(problems)} problem(s).")
        return 1
    print("no drift")
    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main(sys.argv[1:]))
