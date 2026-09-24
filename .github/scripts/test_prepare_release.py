"""Tests for prepare_release.py. Run: python3 -m unittest discover -s .github/scripts"""

import json
import os
import subprocess
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import prepare_release as p  # noqa: E402

TAG = "v1.2.3"


def release(draft, *names):
    return {"isDraft": draft, "assets": [{"name": n} for n in names]}


class FakeGh:
    """`gh release` against one release held in memory."""

    def __init__(self, rel):
        self.release = rel
        self.calls = []

    def __call__(self, args, **_):
        self.calls.append(args)
        if args[:3] == ["gh", "release", "view"]:
            if self.release is None:
                return subprocess.CompletedProcess(args, 1, "", "release not found\n")
            return subprocess.CompletedProcess(args, 0, json.dumps(self.release), "")
        if args[:3] == ["gh", "release", "delete-asset"]:
            name = args[4]
            self.release["assets"] = [a for a in self.release["assets"] if a["name"] != name]
            return subprocess.CompletedProcess(args, 0, "", "")
        raise AssertionError(f"unexpected command {args}")

    def deleted(self):
        return [c[4] for c in self.calls if c[:3] == ["gh", "release", "delete-asset"]]


class MarkersToRemove(unittest.TestCase):
    def test_no_release_yet(self):
        self.assertEqual(p.markers_to_remove(None), [])

    def test_a_draft_loses_only_its_markers(self):
        rel = release(True, "app.msi", "SHA256SUMS.txt", "release-checks.txt", "latest.json")
        self.assertEqual(p.markers_to_remove(rel), ["SHA256SUMS.txt", "release-checks.txt"])
        self.assertEqual(p.markers_to_remove(release(True, "app.msi")), [])

    def test_a_published_release_is_never_changed(self):
        with self.assertRaises(p.Refused):
            p.markers_to_remove(release(False, "app.msi", "SHA256SUMS.txt"))


class Main(unittest.TestCase):
    def test_a_rerun_removes_the_markers_of_the_run_before(self):
        gh = FakeGh(release(True, "app.msi", "SHA256SUMS.txt", "release-checks.txt"))
        self.assertEqual(p.main(["prepare_release.py", TAG], run=gh), 0)
        self.assertEqual(gh.deleted(), ["SHA256SUMS.txt", "release-checks.txt"])
        self.assertEqual([a["name"] for a in gh.release["assets"]], ["app.msi"])

    def test_the_first_run_has_nothing_to_do(self):
        gh = FakeGh(None)
        self.assertEqual(p.main(["prepare_release.py", TAG], run=gh), 0)
        self.assertEqual(gh.deleted(), [])

    def test_a_published_release_fails_the_job(self):
        gh = FakeGh(release(False, "app.msi", "SHA256SUMS.txt"))
        self.assertEqual(p.main(["prepare_release.py", TAG], run=gh), 1)
        self.assertEqual(gh.deleted(), [])

    def test_a_marker_that_stays_fails_the_job(self):
        gh = FakeGh(release(True, "SHA256SUMS.txt"))

        def refuse_deletes(args, **kw):
            if args[:3] == ["gh", "release", "delete-asset"]:
                return subprocess.CompletedProcess(args, 1, "", "HTTP 403\n")
            return gh(args, **kw)

        with self.assertRaises(RuntimeError):
            p.main(["prepare_release.py", TAG], run=refuse_deletes)


if __name__ == "__main__":
    unittest.main()
