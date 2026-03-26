# Architecture Fixes: Lance-Native Execution

Status: proposal
Date: 2026-03-20

## Problem Summary

nanograph treats Lance as a dumb persistence layer. `Database::open()` loads every dataset into memory, mutations rebuild the entire in-memory graph, and CDC diffs the full graph state as JSON. The fix is to keep data in Lance and only materialize what each query actually needs.

Eight issues, ordered by severity:

| ID | Issue | Cost |
|----|-------|------|
| N1 | Full in-memory loading at open | O(total graph) memory, ~305 MB CSR overhead at 1M nodes × 20 edge types |
| N2 | O(N) mutation cost | Every mutation copies all types + rebuilds all CSR/CSC |
| N3 | O(N) snapshot-diff CDC | Serializes entire graph to JSON twice, diffs in memory |
| N5 | Custom transaction coordinator | ~3,200 lines reimplementing Lance's built-in versioning |
| N8 | Brute-force search predicates | `search()`, `fuzzy()`, `match_text()`, `bm25()` scan every row |
| N6 | CDC is a post-hoc diff, not capture | Mutation code knows what changed but discards that knowledge |
| N7 | Lance features blocked | Branching, tagging, stable row IDs, conflict resolution all unavailable |
| N4 | No branching | Single linear version history, no staging/preview |

## Fix 1: Lance TableProvider for Node Scans

**Solves**: N1 (node loading), N8 (search predicates)

### Current

`Database::open()` iterates every dataset in the manifest, calls `read_lance_batches()` — a full `dataset.scan()` with no filters, no projection — loading every `RecordBatch` into `GraphStorage`. `NodeScanExec` scans these in-memory batches. A secondary Lance-native path exists in `NodeScanExec::execute()` but is an optimization within the in-memory architecture.

### Proposed

Register each Lance dataset as a DataFusion `TableProvider`. Lance's `Dataset` already implements this trait. `NodeScanExec` goes away — DataFusion's optimizer handles filter pushdown, projection pushdown, and limit pushdown into Lance automatically.

```
Before: open() → load all datasets → in-memory scan → filter
After:  open() → register TableProviders → query-time Lance scan with pushdown
```

### Node scan changes

Replace `NodeScanExec` (custom `ExecutionPlan` in `plan/node_scan.rs`) with a `TableProvider::scan()` call on the Lance dataset. DataFusion's physical optimizer pushes filters and projections down automatically via `supports_filters_pushdown()`.

The planner (`plan/planner.rs`) currently constructs `NodeScanExec` directly. Change it to reference a registered table in the DataFusion `SessionContext` catalog.

### Search predicate changes (N8)

Create Lance FTS indexes on String properties:

```rust
// At index creation time (during load or explicit index command)
table.create_index(&[prop_name], Index::FTS(
    FtsIndexBuilder::default()
        .with_position(true)  // enables phrase queries for match_text()
)).execute().await?;
```

Map query predicates to Lance's native FTS:

| nanograph predicate | Lance FTS query type | Notes |
|---------------------|---------------------|-------|
| `search(prop, query)` | `MatchQuery` | Token set intersection with BM25 |
| `fuzzy(prop, query, max_edits)` | `MatchQuery` with fuzziness | Levenshtein via FTS index |
| `match_text(prop, query)` | `PhraseQuery` | Requires `with_position: true` |
| `bm25(prop, query)` | `MatchQuery` scored | `_score` column returned by Lance |

Implement as DataFusion scalar UDFs (`ScalarUDFImpl`) so they integrate with the optimizer:

```rust
// search(prop, query) → Bool UDF
// bm25(prop, query) → Float64 UDF (for ORDER BY)
// fuzzy(prop, query, max_edits) → Bool UDF
// match_text(prop, query) → Bool UDF
ctx.register_udf(ScalarUDF::from(SearchUdf::new()));
ctx.register_udf(ScalarUDF::from(Bm25Udf::new()));
```

Additional Lance index types to use:

| Property pattern | Lance index type | Current |
|-----------------|-----------------|---------|
| High-cardinality scalar | `BTreeIndexBuilder` | Already used |
| Enum properties | `BitmapIndexBuilder` | Not used (Lance bitmap index is better for <1000 distinct values) |
| List properties | `LabelListIndexBuilder` | Not used (enables `array_contains_all`/`array_contains_any`) |
| `Vector(dim)` | IVF_PQ / IVF_FLAT | Already used |
| String (full-text) | `FtsIndexBuilder` | Not used |

### What stays in memory

Only CSR/CSC adjacency indices for `ExpandExec`. Node data no longer loaded at open time.

### Key files

- Remove: `plan/node_scan.rs`
- Modify: `plan/planner.rs` (use TableProvider scan instead of NodeScanExec)
- Modify: `store/database.rs` (`open()` registers TableProviders instead of loading batches)
- Add: search UDF implementations (new module in `plan/`)
- Modify: `store/indexing.rs` (add FTS, bitmap, label_list index creation)

---

## Fix 2: Lazy Adjacency Index

**Solves**: N1 (CSR memory overhead)

### Current

`CsrIndex::build(num_nodes, edges)` allocates `vec![0u64; num_nodes + 1]` where `num_nodes` is `self.next_node_id` (the maximum node ID, not the count of nodes with edges). With 20 edge types and 1M nodes: 20 × 2 × 1,000,001 × 8 = ~305 MB of offset arrays, most entries zero. Built at open time for every edge type.

### Tier A: Sparse adjacency (data structure swap)

Replace the dense offset vector with a `HashMap<u64, (u32, u32)>` mapping `node_id → (start_idx, end_idx)` into the neighbors/edge_ids arrays. Only nodes that actually have edges get entries.

```rust
// Before (csr.rs)
pub struct CsrIndex {
    pub offsets: Vec<u64>,      // sized to max_node_id + 1
    pub neighbors: Vec<u64>,
    pub edge_ids: Vec<u64>,
}

// After
pub struct CsrIndex {
    pub ranges: AHashMap<u64, (u32, u32)>,  // node_id → (start, end) in neighbors
    pub neighbors: Vec<u64>,
    pub edge_ids: Vec<u64>,
}
```

Lookup changes from `offsets[node_id]..offsets[node_id + 1]` to `ranges.get(&node_id).map(|(s, e)| &neighbors[*s as usize..*e as usize])`. Same O(1) amortized.

### Tier B: On-demand loading via a session-scoped cache

Only build CSR/CSC for edge types that the current query actually traverses. At `Database::open()`, do not load edge datasets or build indices. Instead, keep edge dataset metadata (path + pinned version) in the immutable snapshot and move adjacency caching out of `GraphStorage`.

The important constraint: query execution holds a shared `Arc<GraphStorage>` snapshot today. That snapshot must stay immutable. Lazy adjacency therefore cannot be stored by mutating `edge_segment.csr` / `edge_segment.csc` inside the read path.

Instead, add a separate `EdgeIndexCache` owned by `DatabaseShared`:

```rust
pub struct EdgeIndexPair {
    pub csr: Arc<CsrIndex>,
    pub csc: Arc<CsrIndex>,
}

pub struct EdgeIndexCache {
    // key: (edge type, pinned dataset version)
    inner: tokio::sync::RwLock<AHashMap<(String, u64), Arc<EdgeIndexPair>>>,
}
```

`ExpandExec` receives a handle to this cache (or a thin `Database` helper around it) and asks for an index pair by edge type + dataset version:

```rust
let pair = edge_index_cache
    .get_or_build(&self.edge_type, dataset_path, dataset_version)
    .await?;
let csr = match self.direction {
    Direction::Out => pair.csr.as_ref(),
    Direction::In => pair.csc.as_ref(),
};
```

`get_or_build()` performs a projected Lance scan of only `("src", "dst", "id")`, builds both CSR and CSC once, stores them in the cache, and returns an `Arc<EdgeIndexPair>`. Concurrent readers either share the cached value or await the same build; they never mutate the `GraphStorage` snapshot.

At mutation time, when an edge dataset version changes, invalidate cache entries for that edge type (or let the versioned cache key naturally miss and prune old entries opportunistically). This preserves the current immutable snapshot/read-concurrency model while still avoiding eager adjacency construction at open.

### Key files

- Modify: `store/csr.rs` (sparse data structure)
- Modify: `store/graph.rs` (store edge dataset metadata only; no eager CSR/CSC)
- Modify: `plan/planner.rs` / `plan/physical.rs` (`ExpandExec` gets cache access instead of mutating snapshot state)
- Modify: `store/database.rs` (`open()` skips edge loading; `DatabaseShared` owns edge index cache + invalidation)

---

## Fix 3: Direct Lance Mutations

**Solves**: N2

### Current

Every mutation path (`delete_nodes_locked`, `delete_edges_locked`, load operations) creates a new `GraphStorage`, copies all node types, copies/filters all edge types, rebuilds all CSR/CSC, then persists to Lance. Cost: O(total graph) for any mutation regardless of size.

### Proposed

Mutate Lance datasets directly. Invalidate only affected adjacency indices.

```
Before: snapshot → new GraphStorage → copy all types → filter changed → build_indices() → persist → replace_storage()
After:  lance_dataset.delete(predicate) → invalidate affected CSR/CSC cache → update manifest
```

### Delete nodes

```rust
// Current: cdc.rs:32-105 — copies everything
pub async fn delete_nodes_locked(&self, type_name: &str, predicate: &DeletePredicate, ...) {
    // ... 70 lines: new GraphStorage, copy all node types, filter all edge types, build_indices

// Proposed:
pub async fn delete_nodes_locked(&self, type_name: &str, predicate: &DeletePredicate, ...) {
    let dataset_path = self.dataset_path_for_node(type_name);
    let filter_expr = predicate_to_lance_sql(predicate);

    // 1. Collect deleted IDs (for edge cascade and CDC)
    let deleted_ids = lance_scan_ids_matching(&dataset_path, &filter_expr).await?;
    if deleted_ids.is_empty() { return Ok(DeleteResult::default()); }

    // 2. Delete from node dataset
    let node_ds = Dataset::open(&dataset_path).await?;
    node_ds.delete(&filter_expr).await?;

    // 3. Cascade: delete edges referencing deleted nodes
    let mut deleted_edges = 0;
    for edge_def in self.schema_ir.edge_types() {
        let edge_path = self.dataset_path_for_edge(&edge_def.name);
        let cascade_filter = format!(
            "src IN ({ids}) OR dst IN ({ids})",
            ids = deleted_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",")
        );
        let edge_ds = Dataset::open(&edge_path).await?;
        let before_count = edge_ds.count_rows(None).await?;
        edge_ds.delete(&cascade_filter).await?;
        let after_count = edge_ds.count_rows(None).await?;
        deleted_edges += before_count - after_count;
        // Invalidate CSR/CSC for this edge type
        self.invalidate_edge_index(&edge_def.name);
    }

    // 4. Update manifest with new Lance dataset versions
    self.commit_manifest().await?;
    Ok(DeleteResult { deleted_nodes: deleted_ids.len(), deleted_edges })
}
```

### Insert (append)

```rust
// Lance append is already used in persist.rs for the "can_append" path.
// Make it the primary path instead of going through GraphStorage.
Dataset::open(&path).await?.append(batch).await?;
```

### Update (merge)

```rust
// MergeInsertBuilder is already used. Remove the GraphStorage intermediary.
let mut builder = MergeInsertBuilder::try_new(dataset, vec![key_prop])?;
builder.when_matched(WhenMatched::UpdateAll)
       .when_not_matched(WhenNotMatched::InsertAll);
builder.execute(source_batch).await?;
```

### Adjacency cache invalidation

After a mutation on edge type `E`, set `edge_segments[E].csr = None` and `edge_segments[E].csc = None`. The next query that traverses `E` will rebuild lazily (Fix 2 Tier B).

### Key files

- Rewrite: `store/database/cdc.rs` (delete_nodes_locked, delete_edges_locked)
- Rewrite: `store/database/persist.rs` (apply_mutation_plan_locked — no longer needs GraphStorage intermediary)
- Modify: `store/database.rs` (add invalidate_edge_index, remove replace_storage for mutations)
- Remove: `store/loader/merge.rs` (merge_storage_with_node_keys, append_storage — Lance handles these natively)

---

## Fix 4: Mutation-Point CDC

**Solves**: N3, N6

### Current

`build_cdc_events_for_storage_transition()` (cdc.rs:402) iterates every node and edge type. For each, `collect_rows_by_id()` serializes every row of both the previous and next `GraphStorage` to `serde_json::Value`, building two complete JSON representations of the entire graph. These are compared field-by-field to produce insert/update/delete events. Cost: O(total graph) for every mutation.

The irony: the mutation code already knows what changed. `delete_nodes_locked` computes `deleted_node_set`. Load operations know which rows are new. But this information is discarded — `apply_mutation_plan_locked` calls the generic O(N) diff.

### Proposed (Option A): Emit CDC events at mutation point

The plumbing already exists. `apply_mutation_plan_locked` (persist.rs:355) checks `if cdc_events.is_empty()` and only falls back to the O(N) diff when no events are provided. Mutations just need to provide them.

```rust
// In delete_nodes_locked, after computing deleted_node_set:
let cdc_events: Vec<CdcLogEntry> = deleted_node_set.iter().map(|&id| {
    CdcLogEntry::pending("delete", "node", type_name, id)
}).chain(
    // Edge cascade events
    cascaded_edge_deletes.iter().map(|(edge_type, id)| {
        CdcLogEntry::pending("delete", "edge", edge_type, *id)
    })
).collect();

self.apply_mutation_plan_locked(
    MutationPlan::prepared_storage(new_storage, "mutation:delete_nodes")
        .with_cdc_events(cdc_events),
    writer,
).await?;
```

Same pattern for inserts (emit from loader) and updates (emit from merge).

### Proposed (Option B, end state): Lance delta() API

Once Fix 3 (direct Lance mutations) lands, CDC can be derived from Lance's version diffing:

```rust
// After mutating Lance datasets directly:
let delta = dataset.delta()
    .compared_against_version(previous_version)
    .build()?;
let inserted_rows = delta.get_inserted_rows().await?;
let deleted_rows = delta.get_deleted_rows().await?;
// Updated rows identified via _row_last_updated_at_version metadata
```

This uses Lance's row lineage metadata (`_row_created_at_version`, `_row_last_updated_at_version`) to identify changes at the fragment level — O(changed fragments), not O(total rows).

### Migration path

1. Option A first: each mutation path emits its own CDC events. No new dependencies.
2. After Fix 3 lands, switch to Option B: derive CDC from Lance version diffs. Remove per-mutation event emission code.

### Key files

- Modify: `store/database/cdc.rs` (delete paths emit events; `build_cdc_events_for_storage_transition` becomes fallback only)
- Modify: `store/database/persist.rs` (load paths emit events)
- Modify: `store/loader.rs` (emit insert events during load)
- Eventually remove: `build_cdc_events_for_storage_transition`, `collect_rows_by_id`, `record_batch_row_to_json_map`

---

## Fix 5: Thin the Manifest Layer

**Solves**: N5, N7

### Current

nanograph builds a custom transaction coordinator across ~3,200 lines:
- `graph.manifest.json` — atomic commit point, records per-dataset Lance versions
- `_tx_catalog.jsonl` — transaction history, links db_version → per-dataset Lance versions
- `_cdc_log.jsonl` — entity-level change log with byte-offset pointers from tx catalog
- `reconcile_logs_to_manifest()` — crash recovery: truncates partial writes, ensures log consistency
- `commit_manifest_and_logs()` — ordered write protocol: CDC log → TX catalog → manifest

This reimplements Lance's built-in MVCC and blocks Lance-native features (branching, tagging, stable row IDs, conflict resolution).

### Proposed

Reduce the manifest to a cross-dataset version pointer. Let Lance own per-dataset versioning.

```json
{
  "db_version": 42,
  "schema_ir_hash": "abc123...",
  "next_node_id": 100000,
  "next_edge_id": 200000,
  "datasets": {
    "node:Person": { "path": "nodes/a1b2c3d4", "lance_version": 17 },
    "node:Company": { "path": "nodes/b2c3d4e5", "lance_version": 8 },
    "edge:Knows": { "path": "edges/e5f6a7b8", "lance_version": 12 }
  }
}
```

### What gets removed

| Component | Current LOC | Replacement |
|-----------|------------|-------------|
| `_tx_catalog.jsonl` + parser/writer | ~400 | Lance's per-dataset version history (`dataset.versions()`) |
| `_cdc_log.jsonl` + byte-offset pointers | ~300 | Fix 4 (mutation-point events or Lance delta) |
| `reconcile_logs_to_manifest()` | ~150 | Atomic manifest write + Lance `checkout_version()` for rollback |
| `commit_manifest_and_logs()` ordering protocol | ~100 | Single atomic manifest write |
| CDC byte-offset tracking in tx catalog | ~100 | Not needed — CDC events are self-contained |

### Crash recovery

Simplified: the manifest is the sole commit gate. Write it atomically (already done via temp + rename). If crash before manifest write, Lance datasets may have advanced versions, but the manifest still points to the previous consistent versions. `checkout_version()` pins each dataset to the manifest-recorded version. No log truncation or reconciliation needed.

### Lance features unblocked (N7)

With the thin manifest, Lance-native features become accessible:

- **Branching**: `dataset.create_branch("staging", current_version)` for speculative writes or preview environments.
- **Tagging**: `dataset.tags().create("release-v1", version)` to protect known-good versions from cleanup.
- **Stable row IDs**: Enable via `WriteParams { enable_move_stable_row_ids: true }` for persistent row identity across compaction.
- **Conflict resolution**: Remove `conflict_retries(0)` from `run_lance_merge_insert_with_key()`. Lance auto-resolves concurrent appends and concurrent deletes.

### Single-writer stays

Keep the `tokio::sync::Mutex<()>` writer lock. Single-writer is a valid design for an embedded database. Concurrent readers are already supported via the `RwLock<Arc<GraphStorage>>` (and will continue to work via Lance's MVCC snapshots).

### Key files

- Simplify: `store/txlog.rs` (remove tx catalog and CDC log management, keep manifest read/write)
- Simplify: `store/manifest.rs` (dataset entries only, no log pointers)
- Remove: `reconcile_logs_to_manifest()`, `commit_manifest_and_logs()` ordering protocol
- Modify: `store/database.rs` (`open()` uses manifest versions to pin Lance datasets)
- Modify: `store/lance_io.rs` (remove `conflict_retries(0)`)

---

## Fix 6: DataFusion Standard Operators

**Solves**: maintenance surface, optimizer integration

### Replace custom CrossJoinExec

Current `CrossJoinExec` in `plan/physical.rs` is a minimal Cartesian product. Replace with `datafusion_physical_plan::joins::CrossJoinExec::new(left, right)`. DataFusion's version handles partitioning, statistics estimation, and integrates with the physical optimizer.

### Replace custom AntiJoinExec

Current `AntiJoinExec` joins on node ID (`u64`), which is an equijoin. Replace with:

```rust
use datafusion_physical_plan::joins::HashJoinExec;
use datafusion_common::JoinType;

// The inner pipeline for `not {}` blocks must be materialized as a
// standalone plan (not seeded from outer input).
let anti_join = HashJoinExec::try_new(
    outer_plan,
    inner_plan,
    vec![(col_outer_id, col_inner_id)],  // equijoin on node id
    None,  // no additional filter
    &JoinType::LeftAnti,
    None,  // projection
    PartitionMode::CollectLeft,
)?;
```

This requires changing how `not {}` blocks are lowered in `ir/lower.rs`. Currently the inner pipeline is "seeded" with the outer plan's bound variables. With a standard anti-join, the inner side must independently produce the set of IDs to exclude, then the join excludes matching rows from the outer side.

### Key files

- Modify: `plan/planner.rs` (use DataFusion join operators)
- Modify: `ir/lower.rs` (change `not {}` lowering to produce independent inner plan)
- Remove: `CrossJoinExec`, `AntiJoinExec` from `plan/physical.rs`
