"""Tests for verify_release.py. Run: python3 -m unittest discover -s .github/scripts"""

import base64
import hashlib
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
import verify_release as v  # noqa: E402

REPO = "owner/stream-delay"
TAG = "v1.2.3"
VERSION = "1.2.3"


# ----- a minisign signer, as Tauri's, for fixtures (RFC 8032 signing) -------------


def _encode(point) -> bytes:
    x, y, z, _ = point
    zi = pow(z, v.P - 2, v.P)
    x, y = x * zi % v.P, y * zi % v.P
    return (y | ((x & 1) << 255)).to_bytes(32, "little")


class Key:
    def __init__(self, seed: bytes, key_id: bytes):
        h = hashlib.sha512(seed).digest()
        a = int.from_bytes(h[:32], "little")
        a &= (1 << 254) - 8
        a |= 1 << 254
        self.a, self.prefix, self.key_id = a, h[32:], key_id
        self.public = _encode(v._mul(a, v.BASE))

    def sign(self, message: bytes) -> bytes:
        r = int.from_bytes(hashlib.sha512(self.prefix + message).digest(), "little") % v.L
        big_r = _encode(v._mul(r, v.BASE))
        k = int.from_bytes(hashlib.sha512(big_r + self.public + message).digest(), "little") % v.L
        return big_r + ((r + k * self.a) % v.L).to_bytes(32, "little")

    def pubkey_b64(self) -> str:
        text = "untrusted comment: minisign public key\n" + base64.b64encode(
            b"Ed" + self.key_id + self.public
        ).decode() + "\n"
        return base64.b64encode(text.encode()).decode()

    def sig_file(self, data: bytes, name: str) -> str:
        sig = self.sign(hashlib.blake2b(data, digest_size=64).digest())
        trusted = f"timestamp:1\tfile:{name}"
        text = "\n".join(
            [
                "untrusted comment: signature from tauri secret key",
                base64.b64encode(b"ED" + self.key_id + sig).decode(),
                f"trusted comment: {trusted}",
                base64.b64encode(self.sign(sig + trusted.encode())).decode(),
                "",
            ]
        )
        return base64.b64encode(text.encode()).decode()


KEY = Key(b"\x01" * 32, b"12345678")
OTHER = Key(b"\x02" * 32, b"87654321")


def make_release(root: Path, key: Key = KEY) -> None:
    for target in v.HEADLESS:
        (root / f"streamdelayd-v{VERSION}-{target}").write_bytes(os.urandom(64))
    for name in v.INSTALLERS:
        (root / name.format(v=VERSION)).write_bytes(os.urandom(256))
    for name in v.SIGNED:
        name = name.format(v=VERSION)
        (root / f"{name}.sig").write_text(key.sig_file((root / name).read_bytes(), name))
    platforms = {}
    for platform, bundle in v.PLATFORMS.items():
        bundle = bundle.format(v=VERSION)
        platforms[platform] = {
            "signature": (root / f"{bundle}.sig").read_text(),
            "url": f"https://github.com/{REPO}/releases/download/{TAG}/{bundle}",
        }
    (root / "latest.json").write_text(json.dumps({"version": VERSION, "platforms": platforms}))


class Ed25519(unittest.TestCase):
    def test_rfc8032_vectors(self):
        for seed, public, message, signature in [
            (
                "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
                "",
                "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
            ),
            (
                "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
                "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
                "72",
                "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
            ),
        ]:
            key = Key(bytes.fromhex(seed), b"\0" * 8)
            self.assertEqual(key.public.hex(), public)
            self.assertEqual(key.sign(bytes.fromhex(message)).hex(), signature)
            self.assertTrue(v.ed25519_verify(key.public, bytes.fromhex(message), bytes.fromhex(signature)))
            self.assertFalse(v.ed25519_verify(key.public, bytes.fromhex(message) + b"x", bytes.fromhex(signature)))


class Release(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.root = Path(self.dir.name)
        make_release(self.root)
        self.key = v.load_public_key(KEY.pubkey_b64())

    def tearDown(self):
        self.dir.cleanup()

    def check(self):
        v.check_release(self.root, TAG, REPO, self.key)

    def fails(self, message):
        with self.assertRaises(v.Invalid) as e:
            self.check()
        self.assertIn(message, str(e.exception))

    def edit_manifest(self, edit):
        path = self.root / "latest.json"
        manifest = json.loads(path.read_text())
        edit(manifest)
        path.write_text(json.dumps(manifest))

    def test_a_complete_release_passes(self):
        self.check()

    def test_updates_come_from_exactly_this_release(self):
        right = f"https://github.com/{REPO}/releases/download/{TAG}/stream-delay_{VERSION}_amd64.AppImage"
        original = (self.root / "latest.json").read_text()
        for wrong in [
            right.replace("https://github.com/", "https://wrong-host.invalid/"),
            right.replace("https://", "http://"),
            right.replace(REPO, "someone/else"),
            right.replace(f"/{TAG}/", "/v9.9.9/"),
            right + "?x",
        ]:
            with self.subTest(wrong):
                (self.root / "latest.json").write_text(original)
                self.edit_manifest(lambda m: m["platforms"]["linux-x86_64"].update(url=wrong))
                self.fails("latest.json points linux-x86_64")

    def test_both_windows_installers_are_offered(self):
        msi = f"https://github.com/{REPO}/releases/download/{TAG}/stream-delay_{VERSION}_x64_en-US.msi"
        self.edit_manifest(lambda m: m["platforms"]["windows-x86_64-nsis"].update(url=msi))
        self.fails("windows-x86_64-nsis")

    def test_every_platform_is_offered(self):
        original = (self.root / "latest.json").read_text()
        for platform in v.PLATFORMS:
            with self.subTest(platform):
                (self.root / "latest.json").write_text(original)
                self.edit_manifest(lambda m: m["platforms"].pop(platform))
                self.fails(f"no update for {platform}")

    def test_installer_specific_entries_are_checked(self):
        # Installed copies ask for these before the plain "<os>-<arch>" ones.
        original = (self.root / "latest.json").read_text()
        appimage = f"https://github.com/{REPO}/releases/download/{TAG}/stream-delay_{VERSION}_amd64.AppImage"
        for platform in ["linux-x86_64-deb", "linux-x86_64-rpm", "windows-x86_64-msi", "darwin-aarch64-app"]:
            for edit in [
                lambda m: m["platforms"][platform].update(url="https://example.invalid/wrong"),
                lambda m: m["platforms"][platform].update(url=appimage),
                lambda m: m["platforms"][platform].update(signature="invalid"),
            ]:
                with self.subTest(platform):
                    (self.root / "latest.json").write_text(original)
                    self.edit_manifest(edit)
                    self.fails(platform)

    def test_unknown_entries_are_refused(self):
        self.edit_manifest(
            lambda m: m["platforms"].update(
                {"linux-aarch64": {"url": "https://example.invalid/x", "signature": "x"}}
            )
        )
        self.fails("does not know: linux-aarch64")

    def test_the_manifest_offers_this_version(self):
        self.edit_manifest(lambda m: m.update(version="1.2.2"))
        self.fails("offers version")

    def test_changed_installers_are_caught(self):
        path = self.root / f"stream-delay_{VERSION}_amd64.deb"
        path.write_bytes(path.read_bytes() + b"!")
        self.fails("does not match the file")

    def test_changed_signatures_are_caught(self):
        path = self.root / f"stream-delay_{VERSION}_x64-setup.exe.sig"
        lines = base64.b64decode(path.read_text()).decode().split("\n")
        raw = bytearray(base64.b64decode(lines[1]))
        raw[20] ^= 1
        lines[1] = base64.b64encode(bytes(raw)).decode()
        path.write_text(base64.b64encode("\n".join(lines).encode()).decode())
        self.fails("does not match the file")

    def test_the_manifest_signature_is_the_bundles(self):
        other = (self.root / f"stream-delay_{VERSION}_amd64.deb.sig").read_text()
        self.edit_manifest(lambda m: m["platforms"]["linux-x86_64"].update(signature=other))
        self.fails("another signature for linux-x86_64")

    def test_another_key_is_refused(self):
        make_release(self.root, OTHER)
        self.fails("signed with key")

    def test_every_asset_is_there(self):
        (self.root / f"streamdelayd-v{VERSION}-x86_64-pc-windows-msvc.zip").unlink()
        self.fails("missing assets")

    def test_signatures_mode_needs_signatures(self):
        empty = self.root / "empty"
        empty.mkdir()
        with self.assertRaises(v.Invalid):
            v.check_signatures(empty, self.key)
        self.assertEqual(v.check_signatures(self.root, self.key), len(v.SIGNED))


class PinnedKey(unittest.TestCase):
    """apps/desktop/updater.pub, which every release is built and checked with."""

    def test_is_the_key_installed_copies_trust(self):
        path = Path(__file__).parents[2] / "apps" / "desktop" / "updater.pub"
        key_id, _ = v.load_public_key(path.read_text())
        # The key of every release so far (minisign shows the id reversed).
        # Changing it strands every installed copy: see docs/RELEASING.md.
        self.assertEqual(key_id[::-1].hex().upper(), "A9CBDB55664BEBFD")


if __name__ == "__main__":
    unittest.main()
