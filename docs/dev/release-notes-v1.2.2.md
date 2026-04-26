v1.2.2

- Patch release for the `NamespaceLineage` line. `v1.2.2` fixes two query-engine bugs caught in the wild and tightens schema-migration and JSONL-load validation so unsafe changes fail fast with clear errors instead of corrupting state.

- Static rejection of unsupported `nearest()` query shapes (#9):
  - `nearest($vec, $node.prop)` and `nearest($vec, $bound_var)` previously parsed and lint-passed but failed at execution with `nearest() query must be a Vector or String literal/parameter`
  - the typechecker now rejects these shapes with a T15 error explaining why and pointing at the workaround (fetch the value in a prior query and pass it as `$param`)
  - per-row evaluation isn't viable because the query vector must be resolved once before scoring

- Fan-in traversals to a shared destination no longer crash at execution (#8):
  - two edges landing on the same already-bound destination (e.g. `$a aToB $b` + `$c cToB $b`) used to fail with `Arrow error: Invalid argument error: number of columns(N) must match number of fields(M) in schema`
  - same bug fired when a `not { }` clause traversed into an outer-bound variable, since AntiJoin merges schemas
  - the cycle-closing lowerer now threads a counter through pipeline construction and emits a unique `__temp_{dst}_{n}` per traversal, eliminating the schema collision

- Schema migration now validates against stored data before applying:
  - migrations that would introduce constraint violations are `Blocked` with a detailed error instead of silently proceeding — this covers adding `@key` on a column with nulls or duplicates, adding `@unique` on a column with duplicates, narrowing an enum to exclude existing values, and most type casts
  - metadata-only changes (description, instruction) auto-apply as `Safe`
  - additive vs breaking vs blocked classification is surfaced in dry-run output

- JSONL loader rejects unknown property names with row numbers:
  - typos like `"naem"` no longer silently drop the field — the loader returns a row-numbered error pointing at the offending property
  - list-typed enum properties also have their values validated per element

- Test coverage:
  - new typecheck tests for both rejected `nearest()` shapes
  - new engine integration tests for fan-in (two edges → same destination) and the `not { }` variant, including an orphan-detection case after a follow-up load
  - new schema-migration tests for each blocking scenario (key-with-nulls, unique-with-duplicates, enum-narrowing-with-invalid-values, blocked type cast)

- Release metadata updated:
  - version bumped to `1.2.2` across Rust crates, npm package metadata, and Swift packaging examples
