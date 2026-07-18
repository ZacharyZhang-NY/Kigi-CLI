# Release checklist (PRD F8)

Kigi ships as a single-file binary for five targets, published on this
repo's GitHub Releases by `.github/workflows/release.yml`. Users install via
`install.sh` / `install.ps1` and stay current through the in-app
self-updater (`kigi-update`), which resolves the same Releases API.

## Cutting a release

1. Bump `[workspace.package] version` in `Cargo.toml`; land the change on
   `main` with green CI.
2. Regenerate the third-party notices (not enforced by CI — this is the
   step that keeps `THIRD-PARTY-NOTICES.md` fresh):

   ```sh
   cargo install cargo-about --locked   # once
   cargo about generate about.hbs -o THIRD-PARTY-NOTICES.md
   ```

   Commit the result if it changed.
3. Tag and push:

   ```sh
   git tag vX.Y.Z && git push origin vX.Y.Z
   ```

   The tag must equal the workspace version (`vX.Y.Z` ↔ `X.Y.Z`); the
   workflow fails fast on a mismatch.
4. The `Release` workflow builds all five targets with the hardened
   `release-dist` profile, packages
   `kigi-<version>-<target-triple>.{tar.gz|zip}` archives (binary +
   LICENSE + NOTICE + THIRD-PARTY-NOTICES), generates `SHA256SUMS`, and
   publishes the GitHub Release. Tags containing `-` (e.g.
   `v0.2.0-alpha.1`) publish as pre-releases, which only the `alpha`
   update channel picks up.
5. Smoke-test an installed artifact:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/ZacharyZhang-NY/Kigi-CLI/main/install.sh | sh
   ~/.kigi/bin/kigi --version
   ```

## Invariants to keep in lockstep

- Asset naming `kigi-<version>-<target-triple>.{tar.gz|zip}` and the
  `SHA256SUMS` manifest are consumed by three clients: `install.sh`,
  `install.ps1`, and `auto_update::release_asset_name()` in
  `crates/codegen/kigi-update`. Change one, change all (the kigi-update
  test `test_release_asset_name_matches_release_workflow_naming` pins the
  Rust side).
- Never publish two builds of the same semver version differing only in
  build metadata (`+…`) — the `semver` crate orders build metadata, so
  auto-update would bounce users between them.
- Rollbacks: deleting the bad release (or re-pointing "latest") is enough —
  the internal installer treats the Releases API as authoritative and
  downgrades clients on its own.
- No PyPI/npm packages, ever; in particular never squat the `kimi-cli`
  package name (PRD F8).
