# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is nanograph?

Embedded local-first typed property graph DB in Rust. Arrow-native columnar execution, Lance storage, DataFusion query engine. Think SQLite for graphs. Two custom DSLs: schema (.pg) and query (.gq), both parsed with Pest grammars.

## Build & Test Commands

```bash
cargo build                              # full workspace build
cargo build -p nanograph                 # library only
cargo build -p nanograph-cli             # CLI only
cargo test                               # all tests (unit + e2e + migration)
cargo test -p nanograph                  # library tests only
cargo test -p nanograph --test engine_integration  # engine integration tests
cargo test -p nanograph --test schema_migration    # migration tests only
cargo test test_bind_by_property         # single test by name
cargo test -- --nocapture                # show stdout
cargo test -p nanograph-cli               # CLI integration tests
cargo test -p nanograph-cli --test starwars_workflows  # single CLI test file
cargo clippy                             # lint
cargo fmt                                # format
RUST_LOG=debug cargo run -p nanograph-cli -- run ...  # enable tracing
cargo bench -p nanograph                 # all criterion benchmarks
cargo bench -p nanograph --bench traversal  # single benchmark domain
NANOGRAPH_BENCH_SMOKE=1 cargo bench -p nanograph  # quick smoke mode
cargo build -p nanograph-ffi             # FFI crate (not in default-members)
cargo build -p nanograph-ts              # TS SDK crate (not in default-members)
```

Workspace `default-members` = `nanograph` + `nanograph-cli`. Plain `cargo build`/`cargo test` skip the FFI and TS SDK crates — target them explicitly with `-p`.

**Requires `protoc`** (Protocol Buffers compiler) at build time for the Lance dependency. MSRV 1.91, Rust edition 2024. `debug = 0` in dev profile (no debuginfo — builds are faster but backtraces are address-only). Dependencies are compiled with `opt-level = 2` even in dev profile so tests run at reasonable speed while the nanograph crate itself stays unoptimized for fast rebuilds. Release profile uses `lto = "thin"` and `codegen-units = 16`.

## Architecture

### Workspace

Four crates:
- `nanograph` — core library. All domain logic lives here.
- `nanograph-cli` — binary named `nanograph`. Thin clap wrapper that calls library functions.
- `nanograph-ffi` — C ABI (`cdylib`/`staticlib`) for Swift and native clients. Exports `nanograph_db_*` C functions; results are heap-allocated JSON strings freed with `nanograph_string_free`. Includes Swift Package wrapper in `swift/`.
- `nanograph-ts` — TypeScript/Node.js SDK via napi-rs. npm package `nanograph-db`. `JsDatabase` wraps core `Database` behind `Arc<tokio::sync::Mutex>`. Platform-aware `.node` loader for macOS/Linux/Windows.

All SDK crates call the same core pipeline (parse → typecheck → lower → execute → serialize via `json_output`). No logic duplication.

### Dual-Mode Execution

The system supports two execution modes that affect many code paths:
- **DB mode** (`--db path.nano`): Lance-backed persistence, supports mutations, CDC, migration, maintenance commands.
- **Legacy mode** (`--schema`/`--data` flags): In-memory GraphStorage, read-only queries. Useful for quick checks without a DB.

### Query Execution Pipeline

```
.gq text → parse_query() → QueryAST
         → typecheck_query() → TypeContext (validates against catalog)
         → lower_query() → QueryIR (pipeline of operators)
         → build_physical_plan() → DataFusion ExecutionPlan
         → execute_query() → Vec<RecordBatch>
```

### Search & Embeddings

Three search subsystems:

**Full-text search** — `@index` on String properties creates Lance inverted indexes (built automatically during load/mutation via `rebuild_node_text_indexes`). Text predicates: `search(string_prop, query)` for token-based keyword match, `fuzzy(string_prop, query[, max_edits])` for approximate match, `match_text(string_prop, query)` for contiguous phrase match. Ordering/ranking: `bm25(string_prop, query)` for lexical relevance. Results are cached per session via `TextSearchCache` in `store/runtime.rs` (keyed by type, property, query, kind, dataset version).

**Vector search** — `Vector(dim)` properties with Lance exact KNN. Two workflows:
- **Manual vectors**: Put vectors directly in JSONL data, query with `nearest(prop, $param)` ordering.
- **Auto-embedding**: Annotate a `Vector(dim)` property with `@embed(source_prop)` — embeddings are generated from the source String property at load time via OpenAI API.

**Hybrid** — `rrf(nearest(...), bm25(...))` for reciprocal rank fusion. `nearest` and `rrf` require a `limit` clause.

Embedding cache: `_embedding_cache.jsonl` in the DB directory caches content-hashed embeddings to avoid re-embedding unchanged data. Large text (>1500 chars by default) is chunked with overlap and averaged.

Key modules: `embedding.rs` (OpenAI client, retry, mock mode), `store/loader/embeddings.rs` (load-time materialization, caching, chunking), `store/runtime.rs` (TextSearchKind, TextSearchCache, native Lance FTS execution), `store/indexing.rs` (scalar, vector, and FTS index lifecycle).

### Module Map (`crates/nanograph/src/`)

| Module | Role |
|--------|------|
| `schema/` | `schema.pest` grammar + parser → schema AST |
| `query/` | `query.pest` grammar + parser → query AST; `typecheck.rs` validates against catalog |
| `catalog/` | `schema_ir.rs` — compiled schema representation used at runtime |
| `ir/` | `lower.rs` — lowers typed AST into flat IR operators (NodeScan, Expand, Filter, AntiJoin, mutations) |
| `plan/bindings.rs` | Flat binding column utilities (`variable::property` naming, struct reconstruction) |
| `plan/planner.rs` | Converts IR to DataFusion physical plans; TailStrategy analysis for DataFusion delegation |
| `plan/node_scan.rs` | Custom NodeScanExec with Lance filter pushdown and text search tracking |
| `plan/physical.rs` | Custom ExpandExec, CrossJoinExec, AntiJoinExec, ScoreExec, mutation execution |
| `store/database.rs` | Lance-backed persistence, delete API, load modes, compact/cleanup/doctor |
| `store/graph.rs` | In-memory GraphStorage with CSR/CSC indices |
| `store/csr.rs` | CSR/CSC adjacency structure — core graph index for traversal |
| `store/loader/` | Load orchestration: `jsonl.rs` (parsing + Arrow builders), `constraints.rs`, `merge.rs`, `embeddings.rs` (load-time embedding materialization) |
| `store/indexing.rs` | Lance scalar, vector, and FTS index lifecycle |
| `store/migration.rs` | Schema evolution engine |
| `store/manifest.rs` | Committed graph snapshot payload (`GraphManifest`) used inside storage layers |
| `store/txlog.rs` | CDC readers/writers across storage generations; default new graphs use lineage-native CDC |
| `store/namespace_lineage_graph_log.rs` | `NamespaceLineage` internal tables: `__graph_tx` and `__graph_deletes` |
| `store/namespace_lineage_internal.rs` | Ensures and merges `NamespaceLineage` internal datasets into committed snapshots |
| `embedding.rs` | OpenAI embedding client, retry logic, mock mode |
| `json_output.rs` | Shared Arrow→JSON serialization for CLI and SDKs. Handles JS safe integer range (i64/u64 > 2^53 are stringified) |
| `query_input.rs` | Query param parsing, named query lookup from `.gq` files, JSON→ParamMap conversion |
| `result.rs` | `RunResult`, `QueryResult`, `MutationResult` — structured result types for CLI and SDKs |
| `types.rs` | Core type definitions, `PropType`, Arrow type mappings |
| `error.rs` | `NanoError` error type |

### Public API (`lib.rs` re-exports)

```rust
pub use catalog::{build_catalog, schema_ir};   // SchemaIR — compiled schema used at runtime
pub use ir::ParamMap;                           // query parameter map
pub use ir::lower::{lower_query, lower_mutation_query};
pub use plan::planner::execute_query;
pub use plan::physical::MutationExecResult;
pub use query::ast::Literal;
pub use query_input::{JsonParamMode, RunInputError, RunInputResult, ToParam,
                      find_named_query, json_params_to_param_map};
pub use result::{MutationResult, QueryResult, RunResult};
pub use types::{Direction, EdgeId, NodeId, PropType, ScalarType};
```

All SDK crates (FFI, TS) call `build_catalog → lower_query → execute_query → json_output`. This is the stable interface.

### Error Handling

All library errors go through `NanoError` (in `error.rs`). Variants: `Parse`, `Catalog`, `Type`, `Storage`, `UniqueConstraint` (structured fields), `Plan`, `Execution`, `Arrow`, `DataFusion`, `Io`, `Lance`, `Manifest`. Source-span diagnostics use the `ariadne` crate for pretty error rendering with source locations (used in schema/query parse errors and type errors).

### Key Design Details

- **Flat binding columns**: During execution, variables are materialized as flat columns named `variable::property` (e.g. `p::name`, `p::age`). The `plan/bindings.rs` module provides utilities to create, split, and access these columns. Struct columns are reconstructed on demand for the return path. This aligns with DataFusion's columnar model.
- **Edge traversal is a Datalog predicate**: `$p knows $f` — no arrows, no Cypher syntax. Direction inferred from schema endpoint types.
- **Custom ExecutionPlans**: NodeScanExec, ExpandExec (CSR/CSC traversal), CrossJoinExec, AntiJoinExec, ScoreExec (text search scoring). The planner analyzes tail operations via `TailStrategy` — sorting, filtering, and aggregation are delegated to DataFusion when possible, falling back to legacy post-processing for search-dependent tails.
- **Reverse traversal**: When source is unbound and destination is bound, the planner swaps direction and uses CSC instead of CSR.
- **Negation**: `not {}` compiles to AntiJoinExec. The inner pipeline must be seeded with the outer plan's input.
- **Bounded expansion**: `knows{1,3}` compiles to a finite union of 1-hop, 2-hop, 3-hop — no recursion.

### Persistence Layout

```
<name>.nano/
├── schema.pg              # source schema
├── schema.ir.json         # compiled schema IR
├── __graph_snapshot/      # committed graph snapshot table
├── __graph_tx/            # committed transaction windows
├── __graph_deletes/       # delete tombstones for lineage-native CDC
├── __blob_store/          # managed imported media blobs
├── nodes/<type_id_hex>/   # Lance dataset per node type
└── edges/<type_id_hex>/   # Lance dataset per edge type
```

New graphs default to the `NamespaceLineage` storage generation. CDC is reconstructed from Lance lineage plus `__graph_deletes`; `__graph_changes` is legacy-only.

Type IDs are FNV-1a hashes of `"node:TypeName"` / `"edge:TypeName"` → u32 hex.

### CLI Commands

All commands support `--json` global flag for structured output. Core: `init`, `load` (requires `--mode overwrite|append|merge`), `check`, `run` (`--format table|csv|jsonl|json`, `--param key=value`), `delete`, `migrate`. Inspection: `version`, `describe`, `export`. Maintenance: `compact`, `cleanup`, `doctor`, `cdc-materialize`, `changes`. Run `nanograph <command> --help` for full flag details.

### Type System

Scalar types: `String`, `I32`, `I64`, `U32`, `U64`, `F32`, `F64`, `Bool`, `Date`, `DateTime`. Vector type: `Vector(dim)` for fixed-size float vectors (semantic search). Enum types: `enum(val1, val2, ...)`. List types: `[String]`, `[I32]`, etc. All property types are nullable by appending `?`. Query literals include `date("2026-01-15")`, `datetime("2026-01-15T10:00:00Z")`, and list literals `[1, 2, 3]`.

### Schema Annotations

- `@key` — single property per node type, used for keyed merge. Auto-indexed.
- `@unique` — enforced on load/upsert. Nullable unique allows multiple nulls.
- `@index` — creates a scalar index for scalar fields or a vector index for `Vector(dim)` fields.
- `@embed(source_prop)` — auto-generates embeddings from a String property at load time. Target must be `Vector(dim)`.
- `@rename_from("old")` — tracks type/property renames for migration.

List properties cannot have `@key`, `@unique`, `@index`, or `@embed`.

### Schema Migration

Edit `<db>/schema.pg` then `nanograph migrate`. Uses `@rename_from("old_name")` for renames. Safety levels: `safe` (auto-apply), `confirm` (needs `--auto-approve`), `blocked` (e.g. adding non-nullable property to populated type).

### Query Mutations

```
insert Person { name: $name, age: $age }
update Person set { age: $age } where name = $name
delete Person where name = $name
```

`insert` = append. `update` requires `@key`, uses merge. `delete` cascades edges. Typechecked at compile time (T10-T14).

## Project Config

`nanograph init` scaffolds `nanograph.toml` (shared defaults, safe to commit) and `.env.nano` (local secrets, gitignored). Config discovery: `--config <path>` or `./nanograph.toml` — does not walk parent dirs. Key sections: `[db]` (default_path), `[schema]` (default_path), `[query]` (roots), `[embedding]` (provider, model, chunk settings), `[cli]` (output_format, json), `[query_aliases.<name>]` (shortcuts for `nanograph run`). Query aliases map positional args to named params — e.g. `nanograph run search "query text"`. See `docs/user/config.md` for full reference.

## Environment Variables

The CLI loads `.env.nano` then `.env` from the config base directory at startup (custom parser, no external dependency). Variables are only set if not already present in the environment. Precedence: process env → `.env.nano` → `.env` → `[embedding]` in `nanograph.toml` → engine defaults.

- `OPENAI_API_KEY` — required only for real embedding API calls.
- `OPENAI_BASE_URL` — custom OpenAI-compatible endpoint (default: OpenAI API).
- `NANOGRAPH_EMBED_MODEL` — OpenAI model name (default: `text-embedding-3-small`).
- `NANOGRAPH_EMBED_BATCH_SIZE` — batch size for API calls (default: 64).
- `NANOGRAPH_EMBED_CHUNK_CHARS` — chunk size for large text; 0 disables (default: 1500).
- `NANOGRAPH_EMBED_CHUNK_OVERLAP_CHARS` — overlap between chunks (default: 200).
- `NANOGRAPH_EMBED_CACHE_MAX_ENTRIES` — max cache entries (default: 50000).
- `NANOGRAPH_EMBED_CACHE_LOCK_STALE_SECS` — stale lock timeout (default: 60).
- `NANOGRAPH_EMBEDDINGS_MOCK=1` — use deterministic mock embeddings (no API key needed). Used in tests/CI.

See `.env.example` for reference.

## Common Change Patterns

**Adding a new scalar type**: `types.rs` (PropType + Arrow mapping) → `schema.pest` + `schema/parser.rs` → `query.pest` + `query/parser.rs` (literal syntax) → `query/typecheck.rs` → `store/loader/jsonl.rs` (Arrow builder) → `store/database.rs` (predicate handling if needed).

**Adding a new IR operator**: `ir/lower.rs` (emit new op) → `plan/planner.rs` (convert to ExecutionPlan) → `plan/physical.rs` (implement ExecutionPlan trait).

**Adding a new CLI command**: `crates/nanograph-cli/src/main.rs` (clap subcommand + handler). Library logic goes in `crates/nanograph/src/`.

**Modifying Pest grammars**: Edit `.pest` file → update corresponding `parser.rs` → update `typecheck.rs` if the change affects type rules → update `grammar.ebnf` to keep it in sync.

**Changing query result shape or adding types**: Update `json_output.rs` (Arrow→JSON conversion shared by CLI + SDKs) → update TS SDK `convert.rs` if type mapping changes → update FFI `src/lib.rs` if C API surface changes.

## Version Constraints

Arrow 57, DataFusion 52, Lance 3.0 + lance-index 3.0 — these must stay compatible with each other. Pest 2 for both grammars. napi/napi-derive 2 for TS SDK. Dependencies use sub-crates, not monolithic packages: `arrow-array`, `arrow-schema`, `arrow-select`, `arrow-cast`, `arrow-ord` (not `arrow`); `datafusion-physical-plan`, `datafusion-physical-expr`, `datafusion-execution`, `datafusion-common`, `datafusion-expr`, `datafusion-functions-aggregate` (not `datafusion`). Import accordingly. All dependency versions are centralized in the root `Cargo.toml` under `[workspace.dependencies]` — add or update versions there, then reference with `dep.workspace = true` in crate-level Cargo.toml files.

## Design Documents

- `grammar.ebnf` — formal grammar for both DSLs, includes type rules (T1-T21; T10-T14 cover mutations, T15-T21 cover search/ordering)
- `docs/dev/release-checklist.md` — release process steps
- `docs/dev/fixes.md` — architectural issues and solutions map
- `docs/dev/cdc-wal-proposal.md` — WAL-first CDC architecture proposal
- `docs/dev/storage-modes.md` — full graph in memory vs on-demand analysis
- `docs/dev/dataset-sharding.md` — per-type dataset justification
- `docs/dev/distributed-architecture.md` — scaling from embedded to distributed
- `docs/dev/nano-omni.md` — product roadmap (NanoGraph embedded vs Omnigraph repo-native)

User-facing docs live in `docs/user/` — schema, queries, search, config, CLI reference, worked examples, and `best-practices.md` (agent anti-patterns and operational guidelines).

Source of truth for behavior is code. Update docs in the same PR when behavior changes.

## Test Fixtures

Test schemas, queries, and data live in `crates/nanograph/tests/fixtures/` (test.pg, test.gq, test.jsonl). Runnable examples in `examples/starwars/` and `examples/revops/`. Library integration tests: `engine_integration.rs` (core query engine), `schema_migration.rs` (schema evolution). Performance harnesses (run with `--ignored`): `index_perf.rs`, `write_amp_perf.rs`, `json_output_perf.rs`.

CLI integration tests (Rust) in `crates/nanograph-cli/tests/` with shared helpers in `common/mod.rs`. Test files: `bootstrap_and_env`, `bug_regressions`, `config_and_aliases`, `config_failures`, `display_formats`, `docs_and_output`, `load_modes_and_export`, `revops_admin_and_cdc`, `revops_workflows`, `schema_analysis`, `starwars_export_roundtrip`, `starwars_workflows`.

Criterion benchmarks in `crates/nanograph/benches/`: `query_lookup`, `traversal`, `search`, `result_transport`. Shared setup in `benches/common/mod.rs`. Uses synthetic data and checked-in examples (starwars, revops). Legacy perf tests (`tests/*_perf.rs`, run with `--ignored`) are frozen — new benchmark work goes in `benches/`.

## Not Yet Implemented

These are on the backlog but **do not exist yet** — do not assume they work or generate code that depends on them: `@locked` annotation, encryption at rest, multimedia content storage, Python SDK, Go SDK, graph analytics (community detection, centrality, flow tracing, blast radius), ASCII graph visualization, progress bars on `nanograph load`.

## Known Pitfalls

- `literal_to_array` must cast to match LHS column type, not default to Int64.
- ExpandExec id→row mapping must be rebuilt from the concatenated batch, not reuse segment.id_to_row.
- AntiJoin inner pipeline needs outer input seeded via `build_physical_plan_with_input`.
- When a query variable isn't in the batch schema, skip the filter rather than erroring.
- Enum values are auto-sorted and deduplicated; duplicate values are rejected at parse time.
- List properties cannot have `@key`, `@unique`, or `@index` annotations.
- On Windows, `File::sync_all()` requires write access — open files with `OpenOptions::write(true)` before syncing. The `write_atomic` helper handles this correctly.
