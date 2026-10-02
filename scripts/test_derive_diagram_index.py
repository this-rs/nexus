"""Tests for the `%% verified:` rule of scripts/derive_diagram_index.py.

`verified` exists so that what the author saw can be replayed: a date can be
looked up in `git log --until`, a short sha can be checked out. `yesterday`
and `TODO` cannot — a diagram carrying one is unverifiable, and until now the
gate only checked the field was non-empty.

Run: python3 -m unittest scripts/test_derive_diagram_index.py
"""
import importlib.util
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("derive", os.path.join(HERE, "derive_diagram_index.py"))
derive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(derive)


def header_problems(verified: str):
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "demo.mmd")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(f"%% name: demo\n%% covers: a/*.rs\n%% verified: {verified}\nflowchart TD\n")
        return derive.parse_header(path)[1]


class VerifiedFormat(unittest.TestCase):
    def test_iso_date_is_accepted(self):
        self.assertEqual(header_problems("2026-10-02"), [])

    def test_short_and_full_git_sha_are_accepted(self):
        self.assertEqual(header_problems("443a2a0"), [])
        self.assertEqual(header_problems("443a2a0" + "b" * 33), [])

    def test_yesterday_is_rejected(self):
        problems = header_problems("yesterday")
        self.assertEqual(len(problems), 1)
        self.assertIn("verified", problems[0])
        self.assertIn("yesterday", problems[0])

    def test_todo_is_rejected(self):
        self.assertEqual(len(header_problems("TODO")), 1)

    def test_an_impossible_date_is_rejected(self):
        # matches the shape YYYY-MM-DD but is not a calendar day
        self.assertEqual(len(header_problems("2026-13-45")), 1)

    def test_too_short_or_non_hex_sha_is_rejected(self):
        self.assertEqual(len(header_problems("abc12")), 1)
        self.assertEqual(len(header_problems("zzzzzzz")), 1)

    def test_every_committed_diagram_passes(self):
        # The 3 existing diagrams were never touched by this rule: prove it on the real files.
        d = os.path.join(HERE, "..", "docs", "diagrams")
        checked = 0
        for name in sorted(os.listdir(d)):
            if name.endswith(".mmd") and not name.startswith("_"):
                _, problems = derive.parse_header(os.path.join(d, name))
                self.assertEqual(problems, [], name)
                checked += 1
        self.assertGreaterEqual(checked, 13)


if __name__ == "__main__":
    unittest.main()
