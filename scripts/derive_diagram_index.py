#!/usr/bin/env python3
"""Derive docs/diagrams/INDEX.yml from the headers of the .mmd files.

WHY DERIVE RATHER THAN WRITE IT. The index and the diagrams describe the same
facts: which source files each diagram owns. Two documents describing the same
facts always drift. So the diagrams are the single source of truth — each one
declares its own scope in its first three lines — and the index is generated.
A hand-written index is a second truth, and the first thing it does is disagree.

WHAT A HEADER LOOKS LIKE (see docs/diagrams/README.md for the full charter):

    %% name: nexus-sdk-transport
    %% covers: claude-code-sdk-rs/src/transport/*.rs
    %% verified: 2026-10-01

WHAT THIS SCRIPT CHECKS, and fails on:
  * a header missing `name`, `covers` or `verified`;
  * a `verified` that is neither an ISO date nor a git sha (cannot be replayed);
  * `name` disagreeing with the file name (the index keys on it);
  * a `covers` glob matching NO file — the usual cause is a file that moved,
    and the diagram silently stops owning anything;
  * one source file claimed by TWO diagrams — ownership must be exclusive, or
    "who documents this file" has no answer.

It also lists the source files no diagram claims. Those are not failures: a
binary entry point, a `mod.rs` that only declares modules, or a `lib.rs` that
only re-exports are not diagram material. A real gap is a module with logic
that nothing documents, and it belongs in this list so it can be seen.

Run with no argument to check and print; `--write` to regenerate INDEX.yml.
Exit code is 1 when a check fails, so CI can gate on it. No network.
"""

from __future__ import annotations

import datetime
import glob
import os
import re
import subprocess
import sys

DIAGRAM_DIR = "docs/diagrams"
INDEX = f"{DIAGRAM_DIR}/INDEX.yml"
HEADER_KEYS = ("name", "covers", "verified")


SHA_RE = re.compile(r"^[0-9a-f]{7,40}$")


def verified_problem(value: str) -> str | None:
    """Why a `%% verified:` value cannot be replayed, or None when it can.

    `verified` records WHAT the author checked the diagram against, so that a
    reviewer can look at the same thing: a calendar date (`git log --until`) or
    a git sha (`git show <sha>:<file>`). `yesterday` and `TODO` cannot be
    replayed, which makes the diagram unverifiable — the very defect diagrams
    are meant to remove.
    """
    if SHA_RE.match(value):
        return None
    if re.match(r"^\d{4}-\d{2}-\d{2}$", value):
        try:
            datetime.date.fromisoformat(value)
            return None
        except ValueError:
            return f"`{value}` has the shape of a date but is not a calendar day"
    return f"`{value}` is neither an ISO date (YYYY-MM-DD) nor a short git sha — it cannot be replayed"


def repo_root() -> str:
    """The git top level, so the script works from any subdirectory."""
    out = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True
    )
    if out.returncode != 0:
        sys.exit("not inside a git repository")
    return out.stdout.strip()


def parse_header(path: str) -> tuple[dict, list[str]]:
    """Read the `%% key: value` lines at the top of a .mmd file."""
    found: dict[str, object] = {}
    problems: list[str] = []
    with open(path, encoding="utf-8") as fh:
        for line in fh.read().split("\n")[:6]:
            if not line.startswith("%% "):
                continue
            if ":" not in line:
                continue
            key, value = line[3:].split(":", 1)
            key, value = key.strip(), value.strip()
            if key == "covers":
                found[key] = [g.strip() for g in value.split(",") if g.strip()]
            elif key in HEADER_KEYS:
                found[key] = value
    for key in HEADER_KEYS:
        if not found.get(key):
            problems.append(f"{path}: header `{key}` is missing or empty")
    if found.get("verified"):
        why = verified_problem(str(found["verified"]))
        if why:
            problems.append(f"{path}: header `verified`: {why}")
    stem = os.path.basename(path)[: -len(".mmd")]
    if found.get("name") and found["name"] != stem:
        problems.append(f"{path}: name `{found['name']}` does not match the file name")
    return found, problems


def source_files() -> list[str]:
    """Tracked Rust sources, excluding test and example trees."""
    out = subprocess.run(
        ["git", "ls-files", "-z", "*.rs"], capture_output=True, text=True, check=True
    )
    files = [p for p in out.stdout.split("\0") if p]
    return sorted(
        p
        for p in files
        if "/src/" in p and "/tests/" not in p and "/examples/" not in p
    )


def main() -> int:
    os.chdir(repo_root())
    write = "--write" in sys.argv

    mmds = sorted(
        p for p in glob.glob(f"{DIAGRAM_DIR}/*.mmd") if "_TEMPLATE" not in p
    )
    if not mmds:
        print(f"no diagram found in {DIAGRAM_DIR}/ — nothing to derive")
        return 1

    entries: list[dict] = []
    problems: list[str] = []
    owner: dict[str, str] = {}

    for path in mmds:
        head, errs = parse_header(path)
        problems += errs
        if errs:
            continue
        for pattern in head["covers"]:
            hits = sorted(glob.glob(pattern, recursive=True))
            if not hits:
                problems.append(
                    f"{path}: `covers` glob `{pattern}` matches no file — "
                    "has the code moved?"
                )
            for hit in hits:
                if hit in owner and owner[hit] != head["name"]:
                    problems.append(
                        f"{hit} is claimed by both `{owner[hit]}` and `{head['name']}`"
                    )
                else:
                    owner[hit] = head["name"]
        entries.append(
            {
                "name": head["name"],
                "file": os.path.basename(path),
                "verified": head["verified"],
                "covers": head["covers"],
            }
        )

    sources = source_files()
    orphans = [s for s in sources if s not in owner]

    print(
        f"{len(entries)} diagrams | {len(sources)} source files | "
        f"{len(owner)} claimed | {len(orphans)} unclaimed"
    )
    for p in problems:
        print(f"  ERROR {p}")
    if orphans:
        print("  unclaimed (not an error — see the module docstring):")
        for o in orphans:
            print(f"    {o}")

    if problems:
        print(f"\n{len(problems)} problem(s). INDEX.yml not written.")
        return 1

    if write:
        with open(INDEX, "w", encoding="utf-8") as fh:
            fh.write(
                "# Diagram -> the code it owns. See README.md for the charter.\n"
                "#\n"
                "# GENERATED from the .mmd headers — do not edit by hand:\n"
                "#   python3 scripts/derive_diagram_index.py --write\n"
                "#\n"
                "# A diagram owns a source file exclusively. A file no diagram\n"
                "# claims is listed under `orphans` so the gap is visible rather\n"
                "# than implicit; binaries, `mod.rs` and re-exporting `lib.rs`\n"
                "# files legitimately live there.\n"
                f"#\n# {len(entries)} diagrams, {len(owner)}/{len(sources)} source files claimed, "
                f"{len(orphans)} unclaimed.\n\n"
                "diagrams:\n"
            )
            for e in entries:
                fh.write(f"  - name: {e['name']}\n")
                fh.write(f"    file: {e['file']}\n")
                fh.write(f"    verified: {e['verified']}\n")
                fh.write("    covers:\n")
                for pattern in e["covers"]:
                    fh.write(f"      - {pattern}\n")
            fh.write("\norphans:\n")
            if orphans:
                for o in orphans:
                    fh.write(f"  - {o}\n")
            else:
                fh.write("  []\n")
        print(f"\n{INDEX} written")

    return 0


if __name__ == "__main__":
    sys.exit(main())
