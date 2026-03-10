---
title: Swift SDK Distribution
slug: swift-split-repo
status: planned
updated: 2026-03-06
---

# Swift SDK Distribution

Plan for isolating the Swift SDK into a publishable Swift Package with prebuilt binaries.

## Problem

The Swift Package at `crates/nanograph-ffi/swift/` requires consumers to:
1. Clone the entire nanograph monorepo
2. Install a Rust toolchain + `protoc`
3. Build the Rust FFI library in the workspace before `swift build`

`Package.swift` still links directly against workspace-relative build outputs:
```swift
.unsafeFlags(["-L", "../../../target/debug"], .when(configuration: .debug)),
.unsafeFlags(["-L", "../../../target/release"], .when(configuration: .release)),
```

This makes the package unusable as a normal SPM dependency.

## Target state

A separate `nanograph-swift` repo acting as a distribution repo, not a source repo.

It contains:
- `Package.swift` with a `.binaryTarget` pointing to a release-hosted XCFramework zip
- `Sources/NanoGraph/NanoGraph.swift`
- `Sources/CNanoGraph/include/nanograph_ffi.h` copied from the monorepo canonical header

Consumers add the package with a standard SPM dependency:
```swift
.package(url: "https://github.com/nanograph/nanograph-swift", from: "0.10.1")
```

No Rust toolchain. No monorepo clone. Just `swift build`.

Initial versioning policy:
- keep `nanograph-swift` tags in lockstep with nanograph release tags
- treat the Swift repo as a packaging layer over a monorepo release, not an independently evolving SDK line
- revisit independent Swift semver only after the C ABI and Swift wrapper remain stable across multiple releases

## Why a separate repo

SPM resolves packages by cloning git repos at a tag. If the Package lives in the nanograph monorepo, SPM clones the entire Rust workspace (~all of nanograph) just to get the Swift wrapper + binary target. A small dedicated repo keeps resolution fast and avoids exposing internal build artifacts.

This is the standard pattern for Rust FFI Swift packages (libsignal-client, nickel, etc.).

The split should be distribution-only:
- the monorepo remains the canonical source of truth for the Rust FFI crate, the public header, and the Swift wrapper
- `nanograph-swift` is generated or synced from monorepo CI
- no hand-edited divergence between the two repos

## Layout

### Monorepo (build source, unchanged)

```
nanograph/
  crates/nanograph-ffi/
    src/lib.rs                    # C ABI exports
    include/nanograph_ffi.h       # canonical public C header
    swift/                        # dev-only Package for local testing
      Package.swift               # keeps workspace-relative paths for dev
      Sources/CNanoGraph/include/ # bridge header that includes ../../../../include/nanograph_ffi.h
      Sources/NanoGraph/          # Swift wrapper
```

### nanograph-swift repo (publishable)

```
nanograph-swift/
  Package.swift                   # binaryTarget URL + checksum
  Sources/
    CNanoGraph/include/nanograph_ffi.h
    NanoGraph/NanoGraph.swift
  Tests/
    NanoGraphTests/
```

`Package.swift`:
```swift
// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "NanoGraph",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "NanoGraph", targets: ["NanoGraph"]),
    ],
    targets: [
        .binaryTarget(
            name: "NanoGraphFFI",
            url: "https://github.com/nanograph/nanograph/releases/download/v0.10.1/NanoGraphFFI.xcframework.zip",
            checksum: "<sha256>"
        ),
        .target(
            name: "CNanoGraph",
            path: "Sources/CNanoGraph",
            publicHeadersPath: "include"
        ),
        .target(
            name: "NanoGraph",
            dependencies: ["CNanoGraph", "NanoGraphFFI"]
        ),
        .testTarget(
            name: "NanoGraphTests",
            dependencies: ["NanoGraph"]
        ),
    ]
)
```

Rationale:
- `NanoGraphFFI` provides the prebuilt library for linking
- `CNanoGraph` keeps the wrapper import surface stable (`import CNanoGraph`) and carries the copied public header
- `NanoGraph.swift` can stay almost entirely unchanged

## XCFramework build

CI currently produces a macOS arm64 XCFramework by default and keeps Intel macOS as an explicit opt-in until the upstream `lance` x86_64 build path is stable again:

```bash
# 1. Build for macOS arm64
cargo build -p nanograph-ffi --release --target aarch64-apple-darwin

# 2. Package as XCFramework
xcodebuild -create-xcframework \
  -library target/aarch64-apple-darwin/release/libnanograph_ffi.a \
  -headers crates/nanograph-ffi/include \
  -output NanoGraphFFI.xcframework

# 3. Zip and upload to GitHub release
zip -r NanoGraphFFI.xcframework.zip NanoGraphFFI.xcframework
# Upload as release asset, compute sha256 for Package.swift checksum
```

If/when Intel macOS support is restored, the build script can include a second `-library ... -headers ...` pair for `x86_64-apple-darwin`.

Also include:
- the canonical header from `crates/nanograph-ffi/include/nanograph_ffi.h`
- any module metadata required by the XCFramework packaging step
- a stable asset naming convention that the Swift repo update job can reference mechanically

## CI workflow

Triggered by tag push (`v*`) in the nanograph monorepo:

1. **Build job** (macOS runner):
   - Build `nanograph-ffi` for `aarch64-apple-darwin`
   - Package XCFramework, zip, compute sha256
   - Upload zip as GitHub release asset

2. **Publish job** (after build):
   - Clone `nanograph-swift` repo
   - Copy the canonical header and Swift wrapper from the monorepo
   - Update `Package.swift` with new release URL and checksum
   - Run a clean consumer smoke test:
     - resolve `nanograph-swift` as an external dependency in a temp package
     - `swift build`
     - `swift test`
   - Commit and tag with the matching version
   - Push

This can be added to the existing `release.yml` workflow.

## What stays in the monorepo

- `crates/nanograph-ffi/` (Rust C ABI crate) — build source
- `crates/nanograph-ffi/swift/` — dev-only Swift Package for local testing with workspace-relative paths
- canonical public header in `crates/nanograph-ffi/include/nanograph_ffi.h`
- Swift wrapper in `crates/nanograph-ffi/swift/Sources/NanoGraph/`

## What does NOT change

- The Rust FFI crate stays as a Cargo workspace member
- The C ABI surface (`nanograph_db_*` functions) is unchanged
- `NanoGraph.swift` remains authored in the monorepo and copied to the distribution repo
- The dev-only `Package.swift` in the monorepo keeps working for local development

## iOS support (future)

The XCFramework format supports multiple platforms. To add iOS:
- Add `aarch64-apple-ios` target to the Cargo build
- Add an iOS library slice to the `xcodebuild -create-xcframework` command
- Update `Package.swift` platforms to include `.iOS(.v16)`

This is additive and does not affect the macOS path.

## Precedent

- **libsignal**: `libsignal` (Rust monorepo) builds C library, CI produces XCFrameworks, `libsignal-client` (separate repo) references them as binary targets
- **libsql**: Rust monorepo + separate Swift Package repo with prebuilt XCFramework
- **prism**: Rust FFI + Swift Package split across repos with CI-driven binary publishing
