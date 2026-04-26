# Release Checklist

## Current release shape

- `CI` runs on PRs and pushes to `main` via [ci.yml](/Users/andrew/code/nanograph/.github/workflows/ci.yml). It is split into parallel jobs:
  - `Core Rust` on Blacksmith Linux for workspace `cargo check`, core tests, workspace clippy, and the Rust `nanograph-ffi` crate build
  - `CLI E2E` on Blacksmith Linux for `cargo test -p nanograph-cli`
  - `TypeScript SDK` on Blacksmith Linux for `npm install`, `npm test`, and the TS consumer smoke test
  - `Swift SDK` on GitHub `macos-14` runners for a local `cargo build -p nanograph-ffi` plus `swift test`
- `Release` runs on tag pushes via [release.yml](/Users/andrew/code/nanograph/.github/workflows/release.yml). It currently automates:
  - macOS ARM CLI binary build + `.sha256` on `macos-14`
  - Swift XCFramework build for macOS arm64 + `.sha256` on `macos-14`
  - render + smoke test of a publishable Swift package from monorepo sources on `macos-14`
  - GitHub Release creation on Blacksmith Linux
  - Homebrew tap update dispatch on Blacksmith Linux
- `Publish Crates` runs on tag pushes (and supports `workflow_dispatch` for manual / dry-run triggers) via [publish-crates.yml](/Users/andrew/code/nanograph/.github/workflows/publish-crates.yml). It publishes `nanograph` first, waits for the crates.io index to surface the new version, then publishes `nanograph-cli`, `nanograph-ffi`, and `nanograph-ts`. Requires the `CARGO_REGISTRY_TOKEN` repo secret.
- `Release` does **not** currently publish npm or update the external `nanograph-swift` repo. Those remain manual.

## Pre-release

- [ ] Release branch state is correct before tagging:
  - release commit is on `main`
  - `git status --short` is empty after the release commit
- [ ] CI workflow green on `main` / PRs (`Core Rust`, `CLI E2E`, `TypeScript SDK`, `Swift SDK`)
- [ ] Local Rust release gate passes:
  - `cargo test --workspace --lib --bins --tests`
  - `cargo clippy --workspace --lib --bins --tests`
- [ ] CLI e2e pass locally: `cargo test -p nanograph-cli`
- [ ] Benchmarks stay separate from the normal release gate; run them manually when needed, not as part of the default release checklist
- [ ] Local SDK checks pass when the release touched those surfaces:
  - `cargo build -p nanograph-ffi`
  - `npm test` in `crates/nanograph-ts`
  - `bash tools/ts-sdk/smoke_test_consumer.sh`
  - `swift test` in `crates/nanograph-ffi/swift`
- [ ] Local release-artifact smoke checks pass when the release touched SDK/distribution surfaces:
  - `bash tools/swift-package/build_xcframework.sh`
  - `bash tools/swift-package/render_package.sh --output /tmp/nanograph-swift --version X.Y.Z --artifact-path "$PWD/target/swift-xcframework/NanoGraphFFI.xcframework"`
  - `(cd /tmp/nanograph-swift && swift test)`
- [ ] Bump version in all crate manifests (currently lockstep: `nanograph`, `nanograph-cli`, `nanograph-ffi`, `nanograph-ts`)
- [ ] Bump version in `crates/nanograph-ts/package.json` and refresh checked-in package metadata:
  - `package-lock.json`
- [ ] Update cross-references (`nanograph = { path = "../nanograph", version = "X.Y.Z" }`) in nanograph-cli, nanograph-ffi, nanograph-ts
- [ ] Confirm the TS package still points at `types.d.ts` and that the npm tarball is sane:
  - `npm pack --dry-run` in `crates/nanograph-ts`
  - regenerate the release tarball if you check it into the workspace or want a local artifact before publish
- [ ] Commit: `release: X.Y.Z — <summary>`

## Publish

### 1. Tag and push (triggers GitHub Actions release workflow)

```bash
git tag vX.Y.Z
git push origin main
git push origin vX.Y.Z
```

This automatically:
- Builds macOS ARM binary on `macos-14` runner
- Builds Swift XCFramework artifacts for macOS arm64 (`NanoGraphFFI.xcframework.zip` + checksum)
- Renders a publishable Swift package from monorepo sources and smoke-tests it with `swift test`
- Creates GitHub Release with `nanograph-vX.Y.Z-aarch64-apple-darwin.tar.gz` + `.sha256` on Blacksmith Linux
- Dispatches formula update to `nanograph/homebrew-tap` on Blacksmith Linux

### 2. crates.io

Automated by the `Publish Crates` workflow on tag push. It publishes `nanograph` first, polls the crates.io index until the new version is visible, then publishes `nanograph-cli`, `nanograph-ffi`, and `nanograph-ts`.

To dry-run or re-trigger manually:

```bash
gh workflow run publish-crates.yml -f dry_run=true   # dry run, no upload
gh workflow run publish-crates.yml                   # real publish from current main
```

If you ever need to fall back to manual:

```bash
cargo publish -p nanograph
# wait for the new version to be searchable on crates.io
cargo publish -p nanograph-cli
cargo publish -p nanograph-ffi
cargo publish -p nanograph-ts
```

### 3. npm

```bash
cd crates/nanograph-ts
npm publish --otp=<code>
```

### 4. Swift distribution repo (`nanograph-swift`)

This is not automated yet. Update the external Swift package repo from the monorepo release outputs:

- Point its `Package.swift` binary target at:
  - `https://github.com/nanograph/nanograph/releases/download/vX.Y.Z/NanoGraphFFI.xcframework.zip`
- Use the checksum from the matching release asset:
  - `NanoGraphFFI.xcframework.sha256`
- Sync the canonical header from:
  - `crates/nanograph-ffi/include/nanograph_ffi.h`
- Sync the Swift wrapper from:
  - `crates/nanograph-ffi/swift/Sources/NanoGraph/NanoGraph.swift`
- Run a clean external `swift build` / `swift test` smoke check before tagging that repo

## Post-release verification

- [ ] GitHub Release exists: `gh release view vX.Y.Z`
- [ ] Binary downloads: `gh release download vX.Y.Z --pattern '*.tar.gz'`
- [ ] Swift XCFramework assets exist: `gh release download vX.Y.Z --pattern 'NanoGraphFFI.xcframework*'`
- [ ] Homebrew formula updated: `gh api repos/nanograph/homebrew-tap/contents/Formula/nanograph.rb --jq '.content' | base64 -d | head -5`
- [ ] Brew install works: `brew install nanograph/tap/nanograph` (or `brew upgrade nanograph`)
- [ ] crates.io: `cargo search nanograph` shows new version
- [ ] npm: `npm view nanograph-db version` shows new version
- [ ] `nanograph-swift`: verify its `Package.swift` points at the new GitHub Release asset URL + checksum and a clean SPM consumer still builds

## Assets

| Asset | Location |
|-------|----------|
| GitHub Release | `github.com/nanograph/nanograph/releases` |
| macOS ARM binary | `nanograph-vX.Y.Z-aarch64-apple-darwin.tar.gz` on release |
| Swift XCFramework (macOS arm64) | `NanoGraphFFI.xcframework.zip` on release |
| Homebrew tap | `github.com/nanograph/homebrew-tap` |
| crates.io (core) | `crates.io/crates/nanograph` |
| crates.io (CLI) | `crates.io/crates/nanograph-cli` |
| crates.io (FFI) | `crates.io/crates/nanograph-ffi` |
| crates.io (TS) | `crates.io/crates/nanograph-ts` |
| npm | `npmjs.com/package/nanograph-db` |
| Swift package repo | `github.com/nanograph/nanograph-swift` |

## Infrastructure

| Component | Repo / Config |
|-----------|---------------|
| CI workflow | `.github/workflows/ci.yml` |
| Release workflow | `.github/workflows/release.yml` |
| Swift XCFramework build | `tools/swift-package/build_xcframework.sh` |
| Swift package renderer | `tools/swift-package/render_package.sh` |
| Homebrew tap | `nanograph/homebrew-tap` (GitHub org) |
| Tap update workflow | `homebrew-tap/.github/workflows/update-formula.yml` |
| `HOMEBREW_TAP_TOKEN` | Secret on `nanograph/nanograph` — fine-grained PAT with Contents write to `nanograph/homebrew-tap` |
