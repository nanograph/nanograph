v1.3.0

- Minor release on the `NamespaceLineage` line. `v1.3.0` reshapes nanograph around the agent-toolbelt model: atomic mutations safe to chain under concurrency, a third local-embedding option, a coordinated Lance 6 / Arrow 58 / DataFusion 53 bump, and several perf wins on the agent hot path.

- Agent-toolbelt mutations (#18):
  - `put Type { ... }` upserts when the type has `@key` (insert when absent, update when present in a single statement) — replaces the awkward "try insert, fall back to update" pattern
  - `update ... where { p1; p2; p3 }` (newline-separated atoms) provides conjunctive CAS predicates, so an agent can claim a row only if every guard still holds — Lance OCC plus the row gate keeps "exactly one winner" under concurrent claims
  - `IS NULL` / `IS NOT NULL` are now first-class atoms inside `where { }` blocks
  - mutation responses now wrap the affected rows in a `matched_nodes` envelope so agents can read back what they touched without a second query
  - new `ql-canon.md` documents the agent-toolbelt design principle (P8): one query is one atomic tool call; agents orchestrate, QL stays simple

- LM Studio as a local embedding provider (#17):
  - third provider option alongside OpenAI and Gemini for `@embed(source_prop)` and `nearest($vec, $q)` query embedding
  - new `NANOGRAPH_EMBED_PROVIDER=lmstudio` selector; `LMSTUDIO_BASE_URL` defaults to `http://localhost:1234/v1`
  - LM Studio is never auto-detected — provider must be set explicitly, model has no default and must be configured
  - lets agents run fully local with Qwen3 / nomic-embed / similar models served from LM Studio

- Coordinated storage + columnar bumps (#19):
  - Lance 4.0 → 6.0, Arrow 57 → 58, DataFusion 52 → 53 (hard-pinned together by Lance v6)
  - pulls in SIMD-accelerated u8/bf16/f16/f64 distance kernels for vector search, RaBitQ 4-bit LUT (16× speedup on ARM), segmented inverted index for FTS, eager I/O scheduling, range-query 500× speedup
  - on-disk format compatibility preserved — graphs created on the v4-pinned binary open cleanly on the v6 binary
  - `MergeInsertBuilder`, namespace API, FTS query construction, and 5 custom `ExecutionPlan` impls migrated to the new trait shapes

- Perf on the agent hot path (#20):
  - `SourceDedupeBehavior::FirstSeen` on `put` upserts (CL-505) — Lance no longer rescans the dataset to deduplicate when we already know the source side is unique
  - `fast_search()` gate on Lance scanners for read-only paths (CL-512) — skips the inverted-index materialization step when we only need IDs
  - compiled-query cache (CL-508) — re-running the same `.gq` text against the same schema reuses the typechecked AST + lowered IR instead of re-parsing per call

- Per-execution embed cache + dedup (#21):
  - per-`DatabaseRuntime` cache of query-text embeddings keyed by `(model, text, dim)` (CL-510) — agents that repeat `similar_issues("memory leak")` 5× during a triage loop skip the network round-trip after the first call; provider switch or `Vector(N)` change invalidates naturally
  - custom `CrossJoinExec` replaced with `datafusion_physical_plan::joins::CrossJoinExec` (CL-511) — ~220 LOC of state-machine cross-join code removed, replaced by the DataFusion built-in that already does the same thing and ships with optimizer support

- CI and release infrastructure:
  - `Publish Crates` workflow now publishes `nanograph` → `nanograph-cli` → `nanograph-ffi` → `nanograph-ts` to crates.io on tag push, polling the sparse index between hops so each crate waits for its dependency to surface
  - `Publish NPM` workflow matrix-builds `.node` binaries for all five platform targets (macOS arm64/x64, Linux x64/arm64, Windows x64) and publishes the bundled `nanograph-db` npm package on tag push
  - both publish workflows skip already-published versions and support `workflow_dispatch` dry-run for manual rehearsal

- Release metadata updated:
  - version bumped to `1.3.0` across Rust crates, npm package metadata, and Swift packaging examples
