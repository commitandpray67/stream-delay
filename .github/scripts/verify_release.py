#!/usr/bin/env python3
"""Checks release assets before anything is published.

    verify_release.py signatures DIR
        Every updater signature (*.sig) under DIR is valid for its file and was
        made with the key in the PUBKEY environment variable (the content of
        TAURI_UPDATER_PUBKEY). Used by dry runs, on what each platform built.

    verify_release.py release DIR TAG REPOSITORY
        DIR holds every asset of release TAG (for example v0.3.0) of REPOSITORY
        (owner/name): all installers and headless archives are there, every
        updater bundle is signed with PUBKEY, and latest.json offers the version
        in every entry installed copies may use (and no other), with those
        signatures, each downloaded from exactly
        https://github.com/REPOSITORY/releases/download/TAG/<file>.

Signatures are minisign signatures, which Tauri produces. Only the Python
standard library is used, so the check runs on any runner as it is.
"""

import base64
import hashlib
import json
import os
import sys
from pathlib import Path

# ----- Ed25519 verification (RFC 8032, section 5.1.7) ------------------------------
# Verifying involves no secrets, so this straightforward version is enough.

P = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
D = -121665 * pow(121666, P - 2, P) % P
SQRT_M1 = pow(2, (P - 1) // 4, P)


def _add(a, b):
    x1, y1, z1, t1 = a
    x2, y2, z2, t2 = b
    aa = (y1 - x1) * (y2 - x2) % P
    bb = (y1 + x1) * (y2 + x2) % P
    cc = t1 * 2 * D * t2 % P
    dd = z1 * 2 * z2 % P
    e, f, g, h = bb - aa, dd - cc, dd + cc, bb + aa
    return (e * f % P, g * h % P, f * g % P, e * h % P)


def _mul(s, point):
    q = (0, 1, 1, 0)
    while s > 0:
        if s & 1:
            q = _add(q, point)
        point = _add(point, point)
        s >>= 1
    return q


def _equal(a, b):
    x1, y1, z1, _ = a
    x2, y2, z2, _ = b
    return (x1 * z2 - x2 * z1) % P == 0 and (y1 * z2 - y2 * z1) % P == 0


def _decompress(s):
    if len(s) != 32:
        return None
    y = int.from_bytes(s, "little")
    sign = y >> 255
    y &= (1 << 255) - 1
    if y >= P:
        return None
    x2 = (y * y - 1) * pow(D * y * y + 1, P - 2, P) % P
    if x2 == 0:
        if sign:
            return None
        x = 0
    else:
        x = pow(x2, (P + 3) // 8, P)
        if (x * x - x2) % P != 0:
            x = x * SQRT_M1 % P
        if (x * x - x2) % P != 0:
            return None
        if x & 1 != sign:
            x = P - x
    return (x, y, 1, x * y % P)


_GY = 4 * pow(5, P - 2, P) % P
BASE = _decompress(_GY.to_bytes(32, "little"))


def ed25519_verify(public: bytes, message: bytes, signature: bytes) -> bool:
    if len(public) != 32 or len(signature) != 64:
        return False
    a = _decompress(public)
    r = _decompress(signature[:32])
    if a is None or r is None:
        return False
    s = int.from_bytes(signature[32:], "little")
    if s >= L:
        return False
    h = int.from_bytes(hashlib.sha512(signature[:32] + public + message).digest(), "little") % L
    return _equal(_mul(s, BASE), _add(r, _mul(h, a)))


# ----- minisign ------------------------------------------------------------------


class Invalid(Exception):
    pass


def _lines(b64: str):
    try:
        text = base64.b64decode(b64.strip(), validate=True).decode()
    except Exception as e:
        raise Invalid(f"not base64-encoded text: {e}") from None
    return [line for line in text.splitlines() if line.strip()]


def load_public_key(b64: str):
    """(key id, key) from a Tauri public key (a base64-encoded minisign key file)."""
    lines = [line for line in _lines(b64) if not line.startswith("untrusted comment:")]
    if len(lines) != 1:
        raise Invalid("the public key has an unexpected format")
    raw = base64.b64decode(lines[0])
    if len(raw) != 42 or raw[:2] != b"Ed":
        raise Invalid("the public key is not a minisign Ed25519 key")
    return raw[2:10], raw[10:]


def verify_file(path: Path, signature_b64: str, key):
    """Raises Invalid unless `signature_b64` (a .sig file's content) signs `path` with `key`."""
    key_id, public = key
    lines = _lines(signature_b64)
    if len(lines) != 4 or not lines[2].startswith("trusted comment: "):
        raise Invalid("the signature has an unexpected format")
    sig = base64.b64decode(lines[1])
    if len(sig) != 74:
        raise Invalid("the signature has an unexpected length")
    algorithm, signed_by, value = sig[:2], sig[2:10], sig[10:]
    if signed_by != key_id:
        raise Invalid(f"signed with key {signed_by[::-1].hex().upper()}, not {key_id[::-1].hex().upper()}")
    if algorithm == b"ED":
        digest = hashlib.blake2b(digest_size=64)
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                digest.update(chunk)
        message = digest.digest()
    elif algorithm == b"Ed":
        message = path.read_bytes()
    else:
        raise Invalid(f"unknown signature algorithm {algorithm!r}")
    if not ed25519_verify(public, message, value):
        raise Invalid("the signature does not match the file")
    trusted = lines[2][len("trusted comment: ") :].encode()
    if not ed25519_verify(public, value + trusted, base64.b64decode(lines[3])):
        raise Invalid("the signature's trusted comment was altered")


# ----- checks --------------------------------------------------------------------


def key_from_env():
    b64 = os.environ.get("PUBKEY", "").strip()
    if not b64:
        raise Invalid("PUBKEY is empty: set the TAURI_UPDATER_PUBKEY repository variable")
    return load_public_key(b64)


def check_signatures(root: Path, key) -> int:
    sigs = sorted(root.rglob("*.sig"))
    if not sigs:
        raise Invalid(f"no updater signatures under {root}")
    for sig in sigs:
        target = sig.with_suffix("")
        if not target.is_file():
            raise Invalid(f"{sig.name}: {target.name} is missing")
        try:
            verify_file(target, sig.read_text(), key)
        except Invalid as e:
            raise Invalid(f"{target.name}: {e}") from None
        print(f"ok  {target.name}")
    return len(sigs)


HEADLESS = [
    "x86_64-unknown-linux-gnu.tar.gz",
    "aarch64-unknown-linux-gnu.tar.gz",
    "x86_64-pc-windows-msvc.zip",
    "aarch64-apple-darwin.tar.gz",
    "x86_64-apple-darwin.tar.gz",
]

# Updater bundles, each signed: what installed copies download.
SIGNED = [
    "stream-delay_{v}_amd64.AppImage",
    "stream-delay_{v}_amd64.deb",
    "stream-delay-{v}-1.x86_64.rpm",
    "stream-delay_{v}_x64-setup.exe",
    "stream-delay_{v}_x64_en-US.msi",
    "stream-delay_universal.app.tar.gz",
]
INSTALLERS = SIGNED + ["stream-delay_{v}_universal.dmg"]

# Every entry installed copies may use, and the bundle it must give. An installed
# copy asks for "<os>-<arch>-<installer>" first (how it was installed: deb, rpm,
# appimage, msi, nsis, app), then for "<os>-<arch>", so both kinds are checked,
# and no other entry is accepted.
PLATFORMS = {
    "linux-x86_64": "stream-delay_{v}_amd64.AppImage",
    "linux-x86_64-appimage": "stream-delay_{v}_amd64.AppImage",
    "linux-x86_64-deb": "stream-delay_{v}_amd64.deb",
    "linux-x86_64-rpm": "stream-delay-{v}-1.x86_64.rpm",
    "windows-x86_64": "stream-delay_{v}_x64_en-US.msi",
    "windows-x86_64-msi": "stream-delay_{v}_x64_en-US.msi",
    "windows-x86_64-nsis": "stream-delay_{v}_x64-setup.exe",
    "darwin-aarch64": "stream-delay_universal.app.tar.gz",
    "darwin-aarch64-app": "stream-delay_universal.app.tar.gz",
    "darwin-x86_64": "stream-delay_universal.app.tar.gz",
    "darwin-x86_64-app": "stream-delay_universal.app.tar.gz",
}


def check_release(root: Path, tag: str, repository: str, key):
    version = tag.removeprefix("v")
    names = {p.name for p in root.iterdir() if p.is_file()}
    expected = [f"streamdelayd-v{version}-{t}" for t in HEADLESS]
    expected += [n.format(v=version) for n in INSTALLERS]
    expected += [n.format(v=version) + ".sig" for n in SIGNED]
    expected.append("latest.json")
    missing = [n for n in expected if n not in names]
    if missing:
        raise Invalid("missing assets: " + ", ".join(missing))
    check_signatures(root, key)

    try:
        manifest = json.loads((root / "latest.json").read_text())
    except ValueError as e:
        raise Invalid(f"latest.json does not parse: {e}") from None
    if manifest.get("version") != version:
        raise Invalid(f"latest.json offers version {manifest.get('version')!r}, not {version}")
    platforms = manifest.get("platforms") or {}
    unexpected = sorted(set(platforms) - set(PLATFORMS))
    if unexpected:
        raise Invalid("latest.json has entries this check does not know: " + ", ".join(unexpected))
    for platform, bundle in PLATFORMS.items():
        bundle = bundle.format(v=version)
        entry = platforms.get(platform)
        if not entry:
            raise Invalid(f"latest.json has no update for {platform}")
        url = f"https://github.com/{repository}/releases/download/{tag}/{bundle}"
        if entry.get("url") != url:
            raise Invalid(f"latest.json points {platform} at {entry.get('url')!r}, not {url}")
        if entry.get("signature", "").strip() != (root / f"{bundle}.sig").read_text().strip():
            raise Invalid(f"latest.json has another signature for {platform} than {bundle}.sig")
    print(f"ok  latest.json offers {version} to {', '.join(PLATFORMS)}")


def main(argv) -> int:
    try:
        if len(argv) == 3 and argv[1] == "signatures":
            n = check_signatures(Path(argv[2]), key_from_env())
            print(f"{n} signatures verified")
        elif len(argv) == 5 and argv[1] == "release":
            check_release(Path(argv[2]), argv[3], argv[4], key_from_env())
        else:
            print(__doc__, file=sys.stderr)
            return 2
    except Invalid as e:
        print(f"::error::{e}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
