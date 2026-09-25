"""Tests for check_image_inputs.py. Run: python3 -m unittest discover -s .github/scripts"""

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
import check_image_inputs as c  # noqa: E402

TAG = "v1.2.3"
COMMIT = "0123456789abcdef0123456789abcdef01234567"


def sha(data):
    return hashlib.sha256(data).hexdigest()


class Release:
    """What publish-image.yml downloads, in a temporary folder."""

    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.archives = {name: name.encode() * 3 for name in c.archives(TAG)}
        for name, data in self.archives.items():
            (self.dir / name).write_bytes(data)
        # As `sha256sum *` writes it, with the other assets listed too.
        lines = [f"{sha(b'msi')}  app.msi"]
        lines += [f"{sha(d)}  {n}" for n, d in self.archives.items()]
        self.write_sums("\n".join(lines) + "\n")

    def write_sums(self, text, checks_note=""):
        sums = text.encode()
        (self.dir / c.SUMS).write_bytes(sums)
        (self.dir / c.CHECKS).write_text(
            f"commit {COMMIT}\nrun https://github.com/o/r/actions/runs/1 (attempt 1)\n"
            f"SHA256SUMS.txt {sha(sums)}\n{checks_note}"
        )

    def sums(self):
        return (self.dir / c.SUMS).read_text()

    def main(self, release=None, commit=COMMIT):
        release = {"isDraft": False, "isPrerelease": False} if release is None else release

        def gh(args, **_):
            assert args[:3] == ["gh", "release", "view"], args
            return subprocess.CompletedProcess(args, 0, json.dumps(release), "")

        return c.main(["check_image_inputs.py", TAG, str(self.dir), commit], run=gh)


class Checks(unittest.TestCase):
    def setUp(self):
        self.r = Release()
        self.addCleanup(self.r.tmp.cleanup)

    def test_a_published_release_that_passed_everything(self):
        self.assertEqual(self.r.main(), 0)

    def test_lines_release_checks_does_not_know_are_ignored(self):
        self.r.write_sums(self.r.sums(), checks_note="note: added by hand\n")
        self.assertEqual(self.r.main(), 0)

    def test_a_draft_or_pre_release_gets_no_image(self):
        self.assertEqual(self.r.main({"isDraft": True, "isPrerelease": False}), 1)
        self.assertEqual(self.r.main({"isDraft": False, "isPrerelease": True}), 1)
        self.assertEqual(self.r.main({}), 1)

    def test_the_checks_must_be_for_the_tags_commit(self):
        self.assertEqual(self.r.main(commit="f" * 40), 1)

    def test_the_checks_must_name_these_checksums(self):
        (self.r.dir / c.SUMS).write_text(self.r.sums() + f"{sha(b'x')}  extra\n")
        self.assertEqual(self.r.main(), 1)

    def test_without_the_checks_there_is_no_image(self):
        (self.r.dir / c.CHECKS).unlink()
        self.assertEqual(self.r.main(), 1)

    def test_an_archive_without_an_entry_is_refused(self):
        # With `sha256sum --check --ignore-missing` this passed.
        first = c.archives(TAG)[0]
        kept = [line for line in self.r.sums().splitlines() if not line.endswith(first)]
        self.r.write_sums("\n".join(kept) + "\n")
        (self.r.dir / first).write_bytes(b"not what was built")
        self.assertEqual(self.r.main(), 1)

    def test_an_archive_with_two_entries_is_refused(self):
        first = c.archives(TAG)[0]
        self.r.write_sums(self.r.sums() + f"{sha(b'other')}  {first}\n")
        self.assertEqual(self.r.main(), 1)

    def test_an_archive_that_does_not_match_is_refused(self):
        (self.r.dir / c.archives(TAG)[1]).write_bytes(b"changed")
        self.assertEqual(self.r.main(), 1)

    def test_a_missing_archive_is_refused(self):
        (self.r.dir / c.archives(TAG)[1]).unlink()
        self.assertEqual(self.r.main(), 1)

    def test_a_line_that_is_not_a_checksum_is_refused(self):
        self.r.write_sums(self.r.sums() + "garbage\n")
        self.assertEqual(self.r.main(), 1)

    def test_archive_names_are_the_release_workflows(self):
        self.assertEqual(
            c.archives("v0.3.0"),
            [
                "streamdelayd-v0.3.0-x86_64-unknown-linux-gnu.tar.gz",
                "streamdelayd-v0.3.0-aarch64-unknown-linux-gnu.tar.gz",
            ],
        )


if __name__ == "__main__":
    unittest.main()
