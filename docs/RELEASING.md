# Releasing

Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed. Why it works this way: see the [packaging ADR](./adr/202610100804-package-with-cargo-packager-and-update-from-github-releases.md).

## One-time setup

1. **Updater key.** The key pair lives in `~/.savoia/` on the maintainer's machine. `updater.key` is the private key, and `updater.key.password` is its password. The public half is committed as `crates/savoia-app/updater.pub`, and tag builds compile it in. Back up `~/.savoia/` outside GitHub: losing the private key strands every installed copy on its version. Store the private half as repository secrets with an account that has admin rights on the repository:

   ```sh
   gh secret set CARGO_PACKAGER_SIGN_PRIVATE_KEY -R joaoGabriel55/savoia-studio < ~/.savoia/updater.key
   gh secret set CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD -R joaoGabriel55/savoia-studio < ~/.savoia/updater.key.password
   ```

   A tag build fails if these secrets are missing. To rotate the key, generate a new pair with `cargo packager signer generate --path ~/.savoia/updater.key`, commit the new `updater.pub`, and reset both secrets. Copies installed before the rotation can't verify newer releases, so their users must download the new version by hand once.
2. **GitHub Pages.** In Settings › Pages, set the source to *GitHub Actions*. The docs site deploys on the next push to `main` that touches `docs/site/`.
3. **Code signing** is off for v0.1, so macOS and Windows warn on first launch (the install docs explain how to get past it).
   - **macOS:** add the secrets `APPLE_CERTIFICATE` (a base64 `.p12`), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (an app-specific password) and `APPLE_TEAM_ID`. The workflow signs and notarizes as soon as they exist.
   - **Windows:** needs a certificate (SignPath for open source, or Azure Trusted Signing) and a `signCommand` under `[package.metadata.packager.windows]`.

Pull requests that change packaging build every installer, unsigned, as a check before a tag.

## Each release

Versions and release notes come from [Changesets](https://github.com/changesets/changesets); see [the ADR](./adr/202610101305-version-releases-with-changesets.md).

Before merging the Release PR, `npm run release:check` shows the current version, the one about to be released, and each commit since the last tag with its guessed bump (chore, patch, minor or major, from its message and the files it touched). It exits 1 when `package.json`, `Cargo.toml` and `Cargo.lock` disagree, or when commits look bigger than the pending changesets, such as a fix or a feature with no changeset. The guess is a check, not the source: the changesets still decide the version.

1. **In each PR users will notice,** run `npx changeset` (after `npm ci` once). Pick patch, minor or major and write one line for the changelog, then commit the generated `.changeset/*.md`. CI warns when a PR has none.
2. **After merging to `main`,** the Changesets workflow opens or updates the **Release Savoia Studio** PR. It bumps `package.json`, `Cargo.toml` and `Cargo.lock` and writes `CHANGELOG.md`. Review the version and the notes.
3. **Merge the Release PR.** The workflow tags `vX.Y.Z` and starts the release build (about 25 minutes).
4. **Review the draft release:** every installer, the `.sig` files and `latest.json`. Then click **Publish release**. Installed copies see the update only after it is published.

One-time setting: in Settings › Actions › General › Workflow permissions, tick *Allow GitHub Actions to create and approve pull requests*, or step 2 can't open the PR.

Don't edit versions by hand: `package.json` is the source and `scripts/sync-version.mjs` copies it into Cargo. To release by hand anyway (for example, to rebuild a tag), run the Release workflow from the Actions tab and choose the tag as the ref.

## Building an installer locally

```sh
cargo install cargo-packager --locked
cd crates/savoia-app
cargo packager --release --formats app,dmg   # or nsis,wix / deb,appimage
```

The packages land in `target/packages/`. Without `SAVOIA_UPDATER_PUBKEY` the build has no update key and never checks for updates.

## Testing an update locally

`SAVOIA_UPDATER_ENDPOINT` replaces the manifest URL at build time, so an older build can update itself from a local server:

1. Generate a throwaway key: `cargo packager signer generate --path /tmp/test.key`.
2. Bump the version to 0.1.1. Build with `SAVOIA_UPDATER_PUBKEY="$(cat /tmp/test.key.pub)"`, `SAVOIA_UPDATER_ENDPOINT=http://127.0.0.1:8765/latest.json` and the `CARGO_PACKAGER_SIGN_PRIVATE_KEY*` variables set: `cargo packager --release --formats app`.
3. Put the `.app.tar.gz`, its `.sig`, and a `latest.json` from `scripts/release-manifest.py` (with its URLs pointed at the local server) into a folder, and serve it with `python3 -m http.server 8765`.
4. Set the version back to 0.1.0, build the same way without the signing variables, copy the `.app` somewhere, and open it. It offers 0.1.1, and **Install and restart** swaps the bundle.

On 2026-10-10 this worked on macOS. Appending bytes to the served package made it fail with "The signature verification failed", and 0.1.0 stayed installed.
