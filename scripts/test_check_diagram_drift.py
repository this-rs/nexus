"""Tests for scripts/check_diagram_drift.py.

The rule is a pure function (`find_drift`) and is tested as one. `main` is then
run against a real, throw-away git repository: the two fixtures the gate exists
for — a covered file changed with nothing said (must fail), and the same change
with the diagram touched or the trailer present (must pass).

Run: python3 -m unittest scripts/test_check_diagram_drift.py
"""
import contextlib
import importlib.util
import io
import os
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("drift", os.path.join(HERE, "check_diagram_drift.py"))
drift = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drift)

DIAGRAMS = [
    {"name": "transport", "file": "docs/diagrams/transport.mmd", "covers": ["sdk/src/transport/*.rs"]},
    {"name": "api", "file": "docs/diagrams/api.mmd", "covers": ["api/src/**/*.rs", "api/build.rs"]},
]


class GlobToRegex(unittest.TestCase):
    def test_star_stays_inside_one_segment(self):
        r = drift.glob_to_regex("sdk/src/transport/*.rs")
        self.assertTrue(r.match("sdk/src/transport/mod.rs"))
        self.assertFalse(r.match("sdk/src/transport/deep/mod.rs"))
        self.assertFalse(r.match("sdk/src/transport/mod.rs.bak"))

    def test_double_star_spans_zero_or_more_directories(self):
        r = drift.glob_to_regex("api/src/**/*.rs")
        self.assertTrue(r.match("api/src/lib.rs"))
        self.assertTrue(r.match("api/src/a/b/c.rs"))
        self.assertFalse(r.match("other/api/src/lib.rs"))

    def test_trailing_double_star_and_question_mark(self):
        self.assertTrue(drift.glob_to_regex("api/**").match("api/x/y.toml"))
        r = drift.glob_to_regex("a/?.rs")
        self.assertTrue(r.match("a/b.rs"))
        self.assertFalse(r.match("a/bc.rs"))
        self.assertFalse(r.match("a//.rs"))

    def test_a_dot_is_a_dot(self):
        self.assertFalse(drift.glob_to_regex("a/b.rs").match("a/bxrs"))


class ParseTrailers(unittest.TestCase):
    def test_reads_name_and_reason_with_any_dash(self):
        stated, problems = drift.parse_trailers([
            "fix: x\n\nDiagram-Unchanged: transport — comment only\n",
            "Diagram-Unchanged: api - rename of a private helper",
            "Diagram-Unchanged: other -- typo",
        ])
        self.assertEqual(problems, [])
        self.assertEqual(stated, {
            "transport": "comment only",
            "api": "rename of a private helper",
            "other": "typo",
        })

    def test_a_trailer_without_a_reason_is_a_problem_not_a_pass(self):
        for line in ("Diagram-Unchanged: transport", "Diagram-Unchanged: transport —", "Diagram-Unchanged:"):
            stated, problems = drift.parse_trailers([line])
            self.assertEqual(stated, {}, line)
            self.assertEqual(len(problems), 1, line)
            self.assertIn("reason is mandatory", problems[0])

    def test_other_lines_are_ignored(self):
        self.assertEqual(drift.parse_trailers(["Co-Authored-By: x\n\nprose about Diagram-Unchanged"]), ({}, []))


class FindDrift(unittest.TestCase):
    def test_a_covered_file_changed_alone_is_drift(self):
        problems, unowned = drift.find_drift(["sdk/src/transport/mod.rs"], DIAGRAMS, {})
        self.assertEqual(len(problems), 1)
        self.assertIn("`transport`", problems[0])
        self.assertIn("sdk/src/transport/mod.rs", problems[0])
        self.assertEqual(unowned, [])

    def test_touching_the_diagram_answers_it(self):
        changed = ["sdk/src/transport/mod.rs", "docs/diagrams/transport.mmd"]
        self.assertEqual(drift.find_drift(changed, DIAGRAMS, {}), ([], []))

    def test_the_trailer_answers_it(self):
        problems, _ = drift.find_drift(["sdk/src/transport/mod.rs"], DIAGRAMS, {"transport": "comment"})
        self.assertEqual(problems, [])

    def test_one_diagram_answered_does_not_excuse_another(self):
        changed = ["sdk/src/transport/mod.rs", "api/build.rs", "docs/diagrams/transport.mmd"]
        problems, _ = drift.find_drift(changed, DIAGRAMS, {})
        self.assertEqual(len(problems), 1)
        self.assertIn("`api`", problems[0])

    def test_a_deleted_covered_file_still_counts(self):
        # The path is matched, not the disk: nothing here exists.
        problems, _ = drift.find_drift(["api/src/gone/old.rs"], DIAGRAMS, {})
        self.assertEqual(len(problems), 1)

    def test_a_trailer_naming_no_diagram_is_a_problem(self):
        problems, _ = drift.find_drift([], DIAGRAMS, {"trnasport": "typo"})
        self.assertEqual(len(problems), 1)
        self.assertIn("trnasport", problems[0])

    def test_unowned_sources_are_listed_not_failed(self):
        changed = ["cli/src/main.rs", "README.md", "docs/diagrams/api.mmd"]
        self.assertEqual(drift.find_drift(changed, DIAGRAMS, {}), ([], ["cli/src/main.rs"]))


def run(cwd, *args):
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


class AgainstARealRepository(unittest.TestCase):
    """`main` end to end, on a repository built for the test."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.repo = os.path.realpath(self._tmp.name)
        self._cwd = os.getcwd()
        run(self.repo, "git", "init", "-q", "-b", "main")
        run(self.repo, "git", "config", "user.email", "t@example.com")
        run(self.repo, "git", "config", "user.name", "t")
        run(self.repo, "git", "config", "commit.gpgsign", "false")
        self.write("docs/diagrams/demo.mmd", "%% name: demo\n%% covers: src/*.rs\n%% verified: 2026-10-01\nflowchart TD\n")
        self.write("docs/diagrams/_TEMPLATE.mmd", "%% name: <name>\n")
        self.write("src/a.rs", "fn a() {}\n")
        self.write("bin/free.rs", "fn main() {}\n")
        self.commit("base")
        run(self.repo, "git", "checkout", "-q", "-b", "work")
        os.chdir(self.repo)

    def tearDown(self):
        os.chdir(self._cwd)
        self._tmp.cleanup()

    def write(self, path, text):
        full = os.path.join(self.repo, path)
        os.makedirs(os.path.dirname(full), exist_ok=True)
        with open(full, "w", encoding="utf-8") as fh:
            fh.write(text)

    def commit(self, message):
        run(self.repo, "git", "add", "-A")
        run(self.repo, "git", "commit", "-q", "-m", message)

    def main(self, *argv):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = drift.main(["--base", "main", *argv])
        return code, out.getvalue()

    def test_covered_file_changed_with_nothing_said_fails(self):
        self.write("src/a.rs", "fn a() { changed(); }\n")
        self.commit("fix: change a")
        code, out = self.main()
        self.assertEqual(code, 1)
        self.assertIn("ERROR `demo` owns src/a.rs", out)

    def test_same_change_with_the_diagram_touched_passes(self):
        self.write("src/a.rs", "fn a() { changed(); }\n")
        self.write("docs/diagrams/demo.mmd", "%% name: demo\n%% covers: src/*.rs\n%% verified: 2026-10-03\nflowchart TD\n")
        self.commit("fix: change a, diagram updated")
        code, out = self.main()
        self.assertEqual(code, 0, out)
        self.assertIn("no drift", out)

    def test_same_change_with_the_trailer_passes(self):
        self.write("src/a.rs", "fn a() { changed(); }\n")
        self.commit("fix: change a\n\nDiagram-Unchanged: demo — body only, same flow")
        code, out = self.main()
        self.assertEqual(code, 0, out)
        self.assertIn("stated unchanged: demo — body only, same flow", out)

    def test_a_trailer_without_reason_fails(self):
        self.write("src/a.rs", "fn a() { changed(); }\n")
        self.commit("fix: change a\n\nDiagram-Unchanged: demo")
        code, out = self.main()
        self.assertEqual(code, 1)
        self.assertIn("reason is mandatory", out)

    def test_an_unowned_change_is_listed_and_passes(self):
        self.write("bin/free.rs", "fn main() { x(); }\n")
        self.commit("chore: free")
        code, out = self.main()
        self.assertEqual(code, 0, out)
        self.assertIn("bin/free.rs", out)

    def test_a_malformed_header_fails(self):
        self.write("docs/diagrams/bad.mmd", "%% name: bad\nflowchart TD\n")
        self.commit("docs: bad diagram")
        code, out = self.main()
        self.assertEqual(code, 1)
        self.assertIn("bad.mmd", out)

    def test_base_defaults_to_origin_main_and_a_missing_ref_exits(self):
        with self.assertRaises(SystemExit) as raised:
            drift.main([])  # no `origin/main` in this repository
        self.assertIn("git diff", str(raised.exception))

    def test_base_without_a_ref_exits(self):
        with self.assertRaises(SystemExit) as raised:
            drift.main(["--base"])
        self.assertEqual(str(raised.exception), "--base needs a ref")


if __name__ == "__main__":
    unittest.main()
