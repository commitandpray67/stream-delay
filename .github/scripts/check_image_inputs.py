"""Run by publish-image.yml before it pushes an image, on what it downloaded from
the release of a tag: the Linux headless archives, SHA256SUMS.txt and
release-checks.txt.

- The release is published as a full release: an image never goes out for a
  draft, which nobody has reviewed yet, or for a pre-release.
- release-checks.txt, which the release workflow adds once everything passed,
  names the tag's commit and this SHA256SUMS.txt.
- Each archive has exactly one entry in SHA256SUMS.txt, and matches it: an
  archive without one is not let through.

Then it writes `latest=true` to GITHUB_OUTPUT if no other published full
release has a higher version, else `latest=false`: only the latest release's
image is tagged `latest` and `edge`, so pushing an older release's image again
never takes them back to it.

Usage: check_image_inputs.py TAG DIR COMMIT, with GH_TOKEN and GH_REPO set.
COMMIT is the tag's commit.
Tests: python3 -m unittest discover -s .github/scripts
"""

import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

# What the image packages, by target.
TARGETS = ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu")
SUMS = "SHA256SUMS.txt"
CHECKS = "release-checks.txt"


class Refused(Exception):
    pass


def archives(tag):
    """The archive names the release workflow gives the Linux builds of `tag`."""
    version = tag.replace("/", "-")
    return [f"streamdelayd-{version}-{t}.tar.gz" for t in TARGETS]


def check_published(tag, release):
    """`release` as `gh release view --json isDraft,isPrerelease` shows it."""
    if release.get("isDraft", True):
        raise Refused(f"{tag} is a draft: its image is pushed once it is published")
    if release.get("isPrerelease", True):
        raise Refused(f"{tag} is a pre-release: images are only pushed for full releases")


def check_release_checks(text, commit, sums):
    """`text` of release-checks.txt names `commit` and the SHA256SUMS.txt whose
    content is `sums`. Lines it doesn't know are ignored."""
    fields = {}
    for line in text.splitlines():
        key, _, value = line.partition(" ")
        fields.setdefault(key, value.strip())
    if fields.get("commit") != commit:
        raise Refused(
            f"{CHECKS} names commit {fields.get('commit')!r}, but the tag is {commit}: "
            "the release that passed its checks was built from another commit"
        )
    if fields.get(SUMS) != hashlib.sha256(sums).hexdigest():
        raise Refused(f"{CHECKS} names another {SUMS} than the release has")


def entries(sums_text):
    """{file name: [sha256, ...]} from `sha256sum` output."""
    found = {}
    for n, line in enumerate(sums_text.splitlines(), 1):
        if not line.strip():
            continue
        m = re.fullmatch(r"([0-9a-f]{64}) [ *](.+)", line)
        if not m:
            raise Refused(f"{SUMS} line {n} is not a checksum line")
        found.setdefault(m.group(2), []).append(m.group(1))
    return found


def check_archives(sums_text, paths):
    """Each file in `paths` has exactly one entry in `sums_text`, and matches it."""
    found = entries(sums_text)
    for path in paths:
        if not path.is_file():
            raise Refused(f"the release has no {path.name}")
        listed = found.get(path.name, [])
        if len(listed) != 1:
            raise Refused(f"{SUMS} has {len(listed)} entries for {path.name}, not one")
        if hashlib.sha256(path.read_bytes()).hexdigest() != listed[0]:
            raise Refused(f"{path.name} does not match its entry in {SUMS}")


def view(tag, run):
    r = run(
        ["gh", "release", "view", tag, "--json", "isDraft,isPrerelease"],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise Refused(f"no release {tag}: {r.stderr.strip()}")
    return json.loads(r.stdout)


def version(tag):
    """(major, minor, patch) of a tag such as v1.2.3, or None."""
    m = re.fullmatch(r"v?(\d+)\.(\d+)\.(\d+)", tag or "")
    return tuple(int(n) for n in m.groups()) if m else None


def is_latest(tag, run):
    """True unless another published full release has a higher version: the one
    an image pulled without a version must be. Decided from the versions, not
    from GitHub's latest release, which may not include one published a
    moment ago."""
    mine = version(tag)
    if mine is None:
        raise Refused(f"{tag} is not a version such as v1.2.3")
    r = run(
        ["gh", "release", "list", "--json", "tagName,isDraft,isPrerelease", "--limit", "1000"],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise Refused(f"could not list the releases: {r.stderr.strip()}")
    published = [
        version(x.get("tagName"))
        for x in json.loads(r.stdout)
        if not x.get("isDraft", True) and not x.get("isPrerelease", True)
    ]
    return all(v is None or v <= mine for v in published)


def main(argv, run=subprocess.run) -> int:
    if len(argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    tag, folder, commit = argv[1], Path(argv[2]), argv[3]
    try:
        check_published(tag, view(tag, run))
        for name in (SUMS, CHECKS):
            if not (folder / name).is_file():
                raise Refused(f"the release has no {name}: it has not passed every check")
        sums = (folder / SUMS).read_bytes()
        check_release_checks((folder / CHECKS).read_text(), commit, sums)
        paths = [folder / name for name in archives(tag)]
        check_archives(sums.decode(), paths)
        latest = is_latest(tag, run)
    except Refused as e:
        print(f"::error::{e}")
        return 1
    for path in paths:
        print(f"ok  {path.name}")
    print(f"{tag} is {'' if latest else 'not '}the latest release")
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as f:
            f.write(f"latest={'true' if latest else 'false'}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
