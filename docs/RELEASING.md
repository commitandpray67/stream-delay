# Releasing

Releases are built by [`.github/workflows/release.yml`](../.github/workflows/release.yml) when a tag `v*` is pushed. The workflow creates a **draft pre-release** containing:

- Desktop installers from `tauri-action`: Windows NSIS and MSI, a universal macOS DMG/app, and Linux AppImage, deb and rpm.
- Headless `streamdelayd` archives for Linux (x86_64, aarch64), Windows and macOS (Apple Silicon, Intel).
- `SHA256SUMS.txt` covering every asset.
- A multi-arch container image `ghcr.io/<owner>/stream-delay:<version>` (and `:edge`).

Review the draft, then publish it.

## Checklist

1. Update `version` in `Cargo.toml` (workspace), `apps/desktop/src-tauri/tauri.conf.json`, `ui/package.json` and `apps/desktop/package.json`.
2. Make sure CI is green on `main`, including the end-to-end job.
3. Test on a real Twitch account with `?bandwidthtest=true` on each OS you can reach, following [`docs/testing.md`](testing.md), and run the 12-hour soak (`DURATION=43200 tests/soak/run.sh`).
4. Move the `Unreleased` section of `CHANGELOG.md` under the new version and date.
5. `git tag v0.x.y && git push origin v0.x.y`.
6. Paste the changelog entry into the draft release notes, then publish.

## One-time setup

### Auto-update signing (recommended)

Tauri's updater only installs updates signed with your key.

```sh
pnpm -C apps/desktop tauri signer generate -w ~/.tauri/stream-delay.key
```

- Add the private key as the repository **secret** `TAURI_SIGNING_PRIVATE_KEY`, and its password as `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
- Add the public key as the repository **variable** `TAURI_UPDATER_PUBKEY`.

Once these are set, releases include `latest.json` and signed update bundles. The app checks `releases/latest/download/latest.json` on start and from the tray menu. Without them the app builds fine and simply has no updater.

### macOS signing and notarization

This needs an Apple Developer ID (99 USD/year; the plan is to fund it through sponsorship). Set these secrets and tauri-action signs and notarizes automatically:

- `APPLE_CERTIFICATE` (base64 .p12)
- `APPLE_CERTIFICATE_PASSWORD`
- `APPLE_SIGNING_IDENTITY`
- `APPLE_ID`
- `APPLE_PASSWORD` (an app-specific password)
- `APPLE_TEAM_ID`

Until then, macOS users must right-click → Open the first time.

### Windows signing

Apply to the [SignPath Foundation](https://signpath.org/) (free for open source), then add their GitHub Action after the desktop job to sign the `.exe`/`.msi` assets. Unsigned builds work but show a SmartScreen warning.
