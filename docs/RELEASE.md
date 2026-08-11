# Release checklist (PRD F8)

Kigi ships as a single-file binary for five targets, published on this
repo's Gitea Releases by `.gitea/workflows/release.yml`. Users install via
`install.sh` / `install.ps1` and stay current through the in-app
self-updater (`kigi-update`), which resolves the same Releases API.

`.github/workflows/release.yml` is the retired GitHub pipeline. Gitea ignores
`.github/workflows` entirely whenever `.gitea/workflows` is non-empty, so it
does not run — it is kept only as a reference for the GitHub-hosted layout.

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
   publishes the Gitea Release. Tags containing `-` (e.g.
   `v0.2.0-alpha.1`) publish as pre-releases, which only the `alpha`
   update channel picks up.

   The release is created as a **draft** and only flipped public once
   `SHA256SUMS` is attached, so `releases/latest` never hands the installer
   a release whose checksum manifest is still missing. A failed run leaves
   the draft in place; re-pushing the tag reuses it rather than erroring on
   a duplicate `tag_name`.
5. Smoke-test an installed artifact:

   ```sh
   curl -fsSL https://kigicli.dev/install.sh | sh
   ~/.kigi/bin/kigi --version
   ```

## Build runners

Gitea has no hosted runners, so all three `runs-on` labels below are machines
we register ourselves with `gitea-runner` (the binary formerly called
`act_runner`). Labels are declared in the runner's `config.yaml` as
`<label>:host` (run directly on the machine) or `<label>:docker://<image>` (run
in a container) — with `gitea-runner` v2, `register --labels` is ignored
whenever a config file supplies them.

| Label            | Machine                        | Mode   | Builds                                             |
| ---------------- | ------------------------------ | ------ | -------------------------------------------------- |
| `ubuntu-latest`  | the Dokploy box                | docker | `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` |
| `macos-arm64`    | Mac Mini (M4)                  | host   | `aarch64-apple-darwin`, `x86_64-apple-darwin`      |
| `windows-x86_64` | ZACHARYZHANG-PC                | host   | `x86_64-pc-windows-msvc`                           |

The Linux legs use `ubuntu-latest` because that is what the Dokploy box's
runner already advertises. That image is a bare runner-image, so those legs
provision their own apt packages and rustup per run (`rustup show` then picks
up the `rust-toolchain.toml` pin). If you register a dedicated
`linux-x86_64:docker://…` runner later, change the three Linux `runs-on`
values back to match it.

**The Mac is not optional.** Apple's SDK licence limits macOS builds to
Apple-branded hardware, and the Darwin targets need the real Apple linker
anyway (`.cargo/config.toml` passes `-ObjC` and `-undefined dynamic_lookup`).
There is no supported way to produce our macOS artifacts on the Linux server.
One Apple Silicon Mac covers both Darwin targets — `x86_64-apple-darwin` is a
cross-compile against the same SDK, exactly as the old GitHub `macos-14`
runner did it.

Windows is a softer requirement but still wants a real machine: the tree links
`ring`, `mimalloc`, `zstd-sys` and `libz-sys` against a static MSVC CRT
(`-C target-feature=+crt-static`). Cross-compiling that from Linux with
`cargo-xwin` is possible but means debugging four C builds under `clang-cl`;
a Windows runner with the VS 2022 C++ Build Tools reproduces today's binary
exactly.

Linux arm64 *is* cross-compiled on the x86_64 box — the workflow installs
`gcc-aarch64-linux-gnu` and points `cc-rs` at it. The one thing this loses is
the ability to smoke-test the arm64 binary before publishing (the old GitHub
pipeline did not test it either). Register a native arm64 runner and change
that matrix leg's `runner:` if you want it built natively.

### Registering a runner

Get a registration token from **Settings → Actions → Runners** (repo scope),
your user settings (all your repos) or the site admin panel (instance-wide).
Over the API it is a **POST**, not a GET — a GET is parsed as runner id `0` and
404s:

```sh
curl -fsS -X POST -H "Authorization: token $GITEA_API_TOKEN" \
  https://git.zacharyzhang.com/api/v1/user/actions/runners/registration-token
```

Write the labels into `config.yaml` first (see `runner.labels` in
`gitea-runner generate-config`), then register on each machine:

```sh
# Linux (the Dokploy box) — container mode
gitea-runner --config config.yaml register --no-interactive \
  --instance https://git.zacharyzhang.com \
  --token <REGISTRATION_TOKEN> \
  --name kigi-linux-x86_64

# macOS (Apple Silicon) — host mode, needs Xcode CLT + rustup + Node on PATH
gitea-runner --config config.yaml register --no-interactive \
  --instance https://git.zacharyzhang.com \
  --token <REGISTRATION_TOKEN> \
  --name kigi-macos-arm64

# Windows — host mode, needs VS 2022 Build Tools + rustup + Node on PATH
gitea-runner --config config.yaml register --no-interactive `
  --instance https://git.zacharyzhang.com `
  --token <REGISTRATION_TOKEN> `
  --name kigi-windows-x86_64
```

Host-mode runners execute steps with the daemon's own environment, so each
needs its build prerequisites installed up front — and the supervisor that
launches the daemon must put them on `PATH`:

- **macOS**: `xcode-select --install`, `rustup`, and Node (for
  `actions/checkout`, which is a JavaScript action).
- **Windows**: VS 2022 Build Tools with the "Desktop development with C++"
  workload, `rustup`, Git for Windows, and Node. Every Windows step in the
  workflow uses `shell: powershell` (5.1) — never `bash`, which on a normal
  Windows box resolves to WSL.
- **Linux (docker mode)**: nothing on the host — the workflow installs
  `build-essential`, `jq`, `unzip`, protoc, rustup and the cross toolchain
  inside the container per run.

### Keeping the runners alive

Each host-mode runner is supervised so it survives reboots and crashes. Both
supervisors export `CARGO_HOME`/`RUSTUP_HOME` and prepend the toolchain, git
and node directories to `PATH`, because host-mode jobs inherit exactly the
daemon's environment.

| Machine  | Supervisor                                    | Manage with                                            |
| -------- | --------------------------------------------- | ------------------------------------------------------ |
| Windows  | Task Scheduler task `Gitea Runner (Kigi)` running `C:\Users\zhang\gitea-runner\run-runner.ps1` | `Get-ScheduledTask`, `Start-ScheduledTask`, `Stop-ScheduledTask` |
| macOS    | LaunchAgent `com.kigi.gitea-runner` (`KeepAlive`) | `launchctl kickstart -k gui/$(id -u)/com.kigi.gitea-runner` |

`prompt.txt` at the repo root is a one-shot setup prompt for the Mac Mini leg.
Both runners also expose a liveness probe on `http://127.0.0.1:9101/healthz`.

### Gitea-specific things that differ from GitHub

- Workflows live in `.gitea/workflows/`. Once that directory is non-empty,
  `.github/workflows/` is ignored completely — you cannot run both.
- The automatic token is `secrets.GITEA_TOKEN`, and the release steps call the
  Gitea API directly (`POST /releases`, `POST /releases/{id}/assets`,
  `PATCH /releases/{id}`) rather than `softprops/action-gh-release`, which
  targets GitHub's API and does not work here.
- `uses:` actions are fetched from `github.com` by default
  (`DEFAULT_ACTIONS_URL`), so the runners need outbound access to it — our
  GitHub *account* being suspended does not affect downloading public actions.
  Absolute URLs (`uses: https://gitea.com/actions/checkout@v4`) work too.
- Artifacts carry only the five checksum lines; the archives themselves go
  straight to the release. If `actions/upload-artifact@v3` misbehaves on the
  instance, the fallback is to drop the artifact steps and have `publish`
  download the archives from the release and hash them there.

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
