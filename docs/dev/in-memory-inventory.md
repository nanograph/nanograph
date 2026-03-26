# In-Memory Inventory

Status: analysis
Date: 2026-03-20

Complete inventory of what nanograph holds in memory, who uses it, and what actually needs to be there.

## GraphStorage Structure

```rust
pub struct GraphStorage {
    pub catalog: Catalog,
    pub node_segments: HashMap<String, NodeSegment>,
    pub edge_segments: HashMap<String, EdgeSegment>,
    node_dataset_paths: HashMap<String, PathBuf>,
    next_node_id: u64,
    next_edge_id: u64,
}
```

### Per Node Type (NodeSegment)

| Field | What it holds | Size for 100K Person nodes |
|-------|--------------|---------------------------|
| `batches: Vec<RecordBatch>` | All rows, all columns — the actual data | ~all node data (names, ages, vectors, everything) |
| `id_to_row: HashMap<u64, (usize, usize)>` | node_id → (batch_idx, row_idx) | ~100K entries × ~50 bytes = ~5 MB |
| `schema: SchemaRef` | Arrow schema | negligible |

### Per Edge Type (EdgeSegment)

| Field | What it holds | Size for 500K Knows edges, 1M max_node_id |
|-------|--------------|-------------------------------------------|
| `src_ids: Vec<u64>` | Source node IDs | 500K × 8 = 4 MB |
| `dst_ids: Vec<u64>` | Destination node IDs | 500K × 8 = 4 MB |
| `edge_ids: Vec<u64>` | Edge IDs | 500K × 8 = 4 MB |
| `batches: Vec<RecordBatch>` | All edge property columns | all edge property data |
| `csr.offsets: Vec<u64>` | Dense array sized to max_node_id+1 | 1M × 8 = 8 MB |
| `csr.neighbors: Vec<u64>` | Destination IDs sorted by source | 500K × 8 = 4 MB |
| `csr.edge_ids: Vec<u64>` | Edge IDs parallel to neighbors | 500K × 8 = 4 MB |
| `csc` (same as csr) | Reverse direction | same as above |

Multiply by number of types. The entire graph is in memory twice — once as raw batches, once as adjacency structures.

## Who Uses What

| Consumer | Reads from | Actually needs it? |
|----------|-----------|-------------------|
| `NodeScanExec` | `node_segments[type].batches` | **No** — Lance path already exists and is better (index-accelerated, projection pushdown) |
| `ExpandExec` (adjacency) | `edge_segments[type].csr/csc` | **Yes** — O(1) adjacency lookup, no Lance equivalent |
| `ExpandExec` (destination nodes) | `node_segments[dst_type].batches` + builds `id_to_row` | **No** — Lance `WHERE id IN (...)` with BTree index |
| `ExpandExec` (edge properties) | `edge_segments[type].batches` | **No** — not even read by any query operator today |
| CDC diff | `node_segments[*].batches` + `edge_segments[*].batches` | **No** — should be eliminated entirely (mutation-point events) |
| Mutations | Clone entire `GraphStorage` | **No** — Lance has native delete/append/merge |
| `persist_storage_with_cdc` | `node_segments[*].batches` for writing | **No** — Lance already has the data |

**The only thing that earns its memory cost is CSR/CSC.** And it only needs `(src, dst, edge_id)` — 24 bytes per edge.

## CLI vs SDK Lifecycle

The CLI is stateless. Every command opens the database from scratch, runs the operation, and exits:

```bash
nanograph run query1    # open → load all → build CSR → run → exit (all freed)
nanograph run query2    # open → load all → build CSR → run → exit (all freed)
nanograph run query3    # open → load all → build CSR → run → exit (all freed)
```

Three queries, three full loads of the entire graph. Same data read from Lance three times, same CSR built three times, all thrown away between commands.

The SDK (`nanograph-ts`, `nanograph-ffi`) is different. `JsDatabase` holds `Arc<RwLock<Option<Database>>>`. The `Database` object lives across queries:

```javascript
const db = await Database.open("my.nano");  // loads entire graph

const friends = await db.run("friends_of", { name: "Alice" });  // uses in-memory data
const companies = await db.run("companies_in", { city: "SF" }); // same in-memory data

await db.load(newData, "append");  // rebuilds entire GraphStorage

const updated = await db.run("friends_of", { name: "Alice" });  // uses updated in-memory data
```

When a mutation happens, it rebuilds the `GraphStorage` and swaps it in via `replace_storage()` — the `Arc` swap under `RwLock`. Concurrent readers hold the old `Arc<GraphStorage>` and finish against the pre-mutation snapshot.

"Implicit cache" means: the entire graph loaded at `open()` stays in memory and serves all subsequent queries. Nothing gets evicted, nothing gets lazily loaded. Everything, all at once, for the lifetime of the object.

## Comparison With Other Embedded DBs

**SQLite** doesn't load anything at open. It reads the file header (100 bytes), then reads 4KB pages on demand as queries touch them. A query that reads 10 rows from a 1M-row table reads ~10 pages, not the whole file. The OS page cache keeps recently-read pages warm across process invocations.

**DuckDB** same idea. Opens the file, reads metadata, then reads column chunks on demand during execution. Buffer manager evicts cold pages under memory pressure.

**nanograph** reads the entire database into memory before it can answer any query.

SQLite and DuckDB don't care whether you use a REPL or one-shot commands. A one-shot `sqlite3 db.sqlite "SELECT * FROM t WHERE id = 5"` reads a handful of pages, not the whole database. The CLI lifecycle doesn't matter when the storage layer reads on demand.

## Offload Steps

### Step 1: Stop loading node batches at open

Remove `storage.load_node_batch()` calls from `Database::open()`. Node scans go through the Lance path that already exists in `NodeScanExec::execute()` (line 331). The in-memory fallback path (line 418) becomes dead code for DB mode.

Remove from `GraphStorage`:
- `NodeSegment.batches`
- `NodeSegment.id_to_row`

**What breaks**: `ExpandExec` calls `storage.get_all_nodes(&dst_type)` to resolve neighbor IDs to full rows. Fix: after collecting neighbor IDs from CSR, do a Lance scan with `WHERE id IN (neighbor_id_1, neighbor_id_2, ...)`. Lance's BTree index on `id` makes this efficient.

**Memory saved**: All node property data. For a graph with 1M nodes across 5 types, each with ~100 bytes of properties, that's ~100 MB.

### Step 2: Stop loading edge batches at open

Edge property batches (`EdgeSegment.batches`) are loaded at open but never read by any query operator. `ExpandExec` uses only `csr/csc`. The batches exist only for persistence (`edge_batch_for_save`) and CDC diff.

Remove `storage.load_edge_batch()` calls from `Database::open()` — but still load the `(src, dst, id)` columns needed to build CSR/CSC. This is a projected scan:

```rust
let batches = read_lance_batches_projected(&path, version, &["id", "src", "dst"]).await?;
```

Remove from `GraphStorage`:
- `EdgeSegment.batches`
- `EdgeSegment.src_ids`, `dst_ids`, `edge_ids` (redundant with CSR data)

**Memory saved**: All edge property data. The `(src, dst, id)` triples stay for CSR construction.

### Step 3: Sparse CSR

Replace `CsrIndex.offsets: Vec<u64>` (sized to `max_node_id + 1`) with `AHashMap<u64, (u32, u32)>` (only nodes with edges).

**Memory saved**: For 20 edge types, 1M max_node_id, 50K edges per type: from ~305 MB of dense offsets to ~50K × 20 × 2 × ~50 bytes = ~100 MB of hash map entries. Net win grows as the graph gets sparser.

### Step 4: Lazy CSR per edge type

Don't build CSR/CSC at open. Build on first query that traverses that edge type. Cache for subsequent queries.

```rust
pub async fn open(db_path: &Path) -> Result<Self> {
    let schema_ir = read_schema_ir(db_path)?;
    let catalog = build_catalog_from_ir(&schema_ir)?;
    let manifest = GraphManifest::read(db_path)?;
    // That's it. No data loading.
    Ok(Self::from_parts(db_path, schema_ir, catalog, manifest))
}
```

**Result**: `Database::open()` is O(1). First traversal query pays the CSR build cost for the edge types it touches. Subsequent queries reuse cached indices.

### Step 5: Direct Lance mutations

After steps 1–4, `GraphStorage` holds only cached CSR/CSC indices. Mutations no longer need to clone it. Delete/insert/update go directly to Lance datasets, then invalidate affected CSR caches.

### Summary

| Step | What leaves memory | open() cost | Query cost change |
|------|--------------------|-------------|-------------------|
| Before | nothing | O(total graph) | O(1) scans |
| Step 1 | Node batches + id_to_row | O(edges) | Node scans: Lance. ExpandExec dst resolution: Lance `id IN (...)` |
| Step 2 | Edge property batches | O(edge triples) | No change — edge properties weren't used in queries |
| Step 3 | Dense CSR offsets | O(edge triples) | No change — same lookup semantics |
| Step 4 | Everything at open | O(1) | First traversal per edge type pays O(edges_of_that_type) |
| Step 5 | N/A | O(1) | Mutations: O(changed) instead of O(total) |
