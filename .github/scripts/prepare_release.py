"""Run by every release job before it changes the draft release for a tag.

Refuses a release that is published: a build never changes one. From a draft, it
removes the files that say a build passed every check (SHA256SUMS.txt and
release-checks.txt, added last, by the checksums job). A rerun that changes the
draft then fails without them unless it passes everything again, whichever of
its jobs are rerun.

Usage: prepare_release.py <tag>, with GH_TOKEN and GH_REPO set.
Tests: python3 -m unittest discover -s .github/scripts
"""

import json
import subprocess
import sys

# Added by the checksums job once everything passed.
MARKERS = ("SHA256SUMS.txt", "release-checks.txt")


class Refused(Exception):
    pass


def markers_to_remove(release):
    """The marker files to remove from `release`, as `gh release view --json
    isDraft,assets` shows it (None: there is no release yet)."""
    if release is None:
        return []
    if not release.get("isDraft", False):
        raise Refused(
            "this release is published, and a build never changes a published "
            "release: make a new version instead"
        )
    return [a["name"] for a in release.get("assets", []) if a["name"] in MARKERS]


def view(tag, run):
    r = run(
        ["gh", "release", "view", tag, "--json", "isDraft,assets"],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        if "not found" in r.stderr.lower():
            return None
        raise RuntimeError(f"gh release view failed: {r.stderr.strip()}")
    return json.loads(r.stdout)


def main(argv, run=subprocess.run) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    tag = argv[1]
    try:
        for name in markers_to_remove(view(tag, run)):
            # Jobs running side by side may remove it at the same time.
            run(["gh", "release", "delete-asset", tag, name, "--yes"], capture_output=True, text=True)
            print(f"removed {name} from the draft release {tag}")
        # Gone, whoever removed them.
        left = markers_to_remove(view(tag, run))
        if left:
            raise RuntimeError("could not remove " + ", ".join(left))
    except Refused as e:
        print(f"::error::{e}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
