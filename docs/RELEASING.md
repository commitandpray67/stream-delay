# Releasing

Releases are built by [`.github/workflows/release.yml`](../.github/workflows/release.yml) when a tag `v*` is pushed. It first runs all of CI on the tagged commit and checks that the tag matches the version in `Cargo.toml`, `tauri.conf.json` and both `package.json` files, and that `CHANGELOG.md` has a section for it; nothing is built unless both pass. It then creates a **draft release** containing:

- Desktop installers from `tauri-action`: Windows NSIS and MSI, a universal macOS DMG/app, and Linux AppImage, deb and rpm.
- Headless `streamdelayd` archives for Linux (x86_64, aarch64), Windows and macOS (Apple Silicon, Intel).
- `latest.json` and update bundles signed with the project's updater key (below). A tag build fails without the key rather than release an app that could never update.
- `SHA256SUMS.txt` covering every asset. It is added last, after [`verify_release.py`](../.github/scripts/verify_release.py) has checked the draft: every installer and headless archive is there, every update bundle's signature verifies against `TAURI_UPDATER_PUBKEY`, and `latest.json` offers this version to every platform with those signatures, each downloaded from exactly `https://github.com/<owner>/<repo>/releases/download/<tag>/<file>`. CI runs the verifier's own tests on every push. Then [`smoke_desktop.py`](../.github/scripts/smoke_desktop.py) installs the draft's installers on clean Ubuntu 22.04, Ubuntu 24.04, Windows and macOS machines, as users would (the deb with apt, which also checks its dependencies; the AppImage; the setup `.exe` and the MSI; the app from the DMG and from the update bundle), starts each, and checks that it reports this version, serves the dashboard with the web UI, accepts RTMP connections and opens its window. A draft without `SHA256SUMS.txt` failed one of these checks: see the workflow run before publishing anything.

Review the draft, then publish it **as a normal release, not a pre-release**: the app looks for updates at `releases/latest`, which skips pre-releases. Say "beta" in the notes instead.

Publishing it runs [`publish-image.yml`](../.github/workflows/publish-image.yml), which pushes the multi-arch container image `ghcr.io/<owner>/stream-delay:<version>` (and `:edge`), packaged from the release's Linux archives after checking them against its `SHA256SUMS.txt`. Nothing is public before you publish. To push an image again, run that workflow by hand with the tag.

**Dry run:** a push to `main` or a `claude/**` branch that changes the release workflow, its verification script, `Dockerfile.release` or `tauri.conf.json` runs the same builds and install tests without publishing anything. The installers and binaries are attached to the workflow run as artifacts (Actions → the run → Artifacts), which is also a quick way to get a test build. Dry runs sign update bundles with a key generated for that run, never the real one, and verify every signature against it, so their installers cannot update to or from real releases.

## Checklist

1. Update `version` in `Cargo.toml` (workspace), `apps/desktop/src-tauri/tauri.conf.json`, `ui/package.json` and `apps/desktop/package.json`. MSI installers need a plain `x.y.z` version.
2. Make sure CI is green on `main`, including the end-to-end job, and the last release dry run passed. (The release runs CI again on the tagged commit.)
3. Run the 12-hour soak (`DURATION=43200 tests/soak/run.sh`) on the release commit's `streamdelayd`.
4. Move the `Unreleased` section of `CHANGELOG.md` under the new version and date.
5. `git tag v0.x.y && git push origin v0.x.y`.
6. When the workflow has finished, check that the draft has `SHA256SUMS.txt` (see above).
7. Install the draft's installers and test on a real Twitch account with `?bandwidthtest=true` on each OS you can reach, following [`docs/testing.md`](testing.md). Nothing is public yet: if something is wrong, delete the draft and the tag, fix it, and tag again.
8. Paste the changelog entry into the draft release notes, then publish (not as a pre-release). The container image follows.

## One-time setup

### The `release` environment (do this first)

The updater signing key is the most valuable secret in the project: anyone who has it can ship an "update" that every installed copy trusts. Repository secrets are readable by any workflow run, including one started by a branch push that edits the workflow, so the signing secrets live in an environment that only release tags can use:

1. **Settings → Environments → New environment**, name it `release`.
2. Under **Deployment branches and tags**, choose **Selected branches and tags** and add a **tag** rule `v*`. (Optionally add yourself under **Required reviewers**, so each release waits for your approval.)
3. Add the signing secrets below (`TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` and any `APPLE_*`) as **environment secrets** of `release`, and delete any repository-level copies under **Settings → Secrets and variables → Actions**.

Only the tag-triggered desktop job uses the `release` environment. `TAURI_UPDATER_PUBKEY` is public and stays a repository variable.

### Auto-update signing (required)

Tauri's updater only installs updates signed with your key, so installs of a release built without a key would never update themselves. Tag builds therefore fail when the key is not set up.

1. Create the key pair. It asks for a password; remember it.

   ```sh
   # macOS/Linux
   pnpm -C apps/desktop tauri signer generate -w ~/.tauri/stream-delay.key
   # Windows (PowerShell)
   pnpm -C apps/desktop tauri signer generate -w $HOME\.tauri\stream-delay.key
   ```

   This writes the private key to `stream-delay.key` and the public key to `stream-delay.key.pub`. Keep the private key and password safe (a password manager); anyone with both can publish updates your users will install.
2. In the GitHub repository:
   - **Settings → Environments → `release` → Environment secrets** → *Add secret*:
     - `TAURI_SIGNING_PRIVATE_KEY`: the whole content of `stream-delay.key`
       (`cat ~/.tauri/stream-delay.key`, or `Get-Content $HOME\.tauri\stream-delay.key` on Windows).
     - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: the password from step 1.
   - **Settings → Secrets and variables → Actions → Variables** tab → *New repository variable*:
     - `TAURI_UPDATER_PUBKEY`: the whole content of `stream-delay.key.pub`.

Releases then include `latest.json` and signed update bundles, and the app checks `releases/latest/download/latest.json` on start, from the tray menu and from the dashboard.

### macOS signing and notarization

This needs an Apple Developer ID (99 USD/year; the plan is to fund it through sponsorship). Set these as secrets of the `release` environment and tauri-action signs and notarizes automatically:

- `APPLE_CERTIFICATE` (base64 .p12)
- `APPLE_CERTIFICATE_PASSWORD`
- `APPLE_SIGNING_IDENTITY`
- `APPLE_ID`
- `APPLE_PASSWORD` (an app-specific password)
- `APPLE_TEAM_ID`

Until then, macOS users must right-click → Open the first time.

### Windows signing

Apply to the [SignPath Foundation](https://signpath.org/) (free for open source), then add their GitHub Action after the desktop job to sign the `.exe`/`.msi` assets. Unsigned builds work but show a SmartScreen warning.

### Protecting `main` and release tags

The release workflow checks the tagged commit, but only repository rules stop someone with write access from pushing to `main` or moving a tag. Under **Settings → Rules → Rulesets**:

1. A **branch** ruleset for the default branch: require a pull request (or at least block force pushes and deletion) and require the `CI` checks to pass.
2. A **tag** ruleset for `v*`: restrict creation, updates and deletion to maintainers, so a release tag cannot be moved to another commit after the fact.

Combined with the `release` environment's tag rule (and optionally its required reviewers), a release then needs a reviewed, CI-checked commit and a maintainer's tag, and publishes nothing until the draft is published by hand.
