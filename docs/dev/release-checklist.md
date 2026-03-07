# Release Checklist

## Current release shape

- `CI` runs on PRs and pushes to `main` via [ci.yml](/Users/andrew/code/nanograph/.github/workflows/ci.yml). It covers `cargo check --workspace --all-targets`, TS SDK tests, the TS consumer smoke test, and Swift SDK tests.
- `Release` runs on tag pushes via [release.yml](/Users/andrew/code/nanograph/.github/workflows/release.yml). It currently automates:
  - macOS ARM CLI binary build + `.sha256`
  - Swift XCFramework build + `.sha256`
  - render + smoke test of a publishable Swift package from monorepo sources
  - GitHub Release creation
  - Homebrew tap update dispatch
- `Release` does **not** currently publish crates.io packages, npm, or an external `nanograph-swift` repo. Those remain manual.

## Pre-release

- [ ] CI workflow green on `main` / PRs (`cargo check`, TS SDK tests, TS consumer smoke test, Swift SDK tests)
- [ ] All tests pass: `cargo test`
- [ ] CLI e2e pass: `cargo test -p nanograph-cli`
- [ ] Clippy clean: `cargo clippy --workspace --all-targets`
- [ ] Bump version in all Cargo.toml files (currently lockstep: `nanograph`, `nanograph-cli`, `nanograph-ffi`, `nanograph-ts`)
- [ ] Bump version in `crates/nanograph-ts/package.json`
- [ ] Update cross-references (`nanograph = { path = "../nanograph", version = "X.Y.Z" }`) in nanograph-cli, nanograph-ffi, nanograph-ts
- [ ] Confirm the TS package still points at `types.d.ts` and that the npm tarball is sane (`npm pack --dry-run` in `crates/nanograph-ts`)
- [ ] Commit: `release: X.Y.Z — <summary>`

## Publish

### 1. Tag and push (triggers GitHub Actions release workflow)

```bash
git tag vX.Y.Z
git push origin main --tags
```

This automatically:
- Builds macOS ARM binary on `macos-14` runner
- Builds Swift XCFramework artifacts for macOS (`NanoGraphFFI.xcframework.zip` + checksum)
- Renders a publishable Swift package from monorepo sources and smoke-tests it with `swift test`
- Creates GitHub Release with `nanograph-vX.Y.Z-aarch64-apple-darwin.tar.gz` + `.sha256`
- Dispatches formula update to `nanograph/homebrew-tap`

### 2. crates.io (publish `nanograph` first, then the dependents)

```bash
cargo publish -p nanograph
cargo publish -p nanograph-cli
cargo publish -p nanograph-ffi
cargo publish -p nanograph-ts
```

Wait for the new `nanograph` version to become visible in the crates.io index before publishing `nanograph-cli`, `nanograph-ffi`, and `nanograph-ts`.

### 3. npm

```bash
cd crates/nanograph-ts
npm publish --otp=<code>
```

### 4. Swift distribution repo (`nanograph-swift`) — only if/when it exists

This is not automated yet. If the external Swift package repo exists, update it from the monorepo release outputs:

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
- [ ] If `nanograph-swift` exists: verify its `Package.swift` points at the new GitHub Release asset URL + checksum and a clean SPM consumer still builds

## Assets

| Asset | Location |
|-------|----------|
| GitHub Release | `github.com/aaltshuler/nanograph/releases` |
| macOS ARM binary | `nanograph-vX.Y.Z-aarch64-apple-darwin.tar.gz` on release |
| Swift XCFramework | `NanoGraphFFI.xcframework.zip` on release |
| Homebrew tap | `github.com/nanograph/homebrew-tap` |
| crates.io (core) | `crates.io/crates/nanograph` |
| crates.io (CLI) | `crates.io/crates/nanograph-cli` |
| crates.io (FFI) | `crates.io/crates/nanograph-ffi` |
| crates.io (TS) | `crates.io/crates/nanograph-ts` |
| npm | `npmjs.com/package/nanograph-db` |
| Swift package repo (optional) | `github.com/nanograph/nanograph-swift` |

## Infrastructure

| Component | Repo / Config |
|-----------|---------------|
| CI workflow | `.github/workflows/ci.yml` |
| Release workflow | `.github/workflows/release.yml` |
| Swift XCFramework build | `tools/swift-package/build_xcframework.sh` |
| Swift package renderer | `tools/swift-package/render_package.sh` |
| Homebrew tap | `nanograph/homebrew-tap` (GitHub org) |
| Tap update workflow | `homebrew-tap/.github/workflows/update-formula.yml` |
| `HOMEBREW_TAP_TOKEN` | Secret on `aaltshuler/nanograph` — fine-grained PAT with Contents write to `nanograph/homebrew-tap` |
