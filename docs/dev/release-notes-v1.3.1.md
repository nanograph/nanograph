v1.3.1

- Patch release that unblocks the npm distribution channel. `v1.3.0` shipped to crates.io and GitHub Releases successfully, but the npm publish failed: the existing strategy bundled all five platform `.node` binaries into a single `nanograph-db` package, and Lance 6's larger native code pushed the combined tarball to 332 MB packed / 931 MB unpacked, above npm's payload cap (HTTP 413). `v1.3.1` switches to the standard napi-rs per-platform optional-dependencies layout so npm install picks up only the binary for the host architecture.

- npm distribution refactor:
  - `nanograph-db` is now a tiny meta package (~10 KB) — JS dispatcher, TypeScript types, source for rebuild — declaring all five per-platform binaries as `optionalDependencies`
  - five new published packages, one per target: `nanograph-db-darwin-arm64`, `nanograph-db-darwin-x64`, `nanograph-db-linux-x64-gnu`, `nanograph-db-linux-arm64-gnu`, `nanograph-db-win32-x64-msvc` — each ~60 MB packed / ~165–217 MB unpacked, well under npm's cap
  - `npm install nanograph-db` works exactly as before: the existing `index.js` dispatcher already preferred `require('nanograph-db-{platform}')` when a local `.node` was absent, so the API and call sites do not change
  - the custom `scripts/install.cjs` postinstall is removed — `optionalDependencies` + the dispatcher cover both the bundled-binary and per-platform paths

- CI changes:
  - `Publish NPM` workflow now runs `napi artifacts` to fan the built `.node` binaries into `crates/nanograph-ts/npm/{platform}/` directories, then publishes each per-platform package, then publishes the meta package
  - each publish step is idempotent — already-published `(name, version)` pairs are skipped, so a partial-failure re-run only retries what's missing
  - new step verifies all `npm/*/package.json` versions match the tag before any publish

- Local development:
  - `tools/ts-sdk/smoke_test_consumer.sh` now packs both the meta package and the host-platform package, then installs both into the temp consumer — mirrors how `npm install nanograph-db` resolves at runtime
  - host-platform detection added (darwin/linux/windows × x64/arm64)

- Rust crates are unchanged at the API or behavior level; `nanograph`, `nanograph-cli`, `nanograph-ffi`, `nanograph-ts` are republished at `1.3.1` only to keep the published version set aligned across all four crates and the npm package

- Release metadata updated:
  - version bumped to `1.3.1` across Rust crates, npm package metadata (main + 5 per-platform), and Swift packaging examples
